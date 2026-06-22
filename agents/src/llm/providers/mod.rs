use async_trait::async_trait;
use futures_util::StreamExt;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio_stream::Stream;

use super::events::LlmEvent;
use super::request::{LlmError, LlmRequest, LlmResponse};

mod aisdk_provider;
mod aisdk_wrappers;
pub mod minimax_token_plan;
mod openai_compatible_backend;
mod openai_compatible_http;
mod openai_compatible_sdk;

pub use aisdk_provider::{
    api_key_env as aisdk_api_key_env, docs_url as aisdk_docs_url,
    provider_base_url_env as aisdk_provider_base_url_env,
    provider_model_env as aisdk_provider_model_env,
    supported_provider_names as aisdk_supported_provider_names,
};
pub use aisdk_wrappers::provider_from_config as aisdk_provider_from_config;
pub use aisdk_wrappers::*;
pub use minimax_token_plan::MiniMaxTokenPlanProvider;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn provider_name(&self) -> &str;
    fn supported_models(&self) -> Vec<String>;

    fn backend_name(&self) -> &str {
        "unknown"
    }

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

pub(super) fn openai_compatible_endpoint_url(base_url: &str, path: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    if base_url.ends_with(&format!("/{path}")) {
        base_url.to_string()
    } else {
        format!("{base_url}/{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_url_appends_chat_completions_to_base_api_url() {
        assert_eq!(
            openai_compatible_endpoint_url("https://api.minimax.io/v1", "chat/completions"),
            "https://api.minimax.io/v1/chat/completions"
        );
    }

    #[test]
    fn endpoint_url_does_not_double_append_chat_completions() {
        assert_eq!(
            openai_compatible_endpoint_url(
                "https://api.minimax.io/v1/chat/completions/",
                "chat/completions",
            ),
            "https://api.minimax.io/v1/chat/completions"
        );
    }
}
