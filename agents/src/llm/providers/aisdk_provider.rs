use async_trait::async_trait;
use futures_util::StreamExt;
use std::sync::Arc;

use aisdk::core::capabilities::{TextInputSupport, ToolCallSupport};
use aisdk::core::language_model::{LanguageModelResponseContentType, StopReason};
use aisdk::core::tools::ToolExecute;
use aisdk::core::{
    AssistantMessage, DynamicModel, LanguageModel, LanguageModelRequest,
    LanguageModelStreamChunkType, Message, Tool, ToolCallInfo, ToolResultInfo, UserMessage,
};

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
    api_key_env: Option<String>,
    api_key: Option<String>,
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
        api_key_env: Option<&str>,
        api_key: Option<String>,
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
            api_key_env,
            api_key,
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
        log_sdk_request_body(
            &self.provider_name,
            self.base_url.as_deref(),
            &request,
            &system,
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
            builder = builder.stop_when(|options| options.tool_calls().is_some());
        }

        let mut sdk_request = builder.build();
        let response = sdk_request.stream_text().await.map_err(|error| {
            LlmError::new("sdk_request", &error.to_string())
                .provider(&self.provider_name)
                .model(&request.model)
        })?;

        let stream_provider = self.provider_name.clone();
        let stream_model = request.model.clone();
        let stream = async_stream::stream! {
            let mut response = response;
            let mut saw_provider_error = false;

            while let Some(chunk) = response.stream.next().await {
                match chunk {
                    LanguageModelStreamChunkType::Start => {}
                    LanguageModelStreamChunkType::Text(text) => {
                        if !text.is_empty() {
                            yield LlmEvent::TextDelta { id: "text".to_string(), text };
                        }
                    }
                    LanguageModelStreamChunkType::Reasoning(text) => {
                        if !text.is_empty() {
                            yield LlmEvent::ReasoningDelta { id: "reasoning".to_string(), text };
                        }
                    }
                    LanguageModelStreamChunkType::ToolCall(_) => {}
                    LanguageModelStreamChunkType::End(_) => {}
                    LanguageModelStreamChunkType::Failed(message)
                    | LanguageModelStreamChunkType::NotSupported(message) => {
                        saw_provider_error = true;
                        yield LlmEvent::ProviderError { message };
                    }
                    LanguageModelStreamChunkType::Incomplete(message) => {
                        if message != "Stopped by hook" {
                            saw_provider_error = true;
                            yield LlmEvent::ProviderError { message };
                        }
                    }
                }
            }

            let tool_calls = response.tool_calls().await.unwrap_or_default();
            for call in &tool_calls {
                for event in sdk_tool_call_events(call) {
                    yield event;
                }
            }

            let usage = Some(sdk_usage(response.usage().await));
            let reason = if !tool_calls.is_empty() {
                FinishReason::ToolCalls
            } else if saw_provider_error {
                FinishReason::Error
            } else {
                sdk_finish_reason(response.stop_reason().await)
            };

            tracing::debug!(
                "sdk llm stream finished: provider={} model={} reason={}",
                stream_provider,
                stream_model,
                reason,
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

macro_rules! sdk_provider_case {
    ($name:expr, $ty:ty, $provider_name:expr, $api_key_env:expr, $api_key:expr, $base_url:expr, $model:expr) => {{
        let provider_name = $provider_name.to_string();
        let api_key_env = $api_key_env;
        let resolved_api_key = $api_key.or_else(|| std::env::var(api_key_env).ok());
        let base_url = $base_url;
        let builder_provider = provider_name.clone();
        let builder_key = resolved_api_key.clone();
        let builder_base_url = base_url.clone();
        let provider = AisdkLanguageModelProvider::<$ty>::new(
            provider_name,
            Some(api_key_env),
            resolved_api_key,
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

fn log_sdk_request_body(
    provider_name: &str,
    base_url: Option<&str>,
    request: &LlmRequest,
    resolved_system: &str,
) {
    let body = sdk_request_debug_body(provider_name, base_url, request, resolved_system);
    let body =
        serde_json::to_string_pretty(&body).unwrap_or_else(|_| "<unserializable>".to_string());
    const MAX_LOG_BYTES: usize = 64 * 1024;
    let body_truncated = body.len() > MAX_LOG_BYTES;
    let body_display = if body_truncated {
        body.chars().take(MAX_LOG_BYTES).collect::<String>()
    } else {
        body
    };

    tracing::info!(
        provider = %provider_name,
        model = %request.model,
        base_url = ?base_url,
        body_bytes = body_display.len(),
        body_truncated,
        body = %body_display,
        "aisdk request body prepared"
    );
}

fn sdk_request_debug_body(
    provider_name: &str,
    base_url: Option<&str>,
    request: &LlmRequest,
    resolved_system: &str,
) -> serde_json::Value {
    serde_json::json!({
        "adapter": "aisdk",
        "provider": provider_name,
        "model": request.model,
        "base_url": base_url,
        "session_id": request.session_id,
        "input_style": "messages",
        "system": if resolved_system.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(resolved_system.to_string())
        },
        "messages": request
            .messages
            .iter()
            .filter(|message| message.role != "system")
            .map(debug_message_json)
            .collect::<Vec<_>>(),
        "options": {
            "temperature": request.temperature.map(percent_u32),
            "temperature_source_scale": "LlmRequest f32 0.0-1.0 converted to AISDK u32 0-100",
            "top_p": request.top_p.map(percent_u32),
            "top_p_source_scale": "LlmRequest f32 0.0-1.0 converted to AISDK u32 0-100",
            "max_output_tokens": request.max_tokens,
            "stop_sequences": request.stop,
            "stop_when": if request.tools.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::Value::String("stop after AISDK records a tool call; SessionRunner executes tools".to_string())
            },
        },
        "tools": request
            .tools
            .iter()
            .map(|tool| serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.parameters,
                "execute": "no-op placeholder; SessionRunner executes tools",
            }))
            .collect::<Vec<_>>(),
        "notes": [
            "This is the sanitized AISDK adapter input, not a provider-secret-bearing HTTP dump.",
            "AISDK lowers this shape into each provider's native wire request internally.",
            "API keys are intentionally omitted.",
        ],
    })
}

fn debug_message_json(message: &crate::llm::request::LlmMessage) -> serde_json::Value {
    serde_json::json!({
        "role": message.role,
        "content": message.content.iter().map(debug_content_part_json).collect::<Vec<_>>(),
    })
}

fn debug_content_part_json(part: &ContentPart) -> serde_json::Value {
    match part {
        ContentPart::Text { text } => serde_json::json!({
            "type": "text",
            "text": text,
        }),
        ContentPart::ToolCall { id, name, input } => serde_json::json!({
            "type": "tool_call",
            "id": id,
            "name": name,
            "input": input,
        }),
        ContentPart::ToolResult { id, name, result } => serde_json::json!({
            "type": "tool_result",
            "id": id,
            "name": name,
            "result": result,
        }),
        ContentPart::Reasoning { text } => serde_json::json!({
            "type": "reasoning",
            "text": text,
            "sent_to_aisdk": false,
        }),
    }
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
    messages
}

fn sdk_assistant_messages(content: &[ContentPart]) -> Vec<Message> {
    let mut messages = Vec::new();
    let mut text_parts = Vec::new();

    for part in content {
        match part {
            ContentPart::ToolCall { id, name, input } => {
                let mut tool_call = ToolCallInfo::new(name.clone());
                tool_call.id(id.clone());
                tool_call.input(input.clone());
                messages.push(Message::Assistant(AssistantMessage::new(
                    LanguageModelResponseContentType::ToolCall(tool_call),
                    None,
                )));
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
                let mut tool_result = ToolResultInfo::new(name.clone());
                tool_result.id(id.clone());
                let result_text =
                    serde_json::to_string(result).unwrap_or_else(|_| "null".to_string());
                tool_result.output(serde_json::Value::String(result_text));
                Some(Message::Tool(tool_result))
            }
            _ => None,
        })
        .collect()
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
            provider_executed: Some(false),
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
    use crate::{ProviderConfig, ProviderProtocol};
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
    fn sdk_tool_result_message_stringifies_result_for_wire() {
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
        let output_string = output
            .as_str()
            .expect("output should be Value::String, not nested Object");
        assert_eq!(output_string, "{\"text\":\"hello world\"}");
    }
}
