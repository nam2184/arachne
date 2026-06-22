use super::{LlmError, LlmProvider, LlmStream};
use crate::llm::request::LlmRequest;

use super::openai_compatible_http::OpenAiCompatibleHttpProvider;
use super::openai_compatible_sdk::OpenAiCompatibleSdkProvider;

pub enum OpenAiCompatibleBackend {
    Http(OpenAiCompatibleHttpProvider),
    Sdk(OpenAiCompatibleSdkProvider),
}

impl OpenAiCompatibleBackend {
    pub fn new(
        provider_name: &str,
        api_key: Option<String>,
        base_url: Option<String>,
        default_base_url: &str,
        api_key_env: &str,
        supported_models: &[&str],
        use_sdk: bool,
    ) -> Self {
        if use_sdk {
            Self::Sdk(OpenAiCompatibleSdkProvider::new(
                provider_name,
                api_key,
                base_url,
                default_base_url,
                api_key_env,
                supported_models,
            ))
        } else {
            Self::Http(OpenAiCompatibleHttpProvider::new(
                provider_name,
                api_key,
                base_url,
                default_base_url,
                api_key_env,
                supported_models,
            ))
        }
    }

    pub fn provider_name(&self) -> &str {
        match self {
            Self::Http(provider) => provider.provider_name(),
            Self::Sdk(provider) => provider.provider_name(),
        }
    }

    pub fn supported_models(&self) -> Vec<String> {
        match self {
            Self::Http(provider) => provider.supported_models(),
            Self::Sdk(provider) => provider.supported_models(),
        }
    }

    pub fn backend_name(&self) -> &str {
        match self {
            Self::Http(provider) => provider.backend_name(),
            Self::Sdk(provider) => provider.backend_name(),
        }
    }

    pub fn model_base_url(&self) -> Option<&str> {
        match self {
            Self::Http(provider) => provider.model_base_url(),
            Self::Sdk(provider) => provider.model_base_url(),
        }
    }

    pub fn api_key(&self) -> Option<&str> {
        match self {
            Self::Http(provider) => provider.api_key(),
            Self::Sdk(provider) => provider.api_key(),
        }
    }

    pub fn with_base_url(self, url: &str) -> Self {
        match self {
            Self::Http(provider) => Self::Http(provider.with_base_url(url)),
            Self::Sdk(provider) => Self::Sdk(provider.with_base_url(url)),
        }
    }

    pub fn chat_completions_url(&self) -> String {
        match self {
            Self::Http(provider) => provider.chat_completions_url(),
            Self::Sdk(provider) => provider.chat_completions_url(),
        }
    }

    pub async fn endpoint_status(&self, model: &str) -> Result<reqwest::StatusCode, LlmError> {
        match self {
            Self::Http(provider) => provider.endpoint_status(model).await,
            Self::Sdk(provider) => provider.endpoint_status(model).await,
        }
    }

    pub async fn stream(&self, request: LlmRequest) -> Result<LlmStream, LlmError> {
        match self {
            Self::Http(provider) => provider.stream(request).await,
            Self::Sdk(provider) => provider.stream(request).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_backend_delegates_metadata() {
        let backend = OpenAiCompatibleBackend::new(
            "openai",
            Some("test-key".to_string()),
            None,
            "https://api.openai.com/v1",
            "OPENAI_API_KEY",
            &["gpt-4o-mini"],
            false,
        );

        assert_eq!(backend.provider_name(), "openai");
        assert_eq!(backend.model_base_url(), Some("https://api.openai.com/v1"));
        assert_eq!(backend.api_key(), Some("test-key"));
        assert_eq!(backend.supported_models(), vec!["gpt-4o-mini".to_string()]);
    }
}
