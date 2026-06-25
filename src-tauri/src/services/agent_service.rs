use std::sync::Arc;

use arachne_agents::{
    llm::providers::{aisdk_provider_from_config, MiniMaxTokenPlanProvider},
    llm::{ContentPart, SubagentRegistry},
    sandbox::SandboxPolicy,
    tools::SandboxedContext,
    CompactionConfig, CompactionOutcome, CompactionRequest, CompactionService, ConversationService,
    LlmProvider, MessageRole, ModelRegistry, ProviderProtocol, ProviderRegistry, ProviderService,
    SessionError, SessionRunEvent, SessionRunner, SessionService,
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
}

pub struct AgentService {
    providers: Arc<ProviderRegistry>,
    session_service: Arc<SessionService>,
    conversation_service: Arc<ConversationService>,
    provider_service: Arc<ProviderService>,
    subagent_registry: Arc<SubagentRegistry>,
    permission_map: Arc<PermissionMap>,
    compactor: Arc<CompactionService>,
}

impl AgentService {
    pub fn new(
        session_service: Arc<SessionService>,
        conversation_service: Arc<ConversationService>,
        provider_service: Arc<ProviderService>,
        subagent_registry: Arc<SubagentRegistry>,
        permission_map: Arc<PermissionMap>,
    ) -> Arc<Self> {
        let providers = Arc::new(ProviderRegistry::new());
        providers.register_defaults_sync();
        let compactor = CompactionService::new(
            Arc::clone(&conversation_service),
            Arc::clone(&providers),
            Arc::new(ModelRegistry::from_embedded_json()),
            CompactionConfig::default(),
        );

        Arc::new(Self {
            providers,
            session_service,
            conversation_service,
            provider_service,
            subagent_registry,
            permission_map,
            compactor,
        })
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
        let sandbox = SandboxPolicy::new(std::path::PathBuf::from(directory));
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
        let config = match self.provider_service.get_config(name) {
            Some(c) if c.enabled => c,
            _ => return,
        };

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

        self.refresh_provider(&session.provider).await;

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
}

fn emit_agent_event(app: &AppHandle, event: AgentUiEvent) {
    if let Err(error) = app.emit(AGENT_EVENT, event) {
        tracing::warn!("failed to emit agent event: {}", error);
    }
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
