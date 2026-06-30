use std::sync::Arc;

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
    ProviderService, SessionError, SessionFileDiff, SessionRunEvent, SessionRunner, SessionService,
    SnapshotService,
};
use tauri::{AppHandle, Emitter};

use crate::services::permission_map::PermissionMap;

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
}

pub struct AgentService {
    providers: Arc<ProviderRegistry>,
    session_service: Arc<SessionService>,
    conversation_service: Arc<ConversationService>,
    provider_service: Arc<ProviderService>,
    subagent_registry: Arc<SubagentRegistry>,
    permission_map: Arc<PermissionMap>,
    compactor: Arc<CompactionService>,
    sse_proxy: Arc<SseProxyManager>,
    snapshot_service: Arc<SnapshotService>,
}

impl AgentService {
    pub fn new(
        session_service: Arc<SessionService>,
        conversation_service: Arc<ConversationService>,
        provider_service: Arc<ProviderService>,
        subagent_registry: Arc<SubagentRegistry>,
        permission_map: Arc<PermissionMap>,
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
            compactor,
            sse_proxy,
            snapshot_service,
        })
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
    }

    /// Build a production runner for a concrete session. Tool execution is
    /// rooted at `directory` and enforced by the v2 sandbox path.
    pub fn build_runner_for_session(&self, session_id: &str, directory: &str) -> SessionRunner {
        let permissions = self.permission_map.get_or_create(session_id);
        let mut sandbox = SandboxPolicy::new(std::path::PathBuf::from(directory));
        for root in permissions.external_roots() {
            sandbox = sandbox.with_external(root);
        }
        let sandboxed_ctx = Arc::new(
            SandboxedContext::new(sandbox, Arc::clone(&permissions))
                .with_caller_session(session_id.to_string(), Arc::clone(&self.session_service)),
        );

        self.build_runner()
            .with_permissions(permissions)
            .with_sandboxed_context(sandboxed_ctx)
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

        let user_message_id =
            self.conversation_service
                .append_message(session_id, MessageRole::User, message)?;

        let session = self
            .session_service
            .get_session(session_id)?
            .ok_or_else(|| "Session not found".to_string())?;

        self.refresh_provider(&session.provider).await;
        let before_snapshot = self.snapshot_service.track(&session);

        let session_id_clone = session_id.to_string();
        let registry = Arc::clone(&self.subagent_registry);
        let app_for_events = app.clone();
        let event_sink = Arc::new(move |event: SessionRunEvent| {
            emit_agent_event(
                &app_for_events,
                AgentUiEvent::LlmEvent {
                    session_id: event.session_id,
                    step: event.step,
                    event: event.event,
                },
            );
        });

        let runner = self
            .build_runner_for_session(session_id, &session.directory)
            .with_event_sink(event_sink)
            .with_subagent_registry(Arc::clone(&registry))
            .with_mode(mode);
        let run_result = runner.run(&session_id_clone).await;
        self.capture_session_diff(&app, &session, &user_message_id, before_snapshot.as_deref());

        if let Err(error) = run_result {
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

    fn capture_session_diff(
        &self,
        app: &AppHandle,
        session: &arachne_agents::AgentSession,
        message_id: &str,
        before_snapshot: Option<&str>,
    ) {
        let Some(before_snapshot) = before_snapshot else {
            return;
        };
        let Some(after_snapshot) = self.snapshot_service.track(session) else {
            return;
        };
        let diff = self
            .snapshot_service
            .diff_full(session, before_snapshot, &after_snapshot);
        if let Err(error) =
            self.conversation_service
                .write_session_diff(&session.id, message_id, diff.clone())
        {
            tracing::warn!(session_id = %session.id, error = %error, "failed to persist session diff");
            return;
        }
        emit_agent_event(
            app,
            AgentUiEvent::SessionDiff {
                session_id: session.id.clone(),
                message_id: message_id.to_string(),
                diff,
            },
        );
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
