use async_trait::async_trait;
use std::sync::Arc;

use aisdk::core::DynamicModel;

use super::aisdk_provider::AisdkLanguageModelProvider;
use super::{LlmError, LlmProvider, LlmStream};
use crate::llm::providers::sse_proxy;
use crate::llm::request::LlmRequest;
use crate::{ProviderAuthFieldType, ProviderConfig};

const OPENAI_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const OPENAI_CODEX_RESPONSES_PATH: &str = "/responses";

macro_rules! define_aisdk_provider {
    ($struct_name:ident, $sdk_ty:ty, $provider_slug:literal, $api_key_env:literal) => {
        pub struct $struct_name {
            inner: AisdkLanguageModelProvider<$sdk_ty>,
        }

        impl $struct_name {
            pub const PROVIDER: &'static str = $provider_slug;
            pub const API_KEY_ENV: &'static str = $api_key_env;

            pub fn new(api_key: Option<String>, base_url: Option<String>, model: String) -> Self {
                Self::with_provider_name(Self::PROVIDER, api_key, base_url, model)
            }

            pub fn from_config(config: &ProviderConfig) -> Self {
                Self::with_provider_name(
                    &config.name,
                    config.api_key.clone(),
                    config.base_url.clone(),
                    config.model.clone(),
                )
            }

            pub fn with_provider_name(
                provider_name: &str,
                api_key: Option<String>,
                base_url: Option<String>,
                model: String,
            ) -> Self {
                let env_api_key = std::env::var(Self::API_KEY_ENV).ok();
                let api_key_source = match api_key.as_deref() {
                    Some(key) if !key.trim().is_empty() => "config",
                    Some(_) => "config-empty",
                    None if env_api_key.as_deref().is_some_and(|key| !key.trim().is_empty()) => "env",
                    None => "none",
                };
                let resolved_api_key = api_key.or(env_api_key);
                let has_api_key = resolved_api_key.as_deref().is_some_and(|key| !key.trim().is_empty());
                let has_base_url = base_url.as_deref().is_some_and(|url| !url.trim().is_empty());
                if has_api_key {
                    tracing::debug!(
                        provider = %provider_name,
                        sdk_provider = Self::PROVIDER,
                        api_key_env = Self::API_KEY_ENV,
                        api_key_source,
                        has_api_key,
                        has_base_url,
                        model = %model,
                        "selected native AISDK provider auth config"
                    );
                } else {
                    tracing::trace!(
                        provider = %provider_name,
                        sdk_provider = Self::PROVIDER,
                        api_key_env = Self::API_KEY_ENV,
                        api_key_source,
                        has_api_key,
                        has_base_url,
                        model = %model,
                        "selected native AISDK provider auth config"
                    );
                }
                let builder_provider = provider_name.to_string();
                let builder_key = resolved_api_key.clone();
                let builder_base_url = base_url.clone();

                Self {
                    inner: AisdkLanguageModelProvider::<$sdk_ty>::new(
                        provider_name,
                        Self::PROVIDER,
                        Some(Self::API_KEY_ENV),
                        resolved_api_key,
                        api_key_source,
                        base_url,
                        model,
                        move |model| {
                            let mut builder = <$sdk_ty>::builder()
                                .provider_name(builder_provider.clone())
                                .model_name(model.to_string());
                            if let Some(api_key) = builder_key.as_deref() {
                                builder = builder.api_key(api_key.to_string());
                            }
                            if let Some(base_url) = builder_base_url.as_deref() {
                                builder = builder.base_url(base_url.to_string());
                            }
                            builder.build().map_err(|error| {
                                LlmError::new("sdk_config", &error.to_string())
                                    .provider(&builder_provider)
                            })
                        },
                    ),
                }
            }
        }

        #[async_trait]
        impl LlmProvider for $struct_name {
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
    };
}

define_aisdk_provider!(
    Ai302Provider,
    aisdk::providers::Ai302<DynamicModel>,
    "302ai",
    "AI302_API_KEY"
);
define_aisdk_provider!(
    AbacusProvider,
    aisdk::providers::Abacus<DynamicModel>,
    "abacus",
    "ABACUS_API_KEY"
);
define_aisdk_provider!(
    AihubmixProvider,
    aisdk::providers::Aihubmix<DynamicModel>,
    "aihubmix",
    "AIHUBMIX_API_KEY"
);
define_aisdk_provider!(
    AlibabaProvider,
    aisdk::providers::Alibaba<DynamicModel>,
    "alibaba",
    "DASHSCOPE_API_KEY"
);
define_aisdk_provider!(
    AlibabaCnProvider,
    aisdk::providers::AlibabaCn<DynamicModel>,
    "alibaba-cn",
    "DASHSCOPE_API_KEY"
);
define_aisdk_provider!(
    AmazonBedrockProvider,
    aisdk::providers::AmazonBedrock<DynamicModel>,
    "amazon-bedrock",
    "BEDROCK_API_KEY"
);
define_aisdk_provider!(
    AnthropicAisdkProvider,
    aisdk::providers::Anthropic<DynamicModel>,
    "anthropic",
    "ANTHROPIC_API_KEY"
);
define_aisdk_provider!(
    BailingProvider,
    aisdk::providers::Bailing<DynamicModel>,
    "bailing",
    "BAILING_API_KEY"
);
define_aisdk_provider!(
    BasetenProvider,
    aisdk::providers::Baseten<DynamicModel>,
    "baseten",
    "BASETEN_API_KEY"
);
define_aisdk_provider!(
    BergetProvider,
    aisdk::providers::Berget<DynamicModel>,
    "berget",
    "BERGET_API_KEY"
);
define_aisdk_provider!(
    ChutesProvider,
    aisdk::providers::Chutes<DynamicModel>,
    "chutes",
    "CHUTES_API_KEY"
);
define_aisdk_provider!(
    CloudflareAiGatewayProvider,
    aisdk::providers::CloudflareAiGateway<DynamicModel>,
    "cloudflare-ai-gateway",
    "CLOUDFLARE_API_KEY"
);
define_aisdk_provider!(
    CloudflareWorkersAiProvider,
    aisdk::providers::CloudflareWorkersAi<DynamicModel>,
    "cloudflare-workers-ai",
    "CLOUDFLARE_API_KEY"
);
define_aisdk_provider!(
    CortecsProvider,
    aisdk::providers::Cortecs<DynamicModel>,
    "cortecs",
    "CORTECS_API_KEY"
);
define_aisdk_provider!(
    DeepseekProvider,
    aisdk::providers::Deepseek<DynamicModel>,
    "deepseek",
    "DEEPSEEK_API_KEY"
);
define_aisdk_provider!(
    FastrouterProvider,
    aisdk::providers::Fastrouter<DynamicModel>,
    "fastrouter",
    "FASTROUTER_API_KEY"
);
define_aisdk_provider!(
    FireworksAiProvider,
    aisdk::providers::FireworksAi<DynamicModel>,
    "fireworks-ai",
    "FIREWORKS_API_KEY"
);
define_aisdk_provider!(
    FirmwareProvider,
    aisdk::providers::Firmware<DynamicModel>,
    "firmware",
    "FIRMWARE_API_KEY"
);
define_aisdk_provider!(
    FriendliProvider,
    aisdk::providers::Friendli<DynamicModel>,
    "friendli",
    "FRIENDLI_TOKEN"
);
define_aisdk_provider!(
    GithubCopilotProvider,
    aisdk::providers::GithubCopilot<DynamicModel>,
    "github-copilot",
    "GITHUB_COPILOT_API_KEY"
);
define_aisdk_provider!(
    GithubModelsProvider,
    aisdk::providers::GithubModels<DynamicModel>,
    "github-models",
    "GITHUB_TOKEN"
);
define_aisdk_provider!(
    GoogleProvider,
    aisdk::providers::Google<DynamicModel>,
    "google",
    "GOOGLE_GENERATIVE_AI_API_KEY"
);
define_aisdk_provider!(
    GroqProvider,
    aisdk::providers::Groq<DynamicModel>,
    "groq",
    "GROQ_API_KEY"
);
define_aisdk_provider!(
    HeliconeProvider,
    aisdk::providers::Helicone<DynamicModel>,
    "helicone",
    "HELICONE_API_KEY"
);
define_aisdk_provider!(
    HuggingfaceProvider,
    aisdk::providers::Huggingface<DynamicModel>,
    "huggingface",
    "HUGGINGFACE_API_KEY"
);
define_aisdk_provider!(
    IflowcnProvider,
    aisdk::providers::Iflowcn<DynamicModel>,
    "iflowcn",
    "IFLOW_API_KEY"
);
define_aisdk_provider!(
    InceptionProvider,
    aisdk::providers::Inception<DynamicModel>,
    "inception",
    "INCEPTION_API_KEY"
);
define_aisdk_provider!(
    InferenceProvider,
    aisdk::providers::Inference<DynamicModel>,
    "inference",
    "INFERENCE_API_KEY"
);
define_aisdk_provider!(
    IoNetProvider,
    aisdk::providers::IoNet<DynamicModel>,
    "io-net",
    "IONET_API_KEY"
);
define_aisdk_provider!(
    JiekouProvider,
    aisdk::providers::Jiekou<DynamicModel>,
    "jiekou",
    "JIEKOU_API_KEY"
);
define_aisdk_provider!(
    KuaeCloudCodingPlanProvider,
    aisdk::providers::KuaeCloudCodingPlan<DynamicModel>,
    "kuae-cloud-coding-plan",
    "KUAE_API_KEY"
);
define_aisdk_provider!(
    LlamaProvider,
    aisdk::providers::Llama<DynamicModel>,
    "llama",
    "LLAMA_API_KEY"
);
define_aisdk_provider!(
    LmstudioProvider,
    aisdk::providers::Lmstudio<DynamicModel>,
    "lmstudio",
    "LMSTUDIO_API_KEY"
);
define_aisdk_provider!(
    LucidqueryProvider,
    aisdk::providers::Lucidquery<DynamicModel>,
    "lucidquery",
    "LUCIDQUERY_API_KEY"
);
define_aisdk_provider!(
    MistralProvider,
    aisdk::providers::Mistral<DynamicModel>,
    "mistral",
    "MISTRAL_API_KEY"
);
define_aisdk_provider!(
    MoarkProvider,
    aisdk::providers::Moark<DynamicModel>,
    "moark",
    "MOARK_API_KEY"
);
define_aisdk_provider!(
    ModelscopeProvider,
    aisdk::providers::Modelscope<DynamicModel>,
    "modelscope",
    "MODELSCOPE_API_KEY"
);
define_aisdk_provider!(
    MoonshotaiProvider,
    aisdk::providers::Moonshotai<DynamicModel>,
    "moonshotai",
    "MOONSHOT_API_KEY"
);
define_aisdk_provider!(
    MoonshotaiCnProvider,
    aisdk::providers::MoonshotaiCn<DynamicModel>,
    "moonshotai-cn",
    "MOONSHOT_API_KEY"
);
define_aisdk_provider!(
    MorphProvider,
    aisdk::providers::Morph<DynamicModel>,
    "morph",
    "MORPH_API_KEY"
);
define_aisdk_provider!(
    NanoGptProvider,
    aisdk::providers::NanoGpt<DynamicModel>,
    "nano-gpt",
    "NANOGPT_API_KEY"
);
define_aisdk_provider!(
    NebiusProvider,
    aisdk::providers::Nebius<DynamicModel>,
    "nebius",
    "NEBIUS_API_KEY"
);
define_aisdk_provider!(
    NovaProvider,
    aisdk::providers::Nova<DynamicModel>,
    "nova",
    "NOVA_API_KEY"
);
define_aisdk_provider!(
    NovitaAiProvider,
    aisdk::providers::NovitaAi<DynamicModel>,
    "novita-ai",
    "NOVITA_API_KEY"
);
define_aisdk_provider!(
    NvidiaProvider,
    aisdk::providers::Nvidia<DynamicModel>,
    "nvidia",
    "NVIDIA_API_KEY"
);
define_aisdk_provider!(
    OllamaCloudProvider,
    aisdk::providers::OllamaCloud<DynamicModel>,
    "ollama-cloud",
    "OLLAMA_API_KEY"
);

pub struct OpenAiAisdkProvider {
    inner: AisdkLanguageModelProvider<aisdk::providers::OpenAI<DynamicModel>>,
    use_codex_oauth_endpoint: bool,
}

impl OpenAiAisdkProvider {
    pub const PROVIDER: &'static str = "openai";
    pub const API_KEY_ENV: &'static str = "OPENAI_API_KEY";

    pub fn from_config(config: &ProviderConfig) -> Self {
        Self::with_provider_name(
            &config.name,
            config.api_key.clone(),
            config.base_url.clone(),
            config.model.clone(),
            config.auth_field_type == ProviderAuthFieldType::OAuth,
            config.auth_account_id.clone(),
        )
    }

    pub fn with_provider_name(
        provider_name: &str,
        api_key: Option<String>,
        base_url: Option<String>,
        model: String,
        use_codex_oauth_endpoint: bool,
        account_id: Option<String>,
    ) -> Self {
        let env_api_key = std::env::var(Self::API_KEY_ENV).ok();
        let api_key_source = match api_key.as_deref() {
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
        let resolved_api_key = api_key.or(env_api_key);
        let configured_base_url = base_url;
        let effective_base_url = if use_codex_oauth_endpoint {
            let mut extra_headers = vec![
                ("originator".to_string(), "openman".to_string()),
                (
                    "User-Agent".to_string(),
                    format!("openman/{}", env!("CARGO_PKG_VERSION")),
                ),
            ];
            if let Some(account_id) = account_id
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                extra_headers.push((
                    "ChatGPT-Account-Id".to_string(),
                    account_id.trim().to_string(),
                ));
            }
            match sse_proxy::global_manager().ensure_for_provider_with_options(
                provider_name,
                OPENAI_CODEX_BASE_URL,
                sse_proxy::SseProxyOptions {
                    extra_headers,
                    require_sse_restructure: true,
                },
            ) {
                Ok((local_base_url, _instance)) => Some(local_base_url),
                Err(error) => {
                    tracing::warn!(
                        provider = %provider_name,
                        error = %error,
                        "failed to start OpenAI Codex SSE proxy; falling back to direct Codex endpoint"
                    );
                    Some(OPENAI_CODEX_BASE_URL.to_string())
                }
            }
        } else if sse_proxy::SseProxyManager::env_enabled(provider_name) {
            let upstream_base_url = configured_base_url
                .clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
            match sse_proxy::global_manager().ensure_for_provider(provider_name, &upstream_base_url)
            {
                Ok((local_base_url, _instance)) => Some(local_base_url),
                Err(error) => {
                    tracing::warn!(
                        provider = %provider_name,
                        upstream_base_url = %upstream_base_url,
                        error = %error,
                        "failed to start OpenAI SSE proxy; falling back to direct endpoint"
                    );
                    Some(upstream_base_url)
                }
            }
        } else {
            configured_base_url
        };
        let effective_path = use_codex_oauth_endpoint.then_some(OPENAI_CODEX_RESPONSES_PATH);
        let has_api_key = resolved_api_key
            .as_deref()
            .is_some_and(|key| !key.trim().is_empty());
        let has_base_url = effective_base_url
            .as_deref()
            .is_some_and(|url| !url.trim().is_empty());
        if has_api_key {
            tracing::debug!(
                provider = %provider_name,
                sdk_provider = Self::PROVIDER,
                api_key_env = Self::API_KEY_ENV,
                api_key_source,
                has_api_key,
                has_base_url,
                openai_oauth_codex_endpoint = use_codex_oauth_endpoint,
                has_account_id = account_id.as_deref().is_some_and(|value| !value.trim().is_empty()),
                model = %model,
                "selected native AISDK provider auth config"
            );
        } else {
            tracing::trace!(
                provider = %provider_name,
                sdk_provider = Self::PROVIDER,
                api_key_env = Self::API_KEY_ENV,
                api_key_source,
                has_api_key,
                has_base_url,
                openai_oauth_codex_endpoint = use_codex_oauth_endpoint,
                has_account_id = account_id.as_deref().is_some_and(|value| !value.trim().is_empty()),
                model = %model,
                "selected native AISDK provider auth config"
            );
        }

        let builder_provider = provider_name.to_string();
        let builder_key = resolved_api_key.clone();
        let builder_base_url = effective_base_url.clone();

        Self {
            inner: AisdkLanguageModelProvider::<aisdk::providers::OpenAI<DynamicModel>>::new(
                provider_name,
                Self::PROVIDER,
                Some(Self::API_KEY_ENV),
                resolved_api_key,
                api_key_source,
                effective_base_url,
                model,
                move |model| {
                    let mut builder = aisdk::providers::OpenAI::<DynamicModel>::builder()
                        .provider_name(builder_provider.clone())
                        .model_name(model.to_string());
                    if let Some(api_key) = builder_key.as_deref() {
                        builder = builder.api_key(api_key.to_string());
                    }
                    if let Some(base_url) = builder_base_url.as_deref() {
                        builder = builder.base_url(base_url.to_string());
                    }
                    if let Some(path) = effective_path {
                        builder = builder.path(path.to_string());
                    }
                    builder.build().map_err(|error| {
                        LlmError::new("sdk_config", &error.to_string()).provider(&builder_provider)
                    })
                },
            ),
            use_codex_oauth_endpoint,
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiAisdkProvider {
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

    fn uses_codex_oauth_endpoint(&self) -> bool {
        self.use_codex_oauth_endpoint
    }
}

define_aisdk_provider!(
    OpenAiCompatibleAisdkProvider,
    aisdk::providers::OpenAICompatible<DynamicModel>,
    "openaicompatible",
    "OPENAI_API_KEY"
);
define_aisdk_provider!(
    OpencodeProvider,
    aisdk::providers::Opencode<DynamicModel>,
    "opencode",
    "OPENCODE_API_KEY"
);
define_aisdk_provider!(
    OpenrouterProvider,
    aisdk::providers::Openrouter<DynamicModel>,
    "openrouter",
    "OPENROUTER_API_KEY"
);
define_aisdk_provider!(
    OvhcloudProvider,
    aisdk::providers::Ovhcloud<DynamicModel>,
    "ovhcloud",
    "OVHCLOUD_API_KEY"
);
define_aisdk_provider!(
    PoeProvider,
    aisdk::providers::Poe<DynamicModel>,
    "poe",
    "POE_API_KEY"
);
define_aisdk_provider!(
    RequestyProvider,
    aisdk::providers::Requesty<DynamicModel>,
    "requesty",
    "REQUESTY_API_KEY"
);
define_aisdk_provider!(
    ScalewayProvider,
    aisdk::providers::Scaleway<DynamicModel>,
    "scaleway",
    "SCALEWAY_API_KEY"
);
define_aisdk_provider!(
    SiliconflowProvider,
    aisdk::providers::Siliconflow<DynamicModel>,
    "siliconflow",
    "SILICONFLOW_API_KEY"
);
define_aisdk_provider!(
    SiliconflowCnProvider,
    aisdk::providers::SiliconflowCn<DynamicModel>,
    "siliconflow-cn",
    "SILICONFLOW_API_KEY"
);
define_aisdk_provider!(
    StackitProvider,
    aisdk::providers::Stackit<DynamicModel>,
    "stackit",
    "STACKIT_API_KEY"
);
define_aisdk_provider!(
    StepfunProvider,
    aisdk::providers::Stepfun<DynamicModel>,
    "stepfun",
    "STEPFUN_API_KEY"
);
define_aisdk_provider!(
    SubmodelProvider,
    aisdk::providers::Submodel<DynamicModel>,
    "submodel",
    "SUBMODEL_API_KEY"
);
define_aisdk_provider!(
    SyntheticProvider,
    aisdk::providers::Synthetic<DynamicModel>,
    "synthetic",
    "SYNTHETIC_API_KEY"
);
define_aisdk_provider!(
    TogetherAiProvider,
    aisdk::providers::TogetherAI<DynamicModel>,
    "togetherai",
    "TOGETHER_API_KEY"
);
define_aisdk_provider!(
    UpstageProvider,
    aisdk::providers::Upstage<DynamicModel>,
    "upstage",
    "UPSTAGE_API_KEY"
);
define_aisdk_provider!(
    VercelProvider,
    aisdk::providers::Vercel<DynamicModel>,
    "vercel",
    "VERCEL_API_KEY"
);
define_aisdk_provider!(
    VultrProvider,
    aisdk::providers::Vultr<DynamicModel>,
    "vultr",
    "VULTR_API_KEY"
);
define_aisdk_provider!(
    WandbProvider,
    aisdk::providers::Wandb<DynamicModel>,
    "wandb",
    "WANDB_API_KEY"
);
define_aisdk_provider!(
    XaiProvider,
    aisdk::providers::XAI<DynamicModel>,
    "xai",
    "XAI_API_KEY"
);
define_aisdk_provider!(
    XiaomiProvider,
    aisdk::providers::Xiaomi<DynamicModel>,
    "xiaomi",
    "XIAOMI_API_KEY"
);
define_aisdk_provider!(
    ZaiProvider,
    aisdk::providers::Zai<DynamicModel>,
    "zai",
    "ZHIPU_API_KEY"
);
define_aisdk_provider!(
    ZaiCodingPlanProvider,
    aisdk::providers::ZaiCodingPlan<DynamicModel>,
    "zai-coding-plan",
    "ZHIPU_API_KEY"
);
define_aisdk_provider!(
    ZenmuxProvider,
    aisdk::providers::Zenmux<DynamicModel>,
    "zenmux",
    "ZENMUX_API_KEY"
);
define_aisdk_provider!(
    ZhipuaiProvider,
    aisdk::providers::Zhipuai<DynamicModel>,
    "zhipuai",
    "ZHIPU_API_KEY"
);
define_aisdk_provider!(
    ZhipuaiCodingPlanProvider,
    aisdk::providers::ZhipuaiCodingPlan<DynamicModel>,
    "zhipuai-coding-plan",
    "ZHIPU_API_KEY"
);

pub fn provider_from_config(config: &ProviderConfig) -> Option<Arc<dyn LlmProvider>> {
    let normalized = config.name.trim().to_lowercase().replace('_', "-");
    let provider: Arc<dyn LlmProvider> = match normalized.as_str() {
        "302ai" => Arc::new(Ai302Provider::from_config(config)),
        "abacus" => Arc::new(AbacusProvider::from_config(config)),
        "aihubmix" => Arc::new(AihubmixProvider::from_config(config)),
        "alibaba" => Arc::new(AlibabaProvider::from_config(config)),
        "alibaba-cn" => Arc::new(AlibabaCnProvider::from_config(config)),
        "amazon-bedrock" => Arc::new(AmazonBedrockProvider::from_config(config)),
        "anthropic" => Arc::new(AnthropicAisdkProvider::from_config(config)),
        "bailing" => Arc::new(BailingProvider::from_config(config)),
        "baseten" => Arc::new(BasetenProvider::from_config(config)),
        "berget" => Arc::new(BergetProvider::from_config(config)),
        "chutes" => Arc::new(ChutesProvider::from_config(config)),
        "cloudflare-ai-gateway" => Arc::new(CloudflareAiGatewayProvider::from_config(config)),
        "cloudflare-workers-ai" => Arc::new(CloudflareWorkersAiProvider::from_config(config)),
        "cortecs" => Arc::new(CortecsProvider::from_config(config)),
        "deepseek" => Arc::new(DeepseekProvider::from_config(config)),
        "fastrouter" => Arc::new(FastrouterProvider::from_config(config)),
        "fireworks-ai" => Arc::new(FireworksAiProvider::from_config(config)),
        "firmware" => Arc::new(FirmwareProvider::from_config(config)),
        "friendli" => Arc::new(FriendliProvider::from_config(config)),
        "github-copilot" => Arc::new(GithubCopilotProvider::from_config(config)),
        "github-models" => Arc::new(GithubModelsProvider::from_config(config)),
        "google" => Arc::new(GoogleProvider::from_config(config)),
        "groq" => Arc::new(GroqProvider::from_config(config)),
        "helicone" => Arc::new(HeliconeProvider::from_config(config)),
        "huggingface" => Arc::new(HuggingfaceProvider::from_config(config)),
        "iflowcn" => Arc::new(IflowcnProvider::from_config(config)),
        "inception" => Arc::new(InceptionProvider::from_config(config)),
        "inference" => Arc::new(InferenceProvider::from_config(config)),
        "io-net" => Arc::new(IoNetProvider::from_config(config)),
        "jiekou" => Arc::new(JiekouProvider::from_config(config)),
        "kuae-cloud-coding-plan" => Arc::new(KuaeCloudCodingPlanProvider::from_config(config)),
        "llama" => Arc::new(LlamaProvider::from_config(config)),
        "lmstudio" => Arc::new(LmstudioProvider::from_config(config)),
        "lucidquery" => Arc::new(LucidqueryProvider::from_config(config)),
        "mistral" => Arc::new(MistralProvider::from_config(config)),
        "moark" => Arc::new(MoarkProvider::from_config(config)),
        "modelscope" => Arc::new(ModelscopeProvider::from_config(config)),
        "moonshotai" => Arc::new(MoonshotaiProvider::from_config(config)),
        "moonshotai-cn" => Arc::new(MoonshotaiCnProvider::from_config(config)),
        "morph" => Arc::new(MorphProvider::from_config(config)),
        "nano-gpt" => Arc::new(NanoGptProvider::from_config(config)),
        "nebius" => Arc::new(NebiusProvider::from_config(config)),
        "nova" => Arc::new(NovaProvider::from_config(config)),
        "novita-ai" => Arc::new(NovitaAiProvider::from_config(config)),
        "nvidia" => Arc::new(NvidiaProvider::from_config(config)),
        "ollama-cloud" => Arc::new(OllamaCloudProvider::from_config(config)),
        "openai" => Arc::new(OpenAiAisdkProvider::from_config(config)),
        "openaicompatible" | "openai-compatible" => {
            Arc::new(OpenAiCompatibleAisdkProvider::from_config(config))
        }
        "opencode" => Arc::new(OpencodeProvider::from_config(config)),
        "openrouter" => Arc::new(OpenrouterProvider::from_config(config)),
        "ovhcloud" => Arc::new(OvhcloudProvider::from_config(config)),
        "poe" => Arc::new(PoeProvider::from_config(config)),
        "requesty" => Arc::new(RequestyProvider::from_config(config)),
        "scaleway" => Arc::new(ScalewayProvider::from_config(config)),
        "siliconflow" => Arc::new(SiliconflowProvider::from_config(config)),
        "siliconflow-cn" => Arc::new(SiliconflowCnProvider::from_config(config)),
        "stackit" => Arc::new(StackitProvider::from_config(config)),
        "stepfun" => Arc::new(StepfunProvider::from_config(config)),
        "submodel" => Arc::new(SubmodelProvider::from_config(config)),
        "synthetic" => Arc::new(SyntheticProvider::from_config(config)),
        "togetherai" => Arc::new(TogetherAiProvider::from_config(config)),
        "upstage" => Arc::new(UpstageProvider::from_config(config)),
        "vercel" => Arc::new(VercelProvider::from_config(config)),
        "vultr" => Arc::new(VultrProvider::from_config(config)),
        "wandb" => Arc::new(WandbProvider::from_config(config)),
        "xai" => Arc::new(XaiProvider::from_config(config)),
        "xiaomi" => Arc::new(XiaomiProvider::from_config(config)),
        "zai" => Arc::new(ZaiProvider::from_config(config)),
        "zai-coding-plan" => Arc::new(ZaiCodingPlanProvider::from_config(config)),
        "zenmux" => Arc::new(ZenmuxProvider::from_config(config)),
        "zhipuai" => Arc::new(ZhipuaiProvider::from_config(config)),
        "zhipuai-coding-plan" => Arc::new(ZhipuaiCodingPlanProvider::from_config(config)),
        _ => return None,
    };
    Some(provider)
}
