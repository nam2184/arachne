mod aisdk_provider;
mod aisdk_wrappers;
mod openai_compatible;
pub(crate) mod tool_registry;

pub(super) use super::error_parsing;
pub(super) use super::{LlmError, LlmProvider, LlmStream, ToolDispatcherFn};
pub use aisdk_provider::{
    api_key_env as aisdk_api_key_env, docs_url as aisdk_docs_url,
    provider_base_url_env as aisdk_provider_base_url_env,
    provider_model_env as aisdk_provider_model_env,
    supported_provider_names as aisdk_supported_provider_names,
};
pub use aisdk_wrappers::provider_from_config as aisdk_provider_from_config;
pub use aisdk_wrappers::*;
pub(crate) use openai_compatible::OpenAiCompatibleSdkProvider;
