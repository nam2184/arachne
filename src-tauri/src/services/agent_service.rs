use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use arachne_agents::{
    llm::providers::{
        aisdk_provider_from_config,
        minimax_token_plan::{
            MiniMaxTokenPlanProvider, DEFAULT_BASE_URL as MINIMAX_DEFAULT_BASE_URL,
        },
        sse_proxy::SseProxyManager,
    },
    llm::{ContentPart, SubagentRegistry},
    sandbox::SandboxPolicy,
    tools::SandboxedContext,
    CompactionConfig, CompactionOutcome, CompactionRequest, CompactionService, ConversationService,
    LlmProvider, MessageRole, ModelRegistry, ProviderConfig, ProviderProtocol, ProviderRegistry,
    ProviderService, SessionCancelToken, SessionError, SessionFileDiff, SessionRunEvent,
    SessionRunner, SessionService, SnapshotService,
};
use parking_lot::Mutex;
use tauri::{AppHandle, Emitter};

use crate::services::permission_map::PermissionMap;
use crate::services::settings_service::SettingsService;

const AGENT_EVENT: &str = "agent:event";

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum AgentUiEvent {
    Started {
        session_id: String,
    },
    LlmEvent {
        session_id: String,
        step: u32,
        event: arachne_agents::llm::LlmEvent,
    },
    Finished {
        session_id: String,
        response: String,
    },
    Error {
        session_id: String,
        message: String,
    },
    SessionDiff {
        session_id: String,
        message_id: String,
        diff: Vec<SessionFileDiff>,
    },
    Stopped {
        session_id: String,
    },
}

#[derive(Clone)]
struct ActiveRun {
    run_id: u64,
    cancellation: SessionCancelToken,
}

struct ActiveRunGuard {
    session_id: String,
    run_id: u64,
    active_runs: Arc<Mutex<HashMap<String, ActiveRun>>>,
}

impl Drop for ActiveRunGuard {
    fn drop(&mut self) {
        let mut active_runs = self.active_runs.lock();
        if active_runs
            .get(&self.session_id)
            .is_some_and(|active| active.run_id == self.run_id)
        {
            active_runs.remove(&self.session_id);
        }
    }
}

pub struct AgentService {
    providers: Arc<ProviderRegistry>,
    session_service: Arc<SessionService>,
    conversation_service: Arc<ConversationService>,
    provider_service: Arc<ProviderService>,
    subagent_registry: Arc<SubagentRegistry>,
    permission_map: Arc<PermissionMap>,
    settings_service: Arc<SettingsService>,
    compactor: Arc<CompactionService>,
    sse_proxy: Arc<SseProxyManager>,
    snapshot_service: Arc<SnapshotService>,
    active_runs: Arc<Mutex<HashMap<String, ActiveRun>>>,
    next_run_id: AtomicU64,
}

impl AgentService {
    pub fn new(
        session_service: Arc<SessionService>,
        conversation_service: Arc<ConversationService>,
        provider_service: Arc<ProviderService>,
        subagent_registry: Arc<SubagentRegistry>,
        permission_map: Arc<PermissionMap>,
        settings_service: Arc<SettingsService>,
        snapshot_service: Arc<SnapshotService>,
    ) -> Arc<Self> {
        let providers = Arc::new(ProviderRegistry::new());
        providers.register_defaults_sync();
        let compactor = CompactionService::new(
            Arc::clone(&conversation_service),
            Arc::clone(&providers),
            Arc::new(ModelRegistry::from_embedded_json()),
            CompactionConfig::default(),
        );
        let sse_proxy = Arc::new(SseProxyManager::new());

        Arc::new(Self {
            providers,
            session_service,
            conversation_service,
            provider_service,
            subagent_registry,
            permission_map,
            settings_service,
            compactor,
            sse_proxy,
            snapshot_service,
            active_runs: Arc::new(Mutex::new(HashMap::new())),
            next_run_id: AtomicU64::new(1),
        })
    }

    fn begin_active_run(&self, session_id: &str) -> (SessionCancelToken, ActiveRunGuard) {
        let run_id = self.next_run_id.fetch_add(1, Ordering::Relaxed);
        let cancellation = SessionCancelToken::new();
        self.active_runs.lock().insert(
            session_id.to_string(),
            ActiveRun {
                run_id,
                cancellation: cancellation.clone(),
            },
        );
        (
            cancellation,
            ActiveRunGuard {
                session_id: session_id.to_string(),
                run_id,
                active_runs: Arc::clone(&self.active_runs),
            },
        )
    }

    pub fn send_cancel(&self, session_id: &str) -> bool {
        let cancellation = self
            .active_runs
            .lock()
            .get(session_id)
            .map(|active| active.cancellation.clone());
        if let Some(cancellation) = cancellation {
            cancellation.cancel();
            true
        } else {
            self.subagent_registry.cancel_session(session_id)
        }
    }

    pub fn sse_proxy(&self) -> Arc<SseProxyManager> {
        Arc::clone(&self.sse_proxy)
    }

    pub fn compactor(&self) -> &Arc<CompactionService> {
        &self.compactor
    }

    /// Build a `SessionRunner` wired with the auto-compactor so the
    /// runner can transparently compact + retry when the request
    /// would exceed the model context window.
    pub fn build_runner(&self) -> SessionRunner {
        SessionRunner::new(
            Arc::clone(&self.session_service),
            Arc::clone(&self.conversation_service),
            Arc::clone(&self.providers),
        )
        .with_compactor(Arc::clone(&self.compactor))
        .with_runtime_config(self.settings_service.runtime_config())
    }

    /// Build a production runner for a concrete session. Tool execution is
    /// rooted at `directory` and enforced by the v2 sandbox path.
    pub fn build_runner_for_session(&self, session_id: &str, directory: &str) -> SessionRunner {
        let runtime_config = self.runtime_config_for_session(session_id, directory);
        let permissions = self
            .permission_map
            .get_or_create_with_runtime_config(session_id, &runtime_config);
        let mut sandbox = SandboxPolicy::new(std::path::PathBuf::from(directory));
        for root in permissions.external_roots() {
            sandbox = sandbox.with_external(root);
        }
        let sandboxed_ctx = Arc::new(
            SandboxedContext::new(sandbox, Arc::clone(&permissions))
                .with_runtime_config(runtime_config.clone())
                .with_caller_session(session_id.to_string(), Arc::clone(&self.session_service)),
        );

        self.build_runner()
            .with_runtime_config(runtime_config)
            .with_permissions(permissions)
            .with_sandboxed_context(sandboxed_ctx)
            .with_snapshot_base_dir(self.snapshot_service.base_dir().to_path_buf())
    }

    fn runtime_config_for_session(
        &self,
        session_id: &str,
        directory: &str,
    ) -> arachne_agents::RuntimeConfig {
        let mut runtime_config = arachne_agents::RuntimeConfig::default();
        tracing::info!(
            session_id,
            directory,
            precedence = ?arachne_agents::CONFIG_PRECEDENCE,
            summary = ?runtime_config.trace_summary(),
            "runtime config resolution started"
        );

        merge_runtime_config_layer(
            &mut runtime_config,
            session_id,
            directory,
            "global/user config.json",
            &arachne_agents::paths::config_file(),
        );
        merge_runtime_config_layer(
            &mut runtime_config,
            session_id,
            directory,
            "project .arachne/config.json",
            &arachne_agents::paths::project_config_file(directory),
        );

        let settings_config = self.settings_service.runtime_config();
        tracing::info!(
            session_id,
            directory,
            layer = "app settings UI overrides",
            path = %self.settings_service.config_path().display(),
            decision = "loaded_merge",
            summary = ?settings_config.trace_summary(),
            "runtime config layer decision"
        );
        runtime_config.merge(settings_config);
        tracing::info!(
            session_id,
            directory,
            summary = ?runtime_config.trace_summary(),
            "runtime config resolution finished"
        );

        runtime_config
    }

    pub async fn compact_now(&self, session_id: &str) -> Result<CompactionOutcome, String> {
        let session = self
            .session_service
            .get_session(session_id)?
            .ok_or_else(|| format!("session not found: {session_id}"))?;
        self.refresh_provider(&session.provider).await;
        Ok(self
            .compactor
            .compact_now(CompactionRequest {
                session_id: session_id.to_string(),
                provider: session.provider,
                model: session.model,
            })
            .await)
    }

    pub fn subagent_registry(&self) -> &Arc<SubagentRegistry> {
        &self.subagent_registry
    }

    pub fn providers(&self) -> &Arc<ProviderRegistry> {
        &self.providers
    }

    pub async fn refresh_provider(&self, name: &str) {
        let mut config = match self.provider_service.get_config(name) {
            Some(c) if c.enabled => c,
            _ => return,
        };

        // Opt-in routing through the in-process SSE proxy. The proxy
        // forwards bytes unchanged, but records raw SSE terminal data
        // for unstable providers such as MiniMax.
        if SseProxyManager::env_enabled(&config.name) {
            let upstream = config
                .base_url
                .clone()
                .unwrap_or_else(|| default_upstream_base_url(&config.name).to_string());
            match self.sse_proxy.ensure_for_provider(&config.name, &upstream) {
                Ok((local_base_url, _instance)) => {
                    tracing::info!(
                        provider = %config.name,
                        upstream_base_url = %upstream,
                        local_base_url = %local_base_url,
                        "routing provider through sse_proxy"
                    );
                    config.base_url = Some(local_base_url);
                }
                Err(error) => {
                    tracing::warn!(
                        provider = %config.name,
                        error = %error,
                        "failed to start sse_proxy; falling back to direct upstream"
                    );
                }
            }
        }

        let provider: Arc<dyn LlmProvider> = match &config.protocol {
            ProviderProtocol::OpenAI if config.name == "minimax" => {
                Arc::new(MiniMaxTokenPlanProvider::from_config(&config))
            }
            _ => match aisdk_provider_from_config(&config) {
                Some(provider) => provider,
                None => return,
            },
        };

        self.providers.register(provider).await;
    }

    pub async fn update_session_provider(
        &self,
        session_id: &str,
        provider: String,
        model: String,
    ) -> Result<(), String> {
        self.session_service
            .update_session_provider(session_id, &provider, &model)
    }

    pub async fn send_message(
        &self,
        session_id: &str,
        message: String,
        mode: arachne_agents::permission::PermissionMode,
        app: AppHandle,
    ) -> Result<String, String> {
        emit_agent_event(
            &app,
            AgentUiEvent::Started {
                session_id: session_id.to_string(),
            },
        );

        self.conversation_service
            .append_message(session_id, MessageRole::User, message)?;

        let session = self
            .session_service
            .get_session(session_id)?
            .ok_or_else(|| "Session not found".to_string())?;

        let (cancellation, _active_run) = self.begin_active_run(session_id);

        self.refresh_provider(&session.provider).await;

        let session_id_clone = session_id.to_string();
        let registry = Arc::clone(&self.subagent_registry);
        let app_for_events = app.clone();
        let event_sink = Arc::new(move |event: SessionRunEvent| match event {
            SessionRunEvent::Llm {
                session_id,
                step,
                event,
            } => emit_agent_event(
                &app_for_events,
                AgentUiEvent::LlmEvent {
                    session_id,
                    step,
                    event,
                },
            ),
            SessionRunEvent::SessionDiff {
                session_id,
                message_id,
                diff,
                ..
            } => emit_agent_event(
                &app_for_events,
                AgentUiEvent::SessionDiff {
                    session_id,
                    message_id,
                    diff,
                },
            ),
        });

        let runner = self
            .build_runner_for_session(session_id, &session.directory)
            .with_event_sink(event_sink)
            .with_subagent_registry(Arc::clone(&registry))
            .with_cancellation(cancellation)
            .with_mode(mode);
        let run_result = runner.run(&session_id_clone).await;

        let run_result = match run_result {
            Ok(result) => result,
            Err(error) => {
                let message = chat_error_message(&error);
                if let Err(append_error) =
                    append_assistant_error(&self.conversation_service, session_id, &message)
                {
                    tracing::warn!("failed to append LLM error to chat: {}", append_error);
                }
                emit_agent_event(
                    &app,
                    AgentUiEvent::Error {
                        session_id: session_id.to_string(),
                        message: message.clone(),
                    },
                );
                return Err(message);
            }
        };

        if run_result.stopped {
            emit_agent_event(
                &app,
                AgentUiEvent::Stopped {
                    session_id: session_id.to_string(),
                },
            );
            return Ok(String::new());
        }

        let messages = self.conversation_service.get_messages(session_id)?;
        let response = messages
            .iter()
            .rev()
            .find(|m| m.role == "assistant")
            .map(|m| m.content.clone())
            .unwrap_or_default();

        emit_agent_event(
            &app,
            AgentUiEvent::Finished {
                session_id: session_id.to_string(),
                response: response.clone(),
            },
        );

        Ok(response)
    }
}

fn merge_runtime_config_layer(
    runtime_config: &mut arachne_agents::RuntimeConfig,
    session_id: &str,
    directory: &str,
    layer: &str,
    path: &std::path::Path,
) {
    if !path.exists() {
        tracing::info!(
            session_id,
            directory,
            layer,
            path = %path.display(),
            decision = "skipped_missing",
            "runtime config layer decision"
        );
        return;
    }

    match arachne_agents::RuntimeConfig::load(path) {
        Ok(layer_config) => {
            tracing::info!(
                session_id,
                directory,
                layer,
                path = %path.display(),
                decision = "loaded_merge",
                summary = ?layer_config.trace_summary(),
                "runtime config layer decision"
            );
            runtime_config.merge(layer_config);
            tracing::debug!(
                session_id,
                directory,
                layer,
                path = %path.display(),
                merged_summary = ?runtime_config.trace_summary(),
                "runtime config layer merged"
            );
        }
        Err(error) => {
            tracing::warn!(
                session_id,
                directory,
                layer,
                path = %path.display(),
                decision = "failed_ignored",
                error = %error,
                "runtime config layer decision"
            );
        }
    }
}

fn emit_agent_event(app: &AppHandle, event: AgentUiEvent) {
    if let Err(error) = app.emit(AGENT_EVENT, event) {
        tracing::warn!("failed to emit agent event: {}", error);
    }
}

fn default_upstream_base_url(provider: &str) -> &'static str {
    if provider.eq_ignore_ascii_case("minimax") {
        return MINIMAX_DEFAULT_BASE_URL;
    }
    "https://api.openai.com/v1"
}

fn chat_error_message(error: &SessionError) -> String {
    match error {
        SessionError::Llm(err) => format!("LLM error: {}", err),
        SessionError::Provider(message) => format!("LLM provider error: {}", message),
        SessionError::NoProviderForSession => {
            "LLM error: no provider is configured for this session.".to_string()
        }
        SessionError::StepLimitExceeded { limit, .. } => {
            format!("LLM error: stopped after reaching the {limit}-step limit.")
        }
        _ => error.to_string(),
    }
}

fn append_assistant_error(
    conversation_service: &ConversationService,
    session_id: &str,
    message: &str,
) -> Result<(), String> {
    let content = serde_json::to_string(&vec![ContentPart::text(message)])
        .unwrap_or_else(|_| message.to_string());
    conversation_service
        .append_ui_message(session_id, MessageRole::Assistant, content)
        .map(|_| ())
}
