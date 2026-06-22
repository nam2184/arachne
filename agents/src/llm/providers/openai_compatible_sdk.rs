use async_trait::async_trait;
use futures_util::StreamExt;
use std::sync::Arc;

use aisdk::core::language_model::{LanguageModelResponseContentType, StopReason};
use aisdk::core::tools::ToolExecute;
use aisdk::core::{
    AssistantMessage, DynamicModel, LanguageModelRequest, LanguageModelStreamChunkType, Message,
    Tool, ToolCallInfo, ToolResultInfo, UserMessage,
};
use aisdk::providers::OpenAICompatible;

use super::{LlmError, LlmProvider, LlmStream};
use crate::llm::events::{FinishReason, LlmEvent, ToolDefinition};
use crate::llm::request::{ContentPart, LlmRequest};

type SdkModel = OpenAICompatible<DynamicModel>;

pub struct OpenAiCompatibleSdkProvider {
    provider_name: String,
    api_key_env: String,
    api_key: Option<String>,
    base_url: String,
    supported_models: Vec<String>,
    sdk_model_name: String,
    sdk_model: Result<SdkModel, LlmError>,
}

impl OpenAiCompatibleSdkProvider {
    pub fn new(
        provider_name: &str,
        api_key: Option<String>,
        base_url: Option<String>,
        default_base_url: &str,
        api_key_env: &str,
        supported_models: &[&str],
    ) -> Self {
        let api_key = api_key.or_else(|| std::env::var(api_key_env).ok());
        let base_url = base_url.unwrap_or_else(|| default_base_url.to_string());
        let supported_models = supported_models
            .iter()
            .map(|model| model.to_string())
            .collect::<Vec<_>>();
        let sdk_model_name = supported_models.first().cloned().unwrap_or_default();
        let sdk_model = build_sdk_model(
            provider_name,
            api_key.as_deref(),
            &base_url,
            api_key_env,
            &sdk_model_name,
        );

        Self {
            provider_name: provider_name.to_string(),
            api_key_env: api_key_env.to_string(),
            api_key,
            base_url,
            supported_models,
            sdk_model_name,
            sdk_model,
        }
    }

    pub fn with_base_url(mut self, url: &str) -> Self {
        self.base_url = url.to_string();
        self.sdk_model = build_sdk_model(
            &self.provider_name,
            self.api_key.as_deref(),
            &self.base_url,
            &self.api_key_env,
            &self.sdk_model_name,
        );
        self
    }

    pub fn chat_completions_url(&self) -> String {
        self.base_url.trim_end_matches('/').to_string()
    }

    pub async fn endpoint_status(&self, _model: &str) -> Result<reqwest::StatusCode, LlmError> {
        Err(LlmError::new(
            "unsupported",
            "endpoint_status is only implemented for the HTTP OpenAI-compatible backend",
        )
        .provider(&self.provider_name))
    }

    fn auth_error(&self) -> LlmError {
        LlmError::new("auth", &format!("{} not set", self.api_key_env))
            .provider(&self.provider_name)
    }

    fn model_for_request(&self, model: &str) -> Result<SdkModel, LlmError> {
        if model == self.sdk_model_name {
            return self.sdk_model.clone();
        }

        build_sdk_model(
            &self.provider_name,
            self.api_key.as_deref(),
            &self.base_url,
            &self.api_key_env,
            model,
        )
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatibleSdkProvider {
    fn provider_name(&self) -> &str {
        &self.provider_name
    }

    fn supported_models(&self) -> Vec<String> {
        self.supported_models.clone()
    }

    fn backend_name(&self) -> &str {
        "sdk"
    }

    fn model_base_url(&self) -> Option<&str> {
        Some(&self.base_url)
    }

    fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    async fn stream(&self, request: LlmRequest) -> Result<LlmStream, LlmError> {
        self.api_key.as_ref().ok_or_else(|| self.auth_error())?;

        let model = self.model_for_request(&request.model)?;
        let sdk_base_url = model.settings.base_url.clone();
        let system = sdk_system_prompt(&request);
        let messages = sdk_messages(&request);
        tracing::info!(
            provider = %self.provider_name,
            model = %request.model,
            sdk_base_url = %sdk_base_url,
            has_api_key = self.api_key.as_ref().is_some_and(|api_key| !api_key.is_empty()),
            tool_count = request.tools.len(),
            "aisdk request starting"
        );
        let mut builder = if system.is_empty() {
            LanguageModelRequest::<SdkModel>::builder()
                .model(model)
                .messages(messages)
        } else {
            LanguageModelRequest::<SdkModel>::builder()
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
                            yield LlmEvent::TextDelta {
                                id: "text".to_string(),
                                text,
                            };
                        }
                    }
                    LanguageModelStreamChunkType::Reasoning(text) => {
                        if !text.is_empty() {
                            yield LlmEvent::ReasoningDelta {
                                id: "reasoning".to_string(),
                                text,
                            };
                        }
                    }
                    LanguageModelStreamChunkType::ToolCall(_) => {}
                    LanguageModelStreamChunkType::End(_) => {}
                    LanguageModelStreamChunkType::Failed(message) => {
                        saw_provider_error = true;
                        yield LlmEvent::ProviderError { message };
                    }
                    LanguageModelStreamChunkType::Incomplete(message) => {
                        if message != "Stopped by hook" {
                            saw_provider_error = true;
                            yield LlmEvent::ProviderError { message };
                        }
                    }
                    LanguageModelStreamChunkType::NotSupported(message) => {
                        saw_provider_error = true;
                        yield LlmEvent::ProviderError { message };
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
            abort_tx: None::<Arc<tokio::sync::oneshot::Sender<()>>>,
        })
    }
}

fn build_sdk_model(
    provider_name: &str,
    api_key: Option<&str>,
    base_url: &str,
    api_key_env: &str,
    model: &str,
) -> Result<SdkModel, LlmError> {
    let api_key = api_key.ok_or_else(|| {
        LlmError::new("auth", &format!("{} not set", api_key_env)).provider(provider_name)
    })?;

    let sdk_model = OpenAICompatible::<DynamicModel>::builder()
        .provider_name(provider_name)
        .base_url(base_url)
        .api_key(api_key)
        .model_name(model)
        .build()
        .map_err(|error| LlmError::new("sdk_config", &error.to_string()).provider(provider_name))?;

    log_sdk_model_details(&sdk_model, api_key, model);

    Ok(sdk_model)
}

fn log_sdk_model_details(sdk_model: &SdkModel, api_key: &str, model: &str) {
    tracing::info!(
        provider = %sdk_model.settings.provider_name,
        model = %model,
        base_url = %sdk_model.settings.base_url,
        path = ?sdk_model.settings.path,
        sdk_base_url = %sdk_model.settings.base_url,
        has_api_key = !sdk_model.settings.api_key.is_empty(),
        sdk_model = %redact_sdk_debug(&format!("{sdk_model:#?}"), api_key),
        "aisdk model built"
    );
}

fn redact_sdk_debug(debug: &str, api_key: &str) -> String {
    if api_key.is_empty() {
        debug.to_string()
    } else {
        debug.replace(api_key, "<redacted>")
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
        messages.insert(0, Message::Assistant(text_parts.join("\n").into()));
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
    use crate::llm::request::LlmMessage;

    #[test]
    fn sdk_provider_can_be_constructed() {
        let provider = OpenAiCompatibleSdkProvider::new(
            "openai",
            Some("test-key".to_string()),
            None,
            "https://api.openai.com/v1",
            "OPENAI_API_KEY",
            &["gpt-4o-mini"],
        );

        assert_eq!(provider.provider_name(), "openai");
        assert_eq!(provider.model_base_url(), Some("https://api.openai.com/v1"));
        assert_eq!(provider.api_key(), Some("test-key"));
        assert_eq!(provider.supported_models(), vec!["gpt-4o-mini".to_string()]);
        assert_eq!(provider.chat_completions_url(), "https://api.openai.com/v1");
    }

    #[test]
    fn sdk_request_mapping_splits_system_from_messages() {
        let request = LlmRequest::new("gpt-4o-mini", "openai")
            .with_system("root system")
            .with_message(LlmMessage::system("message system"))
            .with_message(LlmMessage::user("hello"));

        assert_eq!(sdk_system_prompt(&request), "root system\n\nmessage system");
        let messages = sdk_messages(&request);
        assert_eq!(messages.len(), 1);
        assert!(matches!(&messages[0], Message::User(message) if message.content == "hello"));
    }

    #[test]
    fn sdk_tool_call_events_include_final_tool_call() {
        let mut call = ToolCallInfo::new("read");
        call.id("call_1");
        call.input(serde_json::json!({"path":"src/lib.rs"}));

        let events = sdk_tool_call_events(&call);
        assert!(matches!(events[0], LlmEvent::ToolInputStart { .. }));
        assert!(matches!(events[2], LlmEvent::ToolInputEnd { .. }));
        let LlmEvent::ToolCall {
            id, name, input, ..
        } = &events[3]
        else {
            panic!("expected ToolCall, got {events:?}");
        };
        assert_eq!(id, "call_1");
        assert_eq!(name, "read");
        assert_eq!(input["path"], "src/lib.rs");
    }

    #[test]
    fn redact_sdk_debug_hides_api_key() {
        let debug = "OpenAICompatible { api_key: \"secret-key\", nested: \"secret-key\" }";
        let redacted = redact_sdk_debug(debug, "secret-key");

        assert!(!redacted.contains("secret-key"));
        assert_eq!(redacted.matches("<redacted>").count(), 2);
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
