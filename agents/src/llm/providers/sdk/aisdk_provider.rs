use async_trait::async_trait;
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::Arc;

use aisdk::core::capabilities::{TextInputSupport, ToolCallSupport};
use aisdk::core::language_model::{LanguageModelResponseContentType, Step, StopReason};
use aisdk::core::tools::ToolExecute;
use aisdk::core::{
    AssistantMessage, DynamicModel, LanguageModel, LanguageModelRequest,
    LanguageModelStreamChunkType, Message, Tool, ToolCallInfo, ToolResultInfo, UserMessage,
};

use super::error_parsing::{format_provider_error, parse_provider_error_body};
use super::{LlmError, LlmProvider, LlmStream};
use crate::llm::events::{FinishReason, LlmEvent, ToolDefinition};
use crate::llm::request::{ContentPart, LlmRequest};
use crate::ProviderConfig;

type ModelBuilder<M> = Arc<dyn Fn(&str) -> Result<M, LlmError> + Send + Sync>;

pub struct AisdkLanguageModelProvider<M>
where
    M: LanguageModel + TextInputSupport + ToolCallSupport,
{
    provider_name: String,
    sdk_provider_name: String,
    api_key_env: Option<String>,
    api_key: Option<String>,
    api_key_source: &'static str,
    base_url: Option<String>,
    supported_models: Vec<String>,
    sdk_model_name: String,
    sdk_model: Result<M, LlmError>,
    model_builder: ModelBuilder<M>,
}

impl<M> AisdkLanguageModelProvider<M>
where
    M: LanguageModel + TextInputSupport + ToolCallSupport,
{
    pub fn new(
        provider_name: impl Into<String>,
        sdk_provider_name: impl Into<String>,
        api_key_env: Option<&str>,
        api_key: Option<String>,
        api_key_source: &'static str,
        base_url: Option<String>,
        model: impl Into<String>,
        model_builder: impl Fn(&str) -> Result<M, LlmError> + Send + Sync + 'static,
    ) -> Self {
        let provider_name = provider_name.into();
        let api_key_env = api_key_env.map(str::to_string);
        let api_key = api_key.or_else(|| {
            api_key_env
                .as_deref()
                .and_then(|env| std::env::var(env).ok())
        });
        let sdk_model_name = model.into();
        let model_builder: ModelBuilder<M> = Arc::new(model_builder);
        let sdk_model = model_builder(&sdk_model_name);

        Self {
            provider_name,
            sdk_provider_name: sdk_provider_name.into(),
            api_key_env,
            api_key,
            api_key_source,
            base_url,
            supported_models: vec![sdk_model_name.clone()],
            sdk_model_name,
            sdk_model,
            model_builder,
        }
    }

    fn auth_error(&self) -> LlmError {
        let message = self
            .api_key_env
            .as_deref()
            .map(|env| format!("{env} not set"))
            .unwrap_or_else(|| "provider API key not set".to_string());
        LlmError::new("auth", &message).provider(&self.provider_name)
    }

    fn model_for_request(&self, model: &str) -> Result<M, LlmError> {
        if model == self.sdk_model_name {
            return self.sdk_model.clone();
        }

        (self.model_builder)(model)
    }
}

#[async_trait]
impl<M> LlmProvider for AisdkLanguageModelProvider<M>
where
    M: LanguageModel + TextInputSupport + ToolCallSupport,
{
    fn provider_name(&self) -> &str {
        &self.provider_name
    }

    fn supported_models(&self) -> Vec<String> {
        self.supported_models.clone()
    }

    fn backend_name(&self) -> &str {
        "sdk"
    }

    async fn stream(&self, request: LlmRequest) -> Result<LlmStream, LlmError> {
        self.api_key.as_ref().ok_or_else(|| self.auth_error())?;
        let model = self.model_for_request(&request.model)?;
        let system = sdk_system_prompt(&request);
        let messages = sdk_messages(&request);
        log_aisdk_request_shape(
            &self.provider_name,
            &self.sdk_provider_name,
            &request,
            &system,
            &messages,
        );
        let mut builder = if system.is_empty() {
            LanguageModelRequest::<M>::builder()
                .model(model)
                .messages(messages)
        } else {
            LanguageModelRequest::<M>::builder()
                .model(model)
                .system(system)
                .messages(messages)
        };

        if let Some(temperature) = request.temperature {
            builder = builder.temperature(percent_u32(temperature));
        }
        if let Some(top_p) = request.top_p {
            builder = builder.top_p(percent_u32(top_p));
        }
        if let Some(max_tokens) = request.max_tokens {
            builder.max_output_tokens = Some(max_tokens);
        }
        if let Some(stop) = &request.stop {
            builder = builder.stop_sequences(stop.clone());
        }
        if !request.tools.is_empty() {
            for tool in &request.tools {
                builder = builder.with_tool(sdk_tool(tool)?);
            }
            builder = builder.stop_when(|options| {
                options
                    .last_step()
                    .is_some_and(|step| sdk_generated_step_has_tool_calls(&step))
            });
        }

        let mut sdk_request = builder.build();
        let credential = self.api_key.as_deref().map(credential_debug);
        tracing::debug!(
            provider = %self.provider_name,
            sdk_provider = %self.sdk_provider_name,
            model = %request.model,
            backend = "aisdk",
            api_key_source = self.api_key_source,
            has_api_key = self.api_key.as_deref().is_some_and(|key| !key.trim().is_empty()),
            access_token_preview = credential.as_ref().map(|token| token.preview.as_str()).unwrap_or("none"),
            access_token_sha256 = credential.as_ref().map(|token| token.sha256.as_str()).unwrap_or("none"),
            access_token_len = credential.as_ref().map(|token| token.len).unwrap_or(0),
            "native AISDK provider request starting stream_text"
        );
        let response = sdk_request.stream_text().await.map_err(|error| {
            let raw = error.to_string();
            let info = parse_provider_error_body(&raw);
            let user_message = format_provider_error(&info, &raw);
            tracing::error!(
                provider = %self.provider_name,
                sdk_provider = %self.sdk_provider_name,
                model = %request.model,
                error_kind = %info.kind,
                error_type = info.error_type.as_deref().unwrap_or(""),
                error_code = info.error_code.as_deref().unwrap_or(""),
                error_param = info.error_param.as_deref().unwrap_or(""),
                error_message = %info.message,
                raw_error = %raw,
                "sdk_request failed: provider rejected the request"
            );
            LlmError::new("sdk_request", &user_message)
                .provider(&self.provider_name)
                .model(&request.model)
        })?;

        let stream_provider = self.provider_name.clone();
        let stream_model = request.model.clone();
        let stream = async_stream::stream! {
            let mut response = response;
            let mut saw_provider_error = false;
            let mut saw_end_chunk = false;
            let mut text_chunks = 0_u64;
            let mut text_bytes = 0_usize;

            while let Some(chunk) = response.stream.next().await {
                match chunk {
                    LanguageModelStreamChunkType::Start => {
                        tracing::debug!(
                            provider = %stream_provider,
                            model = %stream_model,
                            "sdk llm stream chunk: start"
                        );
                    }
                    LanguageModelStreamChunkType::Text(text) => {
                        if !text.is_empty() {
                            text_chunks += 1;
                            text_bytes += text.len();
                            tracing::trace!(
                                provider = %stream_provider,
                                model = %stream_model,
                                chunk_bytes = text.len(),
                                text_chunks,
                                text_bytes,
                                "sdk llm stream chunk: text"
                            );
                            yield LlmEvent::TextDelta { id: "text".to_string(), text };
                        }
                    }
                    LanguageModelStreamChunkType::Reasoning(text) => {
                        if !text.is_empty() {
                            yield LlmEvent::ReasoningDelta { id: "reasoning".to_string(), text };
                        }
                    }
                    LanguageModelStreamChunkType::ToolCall(_) => {}
                    LanguageModelStreamChunkType::End(_) => {
                        saw_end_chunk = true;
                        tracing::info!(
                            provider = %stream_provider,
                            model = %stream_model,
                            text_chunks,
                            text_bytes,
                            "sdk llm stream chunk: end"
                        );
                    }
                    LanguageModelStreamChunkType::Failed(message) => {
                        saw_provider_error = true;
                        let info = parse_provider_error_body(&message);
                        let formatted = format_provider_error(&info, &message);
                        tracing::warn!(
                            provider = %stream_provider,
                            model = %stream_model,
                            error = %formatted,
                            raw_error = %message,
                            error_kind = %info.kind,
                            error_type = info.error_type.as_deref().unwrap_or(""),
                            error_code = info.error_code.as_deref().unwrap_or(""),
                            error_param = info.error_param.as_deref().unwrap_or(""),
                            "sdk llm stream chunk: failed_or_not_supported"
                        );
                        yield LlmEvent::ProviderError { message: formatted };
                    }
                    LanguageModelStreamChunkType::NotSupported(message) => {
                        if is_benign_openai_responses_not_supported(&message) {
                            tracing::debug!(
                                provider = %stream_provider,
                                model = %stream_model,
                                raw_event = %message,
                                "sdk llm stream chunk: ignored benign not_supported event"
                            );
                        } else {
                            saw_provider_error = true;
                            let info = parse_provider_error_body(&message);
                            let formatted = format_provider_error(&info, &message);
                            tracing::warn!(
                                provider = %stream_provider,
                                model = %stream_model,
                                error = %formatted,
                                raw_error = %message,
                                error_kind = %info.kind,
                                error_type = info.error_type.as_deref().unwrap_or(""),
                                error_code = info.error_code.as_deref().unwrap_or(""),
                                error_param = info.error_param.as_deref().unwrap_or(""),
                                "sdk llm stream chunk: failed_or_not_supported"
                            );
                            yield LlmEvent::ProviderError { message: formatted };
                        }
                    }
                    LanguageModelStreamChunkType::Incomplete(message) => {
                        tracing::warn!(
                            provider = %stream_provider,
                            model = %stream_model,
                            message = %message,
                            stopped_by_hook = message == "Stopped by hook",
                            "sdk llm stream chunk: incomplete"
                        );
                        if message != "Stopped by hook" {
                            saw_provider_error = true;
                            yield LlmEvent::ProviderError { message };
                        }
                    }
                }
            }

            tracing::info!(
                provider = %stream_provider,
                model = %stream_model,
                saw_end_chunk,
                saw_provider_error,
                text_chunks,
                text_bytes,
                "sdk llm stream exhausted"
            );

            let tool_calls = sdk_generated_step_tool_calls(response.last_step().await);
            for call in &tool_calls {
                for event in sdk_tool_call_events(call) {
                    yield event;
                }
            }

            let usage = Some(sdk_usage(response.usage().await));
            let raw_stop_reason = response.stop_reason().await;
            let raw_stop_reason_debug = format!("{raw_stop_reason:?}");
            let reason = if !tool_calls.is_empty() {
                FinishReason::ToolCalls
            } else if saw_provider_error {
                FinishReason::Error
            } else {
                sdk_finish_reason(raw_stop_reason)
            };

            tracing::info!(
                provider = %stream_provider,
                model = %stream_model,
                stream_exhausted = true,
                reason = %reason,
                raw_stop_reason = %raw_stop_reason_debug,
                saw_end_chunk,
                saw_provider_error,
                text_chunks,
                text_bytes,
                tool_calls = tool_calls.len(),
                "sdk llm stream finished after stream exhaustion"
            );

            yield LlmEvent::Finish { reason, usage };
        };

        Ok(LlmStream {
            events: Box::pin(stream),
            abort_tx: None,
        })
    }

    fn model_base_url(&self) -> Option<&str> {
        self.base_url.as_deref()
    }

    fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }
}

struct CredentialDebug {
    preview: String,
    sha256: String,
    len: usize,
}

fn credential_debug(value: &str) -> CredentialDebug {
    let prefix: String = value.chars().take(8).collect();
    let suffix_chars = value.chars().rev().take(4).collect::<Vec<_>>();
    let suffix = suffix_chars.into_iter().rev().collect::<String>();
    CredentialDebug {
        preview: format!("{prefix}...{suffix}"),
        sha256: format!("{:x}", Sha256::digest(value.as_bytes())),
        len: value.len(),
    }
}

fn is_benign_openai_responses_not_supported(message: &str) -> bool {
    let Some(payload) = aisdk_not_supported_payload(message) else {
        return false;
    };
    let trimmed = payload.trim();
    if matches!(trimmed, "{}" | "[END]") {
        return true;
    }

    let value: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(value) => value,
        Err(_) => return false,
    };
    let Some(event_type) = value.get("type").and_then(serde_json::Value::as_str) else {
        return false;
    };

    matches!(
        event_type,
        "response.created"
            | "response.in_progress"
            | "response.queued"
            | "response.output_item.added"
            | "response.output_item.done"
            | "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.done"
    )
}

fn aisdk_not_supported_payload(message: &str) -> Option<String> {
    let trimmed = message.trim();
    if let Some(inner) = trimmed
        .strip_prefix("NotSupported(")
        .and_then(|value| value.strip_suffix(')'))
    {
        return serde_json::from_str::<String>(inner).ok();
    }
    Some(trimmed.to_string())
}

macro_rules! sdk_provider_case {
    ($name:expr, $ty:ty, $provider_name:expr, $api_key_env:expr, $api_key:expr, $base_url:expr, $model:expr) => {{
        let provider_name = $provider_name.to_string();
        let api_key_env = $api_key_env;
        let config_api_key = $api_key;
        let env_api_key = std::env::var(api_key_env).ok();
        let api_key_source = match config_api_key.as_deref() {
            Some(key) if !key.trim().is_empty() => "config",
            Some(_) => "config-empty",
            None if env_api_key
                .as_deref()
                .is_some_and(|key| !key.trim().is_empty()) =>
            {
                "env"
            }
            None => "none",
        };
        let resolved_api_key = config_api_key.or(env_api_key);
        let base_url = $base_url;
        let builder_provider = provider_name.clone();
        let builder_key = resolved_api_key.clone();
        let builder_base_url = base_url.clone();
        let provider = AisdkLanguageModelProvider::<$ty>::new(
            provider_name,
            $name,
            Some(api_key_env),
            resolved_api_key,
            api_key_source,
            base_url,
            $model,
            move |model| {
                let mut builder = <$ty>::builder()
                    .provider_name(builder_provider.clone())
                    .model_name(model.to_string());
                if let Some(api_key) = builder_key.as_deref() {
                    builder = builder.api_key(api_key.to_string());
                }
                if let Some(base_url) = builder_base_url.as_deref() {
                    builder = builder.base_url(base_url.to_string());
                }
                builder.build().map_err(|error| {
                    LlmError::new("sdk_config", &error.to_string()).provider(&builder_provider)
                })
            },
        );
        Some(Arc::new(provider) as Arc<dyn LlmProvider>)
    }};
}

macro_rules! provider_match {
    ($config:expr, $($name:literal => ($ty:ty, $env:literal)),+ $(,)?) => {{
        let normalized = normalize_provider_name(&$config.name);
        match normalized.as_str() {
            $(
                $name => sdk_provider_case!(
                    $name,
                    $ty,
                    $config.name.as_str(),
                    $env,
                    $config.api_key.clone(),
                    $config.base_url.clone(),
                    $config.model.clone()
                ),
            )+
            _ => None,
        }
    }};
}

#[allow(dead_code)]
fn provider_from_config_generic(config: &ProviderConfig) -> Option<Arc<dyn LlmProvider>> {
    provider_match!(
        config,
        "302ai" => (aisdk::providers::Ai302<DynamicModel>, "AI302_API_KEY"),
        "abacus" => (aisdk::providers::Abacus<DynamicModel>, "ABACUS_API_KEY"),
        "aihubmix" => (aisdk::providers::Aihubmix<DynamicModel>, "AIHUBMIX_API_KEY"),
        "alibaba" => (aisdk::providers::Alibaba<DynamicModel>, "DASHSCOPE_API_KEY"),
        "alibaba-cn" => (aisdk::providers::AlibabaCn<DynamicModel>, "DASHSCOPE_API_KEY"),
        "amazon-bedrock" => (aisdk::providers::AmazonBedrock<DynamicModel>, "BEDROCK_API_KEY"),
        "anthropic" => (aisdk::providers::Anthropic<DynamicModel>, "ANTHROPIC_API_KEY"),
        "bailing" => (aisdk::providers::Bailing<DynamicModel>, "BAILING_API_KEY"),
        "baseten" => (aisdk::providers::Baseten<DynamicModel>, "BASETEN_API_KEY"),
        "berget" => (aisdk::providers::Berget<DynamicModel>, "BERGET_API_KEY"),
        "chutes" => (aisdk::providers::Chutes<DynamicModel>, "CHUTES_API_KEY"),
        "cloudflare-ai-gateway" => (aisdk::providers::CloudflareAiGateway<DynamicModel>, "CLOUDFLARE_API_KEY"),
        "cloudflare-workers-ai" => (aisdk::providers::CloudflareWorkersAi<DynamicModel>, "CLOUDFLARE_API_KEY"),
        "cortecs" => (aisdk::providers::Cortecs<DynamicModel>, "CORTECS_API_KEY"),
        "deepseek" => (aisdk::providers::Deepseek<DynamicModel>, "DEEPSEEK_API_KEY"),
        "fastrouter" => (aisdk::providers::Fastrouter<DynamicModel>, "FASTROUTER_API_KEY"),
        "fireworks-ai" => (aisdk::providers::FireworksAi<DynamicModel>, "FIREWORKS_API_KEY"),
        "firmware" => (aisdk::providers::Firmware<DynamicModel>, "FIRMWARE_API_KEY"),
        "friendli" => (aisdk::providers::Friendli<DynamicModel>, "FRIENDLI_TOKEN"),
        "github-copilot" => (aisdk::providers::GithubCopilot<DynamicModel>, "GITHUB_COPILOT_API_KEY"),
        "github-models" => (aisdk::providers::GithubModels<DynamicModel>, "GITHUB_TOKEN"),
        "google" => (aisdk::providers::Google<DynamicModel>, "GOOGLE_GENERATIVE_AI_API_KEY"),
        "groq" => (aisdk::providers::Groq<DynamicModel>, "GROQ_API_KEY"),
        "helicone" => (aisdk::providers::Helicone<DynamicModel>, "HELICONE_API_KEY"),
        "huggingface" => (aisdk::providers::Huggingface<DynamicModel>, "HUGGINGFACE_API_KEY"),
        "iflowcn" => (aisdk::providers::Iflowcn<DynamicModel>, "IFLOW_API_KEY"),
        "inception" => (aisdk::providers::Inception<DynamicModel>, "INCEPTION_API_KEY"),
        "inference" => (aisdk::providers::Inference<DynamicModel>, "INFERENCE_API_KEY"),
        "io-net" => (aisdk::providers::IoNet<DynamicModel>, "IONET_API_KEY"),
        "jiekou" => (aisdk::providers::Jiekou<DynamicModel>, "JIEKOU_API_KEY"),
        "kuae-cloud-coding-plan" => (aisdk::providers::KuaeCloudCodingPlan<DynamicModel>, "KUAE_API_KEY"),
        "llama" => (aisdk::providers::Llama<DynamicModel>, "LLAMA_API_KEY"),
        "lmstudio" => (aisdk::providers::Lmstudio<DynamicModel>, "LMSTUDIO_API_KEY"),
        "lucidquery" => (aisdk::providers::Lucidquery<DynamicModel>, "LUCIDQUERY_API_KEY"),
        "mistral" => (aisdk::providers::Mistral<DynamicModel>, "MISTRAL_API_KEY"),
        "moark" => (aisdk::providers::Moark<DynamicModel>, "MOARK_API_KEY"),
        "modelscope" => (aisdk::providers::Modelscope<DynamicModel>, "MODELSCOPE_API_KEY"),
        "moonshotai" => (aisdk::providers::Moonshotai<DynamicModel>, "MOONSHOT_API_KEY"),
        "moonshotai-cn" => (aisdk::providers::MoonshotaiCn<DynamicModel>, "MOONSHOT_API_KEY"),
        "morph" => (aisdk::providers::Morph<DynamicModel>, "MORPH_API_KEY"),
        "nano-gpt" => (aisdk::providers::NanoGpt<DynamicModel>, "NANOGPT_API_KEY"),
        "nebius" => (aisdk::providers::Nebius<DynamicModel>, "NEBIUS_API_KEY"),
        "nova" => (aisdk::providers::Nova<DynamicModel>, "NOVA_API_KEY"),
        "novita-ai" => (aisdk::providers::NovitaAi<DynamicModel>, "NOVITA_API_KEY"),
        "nvidia" => (aisdk::providers::Nvidia<DynamicModel>, "NVIDIA_API_KEY"),
        "ollama-cloud" => (aisdk::providers::OllamaCloud<DynamicModel>, "OLLAMA_API_KEY"),
        "opencode" => (aisdk::providers::Opencode<DynamicModel>, "OPENCODE_API_KEY"),
        "openai" => (aisdk::providers::OpenAI<DynamicModel>, "OPENAI_API_KEY"),
        "openaicompatible" => (aisdk::providers::OpenAICompatible<DynamicModel>, "OPENAI_API_KEY"),
        "openai-compatible" => (aisdk::providers::OpenAICompatible<DynamicModel>, "OPENAI_API_KEY"),
        "openrouter" => (aisdk::providers::Openrouter<DynamicModel>, "OPENROUTER_API_KEY"),
        "ovhcloud" => (aisdk::providers::Ovhcloud<DynamicModel>, "OVHCLOUD_API_KEY"),
        "poe" => (aisdk::providers::Poe<DynamicModel>, "POE_API_KEY"),
        "requesty" => (aisdk::providers::Requesty<DynamicModel>, "REQUESTY_API_KEY"),
        "scaleway" => (aisdk::providers::Scaleway<DynamicModel>, "SCALEWAY_API_KEY"),
        "siliconflow" => (aisdk::providers::Siliconflow<DynamicModel>, "SILICONFLOW_API_KEY"),
        "siliconflow-cn" => (aisdk::providers::SiliconflowCn<DynamicModel>, "SILICONFLOW_API_KEY"),
        "stackit" => (aisdk::providers::Stackit<DynamicModel>, "STACKIT_API_KEY"),
        "stepfun" => (aisdk::providers::Stepfun<DynamicModel>, "STEPFUN_API_KEY"),
        "submodel" => (aisdk::providers::Submodel<DynamicModel>, "SUBMODEL_API_KEY"),
        "synthetic" => (aisdk::providers::Synthetic<DynamicModel>, "SYNTHETIC_API_KEY"),
        "togetherai" => (aisdk::providers::TogetherAI<DynamicModel>, "TOGETHER_API_KEY"),
        "upstage" => (aisdk::providers::Upstage<DynamicModel>, "UPSTAGE_API_KEY"),
        "vercel" => (aisdk::providers::Vercel<DynamicModel>, "VERCEL_API_KEY"),
        "vultr" => (aisdk::providers::Vultr<DynamicModel>, "VULTR_API_KEY"),
        "wandb" => (aisdk::providers::Wandb<DynamicModel>, "WANDB_API_KEY"),
        "xai" => (aisdk::providers::XAI<DynamicModel>, "XAI_API_KEY"),
        "xiaomi" => (aisdk::providers::Xiaomi<DynamicModel>, "XIAOMI_API_KEY"),
        "zai" => (aisdk::providers::Zai<DynamicModel>, "ZHIPU_API_KEY"),
        "zai-coding-plan" => (aisdk::providers::ZaiCodingPlan<DynamicModel>, "ZHIPU_API_KEY"),
        "zenmux" => (aisdk::providers::Zenmux<DynamicModel>, "ZENMUX_API_KEY"),
        "zhipuai" => (aisdk::providers::Zhipuai<DynamicModel>, "ZHIPU_API_KEY"),
        "zhipuai-coding-plan" => (aisdk::providers::ZhipuaiCodingPlan<DynamicModel>, "ZHIPU_API_KEY"),
    )
}

pub fn supported_provider_names() -> &'static [&'static str] {
    &[
        "302ai",
        "abacus",
        "aihubmix",
        "alibaba",
        "alibaba-cn",
        "amazon-bedrock",
        "anthropic",
        "bailing",
        "baseten",
        "berget",
        "chutes",
        "cloudflare-ai-gateway",
        "cloudflare-workers-ai",
        "cortecs",
        "deepseek",
        "fastrouter",
        "fireworks-ai",
        "firmware",
        "friendli",
        "github-copilot",
        "github-models",
        "google",
        "groq",
        "helicone",
        "huggingface",
        "iflowcn",
        "inception",
        "inference",
        "io-net",
        "jiekou",
        "kuae-cloud-coding-plan",
        "llama",
        "lmstudio",
        "lucidquery",
        "mistral",
        "moark",
        "modelscope",
        "moonshotai",
        "moonshotai-cn",
        "morph",
        "nano-gpt",
        "nebius",
        "nova",
        "novita-ai",
        "nvidia",
        "ollama-cloud",
        "openai",
        "openaicompatible",
        "opencode",
        "openrouter",
        "ovhcloud",
        "poe",
        "requesty",
        "scaleway",
        "siliconflow",
        "siliconflow-cn",
        "stackit",
        "stepfun",
        "submodel",
        "synthetic",
        "togetherai",
        "upstage",
        "vercel",
        "vultr",
        "wandb",
        "xai",
        "xiaomi",
        "zai",
        "zai-coding-plan",
        "zenmux",
        "zhipuai",
        "zhipuai-coding-plan",
    ]
}

pub fn docs_url(provider_name: &str) -> String {
    format!(
        "https://aisdk.rs/docs/providers/{}",
        normalize_provider_name(provider_name)
    )
}

pub fn api_key_env(provider_name: &str) -> Option<&'static str> {
    match normalize_provider_name(provider_name).as_str() {
        "302ai" => Some("AI302_API_KEY"),
        "abacus" => Some("ABACUS_API_KEY"),
        "aihubmix" => Some("AIHUBMIX_API_KEY"),
        "alibaba" | "alibaba-cn" => Some("DASHSCOPE_API_KEY"),
        "amazon-bedrock" => Some("BEDROCK_API_KEY"),
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        "bailing" => Some("BAILING_API_KEY"),
        "baseten" => Some("BASETEN_API_KEY"),
        "berget" => Some("BERGET_API_KEY"),
        "chutes" => Some("CHUTES_API_KEY"),
        "cloudflare-ai-gateway" | "cloudflare-workers-ai" => Some("CLOUDFLARE_API_KEY"),
        "cortecs" => Some("CORTECS_API_KEY"),
        "deepseek" => Some("DEEPSEEK_API_KEY"),
        "fastrouter" => Some("FASTROUTER_API_KEY"),
        "fireworks-ai" => Some("FIREWORKS_API_KEY"),
        "firmware" => Some("FIRMWARE_API_KEY"),
        "friendli" => Some("FRIENDLI_TOKEN"),
        "github-copilot" => Some("GITHUB_COPILOT_API_KEY"),
        "github-models" => Some("GITHUB_TOKEN"),
        "google" => Some("GOOGLE_GENERATIVE_AI_API_KEY"),
        "groq" => Some("GROQ_API_KEY"),
        "helicone" => Some("HELICONE_API_KEY"),
        "huggingface" => Some("HUGGINGFACE_API_KEY"),
        "iflowcn" => Some("IFLOW_API_KEY"),
        "inception" => Some("INCEPTION_API_KEY"),
        "inference" => Some("INFERENCE_API_KEY"),
        "io-net" => Some("IONET_API_KEY"),
        "jiekou" => Some("JIEKOU_API_KEY"),
        "kuae-cloud-coding-plan" => Some("KUAE_API_KEY"),
        "llama" => Some("LLAMA_API_KEY"),
        "lmstudio" => Some("LMSTUDIO_API_KEY"),
        "lucidquery" => Some("LUCIDQUERY_API_KEY"),
        "mistral" => Some("MISTRAL_API_KEY"),
        "moark" => Some("MOARK_API_KEY"),
        "modelscope" => Some("MODELSCOPE_API_KEY"),
        "moonshotai" | "moonshotai-cn" => Some("MOONSHOT_API_KEY"),
        "morph" => Some("MORPH_API_KEY"),
        "nano-gpt" => Some("NANOGPT_API_KEY"),
        "nebius" => Some("NEBIUS_API_KEY"),
        "nova" => Some("NOVA_API_KEY"),
        "novita-ai" => Some("NOVITA_API_KEY"),
        "nvidia" => Some("NVIDIA_API_KEY"),
        "ollama-cloud" => Some("OLLAMA_API_KEY"),
        "opencode" => Some("OPENCODE_API_KEY"),
        "openai" | "openaicompatible" | "openai-compatible" => Some("OPENAI_API_KEY"),
        "openrouter" => Some("OPENROUTER_API_KEY"),
        "ovhcloud" => Some("OVHCLOUD_API_KEY"),
        "poe" => Some("POE_API_KEY"),
        "requesty" => Some("REQUESTY_API_KEY"),
        "scaleway" => Some("SCALEWAY_API_KEY"),
        "siliconflow" | "siliconflow-cn" => Some("SILICONFLOW_API_KEY"),
        "stackit" => Some("STACKIT_API_KEY"),
        "stepfun" => Some("STEPFUN_API_KEY"),
        "submodel" => Some("SUBMODEL_API_KEY"),
        "synthetic" => Some("SYNTHETIC_API_KEY"),
        "togetherai" => Some("TOGETHER_API_KEY"),
        "upstage" => Some("UPSTAGE_API_KEY"),
        "vercel" => Some("VERCEL_API_KEY"),
        "vultr" => Some("VULTR_API_KEY"),
        "wandb" => Some("WANDB_API_KEY"),
        "xai" => Some("XAI_API_KEY"),
        "xiaomi" => Some("XIAOMI_API_KEY"),
        "zai" | "zai-coding-plan" | "zhipuai" | "zhipuai-coding-plan" => Some("ZHIPU_API_KEY"),
        "zenmux" => Some("ZENMUX_API_KEY"),
        _ => None,
    }
}

pub fn provider_model_env(provider_name: &str) -> String {
    format!("AISDK_{}_MODEL", env_suffix(provider_name))
}

pub fn provider_base_url_env(provider_name: &str) -> String {
    format!("AISDK_{}_BASE_URL", env_suffix(provider_name))
}

fn normalize_provider_name(name: &str) -> String {
    name.trim().to_lowercase().replace('_', "-")
}

fn env_suffix(provider_name: &str) -> String {
    normalize_provider_name(provider_name)
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn sdk_system_prompt(request: &LlmRequest) -> String {
    let mut parts = request.system.clone();
    for message in &request.messages {
        if message.role == "system" {
            parts.extend(
                message
                    .content
                    .iter()
                    .filter_map(ContentPart::as_prompt_text),
            );
        }
    }
    parts
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn sdk_messages(request: &LlmRequest) -> Vec<Message> {
    let mut messages = Vec::new();
    for message in &request.messages {
        if message.role == "system" {
            continue;
        }

        match message.role.as_str() {
            "assistant" => messages.extend(sdk_assistant_messages(&message.content)),
            "tool" => messages.extend(sdk_tool_result_messages(&message.content)),
            "user" => {
                let text = prompt_text(&message.content);
                if !text.is_empty() {
                    messages.push(Message::User(UserMessage::new(text)));
                }
            }
            _ => {
                let text = prompt_text(&message.content);
                if !text.is_empty() {
                    messages.push(Message::User(UserMessage::new(text)));
                }
            }
        }
    }
    sanitize_sdk_messages(messages)
}

fn sdk_assistant_messages(content: &[ContentPart]) -> Vec<Message> {
    let mut messages = Vec::new();
    let mut text_parts = Vec::new();
    let mut pending_tool_calls: Vec<ToolCallInfo> = Vec::new();

    let flush_pending_tool_calls =
        |messages: &mut Vec<Message>, pending: &mut Vec<ToolCallInfo>| {
            messages.extend(pending.drain(..).map(|tool_call| {
                Message::Assistant(AssistantMessage::new(
                    LanguageModelResponseContentType::ToolCall(tool_call),
                    None,
                ))
            }));
        };

    for part in content {
        match part {
            ContentPart::ToolCall { id, name, input } => {
                let mut tool_call = ToolCallInfo::new(name.clone());
                tool_call.id(id.clone());
                tool_call.input(input.clone());
                pending_tool_calls.push(tool_call);
            }
            ContentPart::ToolResult { id, name, result } => {
                if let Some(index) = pending_tool_calls
                    .iter()
                    .position(|call| call.tool.id == *id)
                {
                    let tool_call = pending_tool_calls.remove(index);
                    messages.push(Message::Assistant(AssistantMessage::new(
                        LanguageModelResponseContentType::ToolCall(tool_call),
                        None,
                    )));
                } else {
                    flush_pending_tool_calls(&mut messages, &mut pending_tool_calls);
                }
                messages.push(Message::Tool(sdk_tool_result_info(id, name, result)));
            }
            ContentPart::Reasoning { .. } => {}
            _ => {
                if let Some(text) = part.as_prompt_text() {
                    if !text.is_empty() {
                        text_parts.push(text);
                    }
                }
            }
        }
    }

    flush_pending_tool_calls(&mut messages, &mut pending_tool_calls);

    if !text_parts.is_empty() {
        messages.insert(
            0,
            Message::Assistant(AssistantMessage::from(text_parts.join("\n"))),
        );
    }

    messages
}

fn sdk_tool_result_messages(content: &[ContentPart]) -> Vec<Message> {
    content
        .iter()
        .filter_map(|part| match part {
            ContentPart::ToolResult { id, name, result } => {
                Some(Message::Tool(sdk_tool_result_info(id, name, result)))
            }
            _ => None,
        })
        .collect()
}

fn sanitize_sdk_messages(messages: Vec<Message>) -> Vec<Message> {
    let mut pending_tool_call_ids = HashSet::new();
    let mut sanitized = Vec::with_capacity(messages.len());
    let mut orphan_tool_results = 0_usize;

    for message in messages {
        match &message {
            Message::Assistant(assistant) => {
                if let LanguageModelResponseContentType::ToolCall(call) = &assistant.content {
                    pending_tool_call_ids.insert(call.tool.id.clone());
                }
                sanitized.push(message);
            }
            Message::Tool(result) => {
                if pending_tool_call_ids.remove(&result.tool.id) {
                    sanitized.push(message);
                } else {
                    orphan_tool_results += 1;
                    tracing::warn!(
                        tool_call_id = %result.tool.id,
                        tool = %result.tool.name,
                        "lowering orphan SDK tool result as user text to avoid invalid Responses input"
                    );
                    sanitized.push(Message::User(UserMessage::new(orphan_tool_result_text(
                        result,
                    ))));
                }
            }
            _ => sanitized.push(message),
        }
    }

    if orphan_tool_results > 0 {
        tracing::warn!(
            orphan_tool_results,
            message_count = sanitized.len(),
            "sanitized SDK message history before provider request"
        );
    }

    sanitized
}

fn orphan_tool_result_text(result: &ToolResultInfo) -> String {
    let value = result
        .output
        .clone()
        .unwrap_or_else(|error| serde_json::Value::String(error.to_string()));
    format!(
        "<tool_result tool_call_id=\"{}\" name=\"{}\">\n{}\n</tool_result>",
        result.tool.id,
        result.tool.name,
        serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string())
    )
}

fn log_aisdk_request_shape(
    provider_name: &str,
    sdk_provider_name: &str,
    request: &LlmRequest,
    system: &str,
    messages: &[Message],
) {
    let message_shape = messages
        .iter()
        .enumerate()
        .map(|(index, message)| sdk_message_shape(index, message))
        .collect::<Vec<_>>();
    let tool_shape = request
        .tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "name": tool.name.as_str(),
                "description_bytes": tool.description.len(),
                "parameters_type": json_type_name(&tool.parameters),
                "parameters_bytes": json_bytes(&tool.parameters),
            })
        })
        .collect::<Vec<_>>();

    tracing::debug!(
        provider = %provider_name,
        sdk_provider = %sdk_provider_name,
        model = %request.model,
        backend = "aisdk",
        session_id = request.session_id.as_deref().unwrap_or(""),
        system_bytes = system.len(),
        system_preview = %preview(system, 512),
        request_message_count = request.messages.len(),
        sdk_message_count = messages.len(),
        tool_count = request.tools.len(),
        has_temperature = request.temperature.is_some(),
        has_top_p = request.top_p.is_some(),
        max_tokens = request.max_tokens.unwrap_or(0),
        stop_count = request.stop.as_ref().map(Vec::len).unwrap_or(0),
        sdk_message_shape = %json_string(&message_shape),
        tool_shape = %json_string(&tool_shape),
        "native AISDK provider lowered request shape"
    );
}

fn sdk_message_shape(index: usize, message: &Message) -> serde_json::Value {
    match message {
        Message::System(system) => serde_json::json!({
            "index": index,
            "role": "system",
            "content_bytes": system.content.len(),
            "content_preview": preview(&system.content, 180),
        }),
        Message::User(user) => serde_json::json!({
            "index": index,
            "role": "user",
            "content_bytes": user.content.len(),
            "content_preview": preview(&user.content, 180),
        }),
        Message::Assistant(assistant) => match &assistant.content {
            LanguageModelResponseContentType::Text(text) => serde_json::json!({
                "index": index,
                "role": "assistant",
                "content_type": "text",
                "content_bytes": text.len(),
                "content_preview": preview(text, 180),
            }),
            LanguageModelResponseContentType::ToolCall(call) => serde_json::json!({
                "index": index,
                "role": "assistant",
                "content_type": "tool_call",
                "tool_call_id": call.tool.id.as_str(),
                "tool": call.tool.name.as_str(),
                "input_type": json_type_name(&call.input),
                "input_bytes": json_bytes(&call.input),
                "input_preview": preview(&json_string(&call.input), 180),
            }),
            LanguageModelResponseContentType::Reasoning { content, .. } => serde_json::json!({
                "index": index,
                "role": "assistant",
                "content_type": "reasoning",
                "content_bytes": content.len(),
                "content_preview": preview(content, 180),
            }),
            LanguageModelResponseContentType::NotSupported(content) => serde_json::json!({
                "index": index,
                "role": "assistant",
                "content_type": "not_supported",
                "content_bytes": content.len(),
                "content_preview": preview(content, 180),
            }),
        },
        Message::Tool(result) => {
            let output = result
                .output
                .as_ref()
                .map(json_string)
                .unwrap_or_else(|error| error.to_string());
            serde_json::json!({
                "index": index,
                "role": "tool",
                "tool_call_id": result.tool.id.as_str(),
                "tool": result.tool.name.as_str(),
                "output_ok": result.output.is_ok(),
                "output_bytes": output.len(),
                "output_preview": preview(&output, 180),
            })
        }
        Message::Developer(content) => serde_json::json!({
            "index": index,
            "role": "developer",
            "content_bytes": content.len(),
            "content_preview": preview(content, 180),
        }),
    }
}

fn preview(value: &str, max_chars: usize) -> String {
    let mut preview = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        preview.push_str("...");
    }
    preview
}

fn json_string(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<json serialization failed>".to_string())
}

fn json_bytes(value: &impl serde::Serialize) -> usize {
    json_string(value).len()
}

fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn sdk_tool_result_info(id: &str, name: &str, result: &serde_json::Value) -> ToolResultInfo {
    let mut tool_result = ToolResultInfo::new(name.to_string());
    tool_result.id(id.to_string());
    tool_result.output(result.clone());
    tool_result
}

fn prompt_text(content: &[ContentPart]) -> String {
    content
        .iter()
        .filter_map(ContentPart::as_prompt_text)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn sdk_tool(tool: &ToolDefinition) -> Result<Tool, LlmError> {
    let schema = aisdk::__private::schemars::Schema::try_from(tool.parameters.clone())
        .unwrap_or_else(|_| aisdk::__private::schemars::Schema::default());

    Tool::builder()
        .name(tool.name.clone())
        .description(tool.description.clone())
        .input_schema(schema)
        .execute(ToolExecute::new(Box::new(|_| Ok(String::new()))))
        .build()
        .map_err(|error| LlmError::new("sdk_tool", &error.to_string()))
}

fn sdk_generated_step_has_tool_calls(step: &Step) -> bool {
    step.step_id > 0 && step.tool_calls().is_some_and(|calls| !calls.is_empty())
}

fn sdk_generated_step_tool_calls(step: Option<Step>) -> Vec<ToolCallInfo> {
    let Some(step) = step else {
        return Vec::new();
    };

    if step.step_id == 0 {
        return Vec::new();
    }

    step.tool_calls().unwrap_or_default()
}

fn sdk_tool_call_events(call: &ToolCallInfo) -> Vec<LlmEvent> {
    let id = if call.tool.id.is_empty() {
        format!("tool-{}", call.tool.name)
    } else {
        call.tool.id.clone()
    };
    let name = call.tool.name.clone();
    let input = call.input.clone();
    let input_text = serde_json::to_string(&input).unwrap_or_else(|_| "null".to_string());

    vec![
        LlmEvent::ToolInputStart {
            id: id.clone(),
            name: name.clone(),
        },
        LlmEvent::ToolInputDelta {
            id: id.clone(),
            name: name.clone(),
            text: input_text,
        },
        LlmEvent::ToolInputEnd {
            id: id.clone(),
            name: name.clone(),
        },
        LlmEvent::ToolCall {
            id,
            name,
            input,
            provider_executed: Some(true),
        },
    ]
}

fn sdk_usage(usage: aisdk::core::language_model::Usage) -> crate::llm::events::Usage {
    let input_tokens = usage.input_tokens.map(|value| value as u64);
    let output_tokens = usage.output_tokens.map(|value| value as u64);
    crate::llm::events::Usage {
        input_tokens,
        output_tokens,
        total_tokens: match (input_tokens, output_tokens) {
            (Some(input), Some(output)) => Some(input + output),
            _ => None,
        },
        reasoning_tokens: usage.reasoning_tokens.map(|value| value as u64),
        cache_read_input_tokens: usage.cached_tokens.map(|value| value as u64),
        cache_write_input_tokens: None,
    }
}

fn sdk_finish_reason(reason: Option<StopReason>) -> FinishReason {
    match reason {
        Some(StopReason::Finish) => FinishReason::Stop,
        Some(StopReason::Hook) => FinishReason::Stop,
        Some(StopReason::Error(_)) => FinishReason::Error,
        Some(StopReason::Provider(_)) => FinishReason::Error,
        Some(StopReason::Other(_)) | None => FinishReason::Unknown,
    }
}

fn percent_u32(value: f32) -> u32 {
    (value.clamp(0.0, 1.0) * 100.0).round() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::providers::provider_from_config;
    use crate::llm::request::LlmMessage;
    use crate::{ProviderAuthFieldType, ProviderConfig, ProviderProtocol};
    use futures_util::StreamExt;

    #[test]
    fn factory_supports_every_listed_provider_with_dummy_key() {
        for provider_name in supported_provider_names() {
            let config = ProviderConfig {
                name: provider_name.to_string(),
                model: "test-model".to_string(),
                api_key: Some("test-key".to_string()),
                base_url: None,
                protocol: ProviderProtocol::OpenAI,
                enabled: true,
                auth_account_id: None,
                auth_field_type: ProviderAuthFieldType::ApiKey,
            };
            assert!(
                provider_from_config(&config).is_some(),
                "provider should be supported by AISDK factory: {provider_name}"
            );
        }
    }

    #[test]
    fn docs_url_uses_provider_path() {
        assert_eq!(
            docs_url("Anthropic"),
            "https://aisdk.rs/docs/providers/anthropic"
        );
    }

    #[test]
    fn sdk_generated_step_tool_calls_ignore_initial_history_step() {
        let mut call = ToolCallInfo::new("read");
        call.id("call_history");
        call.input(serde_json::json!({"path":"README.md"}));

        let step = Step::new(
            0,
            vec![Message::Assistant(AssistantMessage::new(
                LanguageModelResponseContentType::ToolCall(call),
                None,
            ))],
        );

        assert!(!sdk_generated_step_has_tool_calls(&step));
        assert!(sdk_generated_step_tool_calls(Some(step)).is_empty());
    }

    #[test]
    fn sdk_generated_step_tool_calls_keep_generated_step() {
        let mut call = ToolCallInfo::new("glob");
        call.id("call_new");
        call.input(serde_json::json!({"path":"src"}));

        let step = Step::new(
            1,
            vec![Message::Assistant(AssistantMessage::new(
                LanguageModelResponseContentType::ToolCall(call),
                None,
            ))],
        );

        assert!(sdk_generated_step_has_tool_calls(&step));
        let calls = sdk_generated_step_tool_calls(Some(step));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool.id, "call_new");
    }

    #[tokio::test]
    #[ignore = "requires provider API keys and AISDK_<PROVIDER>_MODEL env vars"]
    async fn live_smoke_streams_configured_aisdk_providers_from_env() {
        for provider_name in supported_provider_names() {
            let Some(api_key_env) = api_key_env(provider_name) else {
                continue;
            };
            let Ok(api_key) = std::env::var(api_key_env) else {
                continue;
            };
            if api_key.trim().is_empty() {
                continue;
            }

            let model_env = provider_model_env(provider_name);
            let Ok(model) = std::env::var(&model_env) else {
                continue;
            };
            if model.trim().is_empty() {
                continue;
            }

            let base_url = std::env::var(provider_base_url_env(provider_name)).ok();
            let config = ProviderConfig {
                name: provider_name.to_string(),
                model: model.clone(),
                api_key: Some(api_key),
                base_url,
                protocol: ProviderProtocol::OpenAI,
                enabled: true,
                auth_account_id: None,
                auth_field_type: ProviderAuthFieldType::ApiKey,
            };
            let provider = provider_from_config(&config).expect("provider factory");
            let request = LlmRequest::new(&model, provider_name)
                .with_message(crate::llm::request::LlmMessage::user(
                    "Reply with exactly: ok",
                ))
                .max_tokens(16);
            let stream = provider.stream(request).await.unwrap_or_else(|error| {
                panic!("{} failed to start stream: {error}", provider_name)
            });
            let events: Vec<_> = stream.events.collect().await;
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, LlmEvent::Finish { .. })),
                "{provider_name} did not emit a Finish event: {events:?}"
            );
        }
    }

    #[test]
    fn sdk_tool_result_message_preserves_json_value_for_wire() {
        let content = vec![ContentPart::tool_result(
            "call_1",
            "read",
            serde_json::json!({"text": "hello world"}),
        )];

        let messages = sdk_tool_result_messages(&content);
        assert_eq!(messages.len(), 1);

        let Message::Tool(tool_result) = &messages[0] else {
            panic!("expected Message::Tool, got {:?}", messages[0]);
        };
        let output = tool_result.output.as_ref().expect("output should be Ok");
        assert_eq!(output, &serde_json::json!({"text": "hello world"}));
    }

    #[test]
    fn sdk_assistant_messages_keep_tool_results_after_matching_calls() {
        let messages = sdk_assistant_messages(&[
            ContentPart::tool_call("call_1", "read", serde_json::json!({"path": "README.md"})),
            ContentPart::tool_result("call_1", "read", serde_json::json!({"text": "hello"})),
        ]);

        assert!(matches!(messages[0], Message::Assistant(_)));
        let Message::Tool(tool_result) = &messages[1] else {
            panic!("expected Message::Tool, got {:?}", messages[1]);
        };
        assert_eq!(tool_result.tool.id, "call_1");
    }

    #[test]
    fn sdk_messages_downgrade_orphan_tool_results_to_user_text() {
        let request = LlmRequest::new("gpt-5.5", "openai").with_message(LlmMessage::tool(
            "orphan_1",
            "read",
            serde_json::json!({"text": "hello"}),
        ));

        let messages = sdk_messages(&request);

        assert_eq!(messages.len(), 1);
        let Message::User(user) = &messages[0] else {
            panic!("expected orphan tool result to become user text, got {messages:?}");
        };
        assert!(user.content.contains("tool_result"));
        assert!(user.content.contains("orphan_1"));
    }
}
