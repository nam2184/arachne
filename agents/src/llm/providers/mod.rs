use async_trait::async_trait;
use futures_util::StreamExt;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio_stream::Stream;

use super::events::LlmEvent;
use super::request::{LlmError, LlmMessage, LlmRequest, LlmResponse};

pub mod anthropic;
pub mod minimax_token_plan;
pub mod openai;
mod openai_compatible_chat;

pub use anthropic::AnthropicProvider;
pub use minimax_token_plan::MiniMaxTokenPlanProvider;
pub use openai::OpenAiProvider;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn provider_name(&self) -> &str;
    fn supported_models(&self) -> Vec<String>;

    async fn stream(&self, request: LlmRequest) -> Result<LlmStream, LlmError>;

    async fn complete(&self, request: LlmRequest) -> Result<LlmResponse, LlmError>
    where
        Self: Sized,
    {
        let stream = self.stream(request).await?;
        let events: Vec<LlmEvent> = stream.events.collect().await;
        Ok(LlmResponse::from_events(events))
    }

    fn model_base_url(&self) -> Option<&str>;
    fn api_key(&self) -> Option<&str>;
}

pub struct LlmStream {
    pub events: Pin<Box<dyn Stream<Item = LlmEvent> + Send>>,
    pub abort_tx: Option<Arc<oneshot::Sender<()>>>,
}

impl LlmStream {
    pub fn abort(&self) {
        if let Some(tx) = self.abort_tx.as_ref() {
            if let Ok(sender) = Arc::try_unwrap(tx.clone()) {
                let _ = sender.send(());
            }
        }
    }
}

/// Helper used by provider implementations to extract the JSON
/// payload from an SSE `data: ...` line. Returns `None` if the
/// line is not a `data:` line.
pub fn parse_sse_line(line: &str) -> Option<String> {
    const PREFIX: &str = "data: ";
    line.strip_prefix(PREFIX).map(|s| s.to_string())
}

pub fn to_llm_messages(history: &[(String, String)]) -> Vec<LlmMessage> {
    let mut messages = Vec::new();
    for (role, content) in history {
        match role.as_str() {
            "user" => messages.push(LlmMessage::user(content)),
            "assistant" => messages.push(LlmMessage::assistant(content)),
            "system" => messages.push(LlmMessage::system(content)),
            "tool" => {} // tool results handled separately
            _ => messages.push(LlmMessage::user(content)),
        }
    }
    messages
}

pub fn system_prompt(agent_name: &str, languages: &[String]) -> String {
    format!(
        "You are {}, an AI coding assistant. Languages detected in this project: {}.",
        agent_name,
        languages.join(", ")
    )
}

pub(super) fn log_sse_event_body(provider: &str, model: &str, body: &str) {
    const MAX_SSE_LOG_BYTES: usize = 32 * 1024;

    let body_truncated = body.len() > MAX_SSE_LOG_BYTES;
    let body_display: String = if body_truncated {
        body.chars().take(MAX_SSE_LOG_BYTES).collect()
    } else {
        body.to_string()
    };

    tracing::info!(
        provider = %provider,
        model = %model,
        body_bytes = body.len(),
        body_truncated,
        body = %body_display,
        "llm sse response body"
    );
}
