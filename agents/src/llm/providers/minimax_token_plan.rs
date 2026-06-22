use async_trait::async_trait;

use super::openai_compatible_backend::OpenAiCompatibleBackend;
use super::{LlmError, LlmProvider, LlmStream};
use crate::llm::request::LlmRequest;
use crate::ProviderConfig;

pub const DEFAULT_BASE_URL: &str = "https://api.minimax.io/v1";
pub const DEFAULT_MODEL: &str = "MiniMax-M3";

pub struct MiniMaxTokenPlanProvider {
    inner: OpenAiCompatibleBackend,
}

impl MiniMaxTokenPlanProvider {
    pub fn new(api_key: Option<String>, base_url: Option<String>, use_sdk: bool) -> Self {
        let api_key = api_key
            .or_else(|| std::env::var("MINIMAX_TOKEN_PLAN_KEY").ok())
            .or_else(|| std::env::var("MINIMAX_API_KEY").ok());

        Self {
            inner: OpenAiCompatibleBackend::new(
                "minimax",
                api_key,
                base_url,
                DEFAULT_BASE_URL,
                "MINIMAX_TOKEN_PLAN_KEY",
                &["MiniMax-M3", "MiniMax-M2.7", "MiniMax-M2.5"],
                use_sdk,
            ),
        }
    }

    pub fn from_config(config: &ProviderConfig) -> Self {
        Self::new(config.api_key.clone(), config.base_url.clone(), true)
    }

    pub fn with_base_url(mut self, url: &str) -> Self {
        self.inner = self.inner.with_base_url(url);
        self
    }

    pub fn chat_completions_url(&self) -> String {
        self.inner.chat_completions_url()
    }

    pub async fn endpoint_status(&self, model: &str) -> Result<reqwest::StatusCode, LlmError> {
        self.inner.endpoint_status(model).await
    }
}

#[async_trait]
impl LlmProvider for MiniMaxTokenPlanProvider {
    fn provider_name(&self) -> &str {
        self.inner.provider_name()
    }

    fn supported_models(&self) -> Vec<String> {
        self.inner.supported_models()
    }

    fn backend_name(&self) -> &str {
        self.inner.backend_name()
    }

    async fn stream(&self, request: LlmRequest) -> Result<LlmStream, LlmError> {
        self.inner.stream(request).await
    }

    fn model_base_url(&self) -> Option<&str> {
        self.inner.model_base_url()
    }

    fn api_key(&self) -> Option<&str> {
        self.inner.api_key()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::events::LlmEvent;
    use crate::llm::request::LlmMessage;
    use crate::ProviderProtocol;
    use futures_util::StreamExt;

    #[test]
    fn config_uses_openai_protocol_endpoint() {
        let config = ProviderConfig::new(
            "minimax".to_string(),
            DEFAULT_MODEL.to_string(),
            ProviderProtocol::OpenAI,
        );
        let provider = MiniMaxTokenPlanProvider::from_config(&config);

        assert_eq!(config.protocol, ProviderProtocol::OpenAI);
        assert_eq!(provider.model_base_url(), Some(DEFAULT_BASE_URL));
        assert_eq!(
            provider.chat_completions_url(),
            "https://api.minimax.io/v1/chat/completions"
        );
    }

    #[tokio::test]
    async fn minimax_endpoint_returns_http_status() {
        let provider =
            MiniMaxTokenPlanProvider::new(Some("invalid-token-plan-key".to_string()), None, true);
        let err = provider.endpoint_status(DEFAULT_MODEL).await.unwrap_err();

        assert_eq!(err.code, "unsupported");
    }

    #[tokio::test]
    async fn stream_produces_events_or_http_error() {
        let provider =
            MiniMaxTokenPlanProvider::new(Some("invalid-token-plan-key".to_string()), None, true);
        let request =
            LlmRequest::new(DEFAULT_MODEL, "minimax").with_message(LlmMessage::user("say hello"));

        let result = provider.stream(request).await;

        if let Ok(stream) = result {
            let events: Vec<_> = stream.events.collect().await;
            assert!(
                !events.is_empty(),
                "stream should produce at least one event"
            );
            let has_text_or_error = events.iter().any(|e| {
                matches!(e, LlmEvent::TextDelta { .. })
                    || matches!(e, LlmEvent::ProviderError { .. })
                    || matches!(e, LlmEvent::Finish { .. })
            });
            assert!(
                has_text_or_error,
                "stream should contain text, error, or finish event: {events:?}"
            );
        } else {
            let err = match result {
                Err(e) => e,
                _ => unreachable!("expected error"),
            };
            assert!(
                err.code.starts_with("http_") || err.code.starts_with("sdk_"),
                "should be an HTTP or SDK error, got: {} - {}",
                err.code,
                err.message
            );
        }
    }
}
