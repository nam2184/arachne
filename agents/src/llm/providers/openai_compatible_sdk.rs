use async_trait::async_trait;
use futures_util::StreamExt;
use std::sync::{Arc, Mutex};

use aisdk::core::language_model::{LanguageModelResponseContentType, StopReason};
use aisdk::core::{
    AssistantMessage, DynamicModel, LanguageModelRequest, LanguageModelStreamChunkType, Message,
    ToolCallInfo, ToolResultInfo, UserMessage,
};
use aisdk::providers::OpenAICompatible;

use super::error_parsing::{format_provider_error, parse_provider_error_body};
use super::{LlmError, LlmProvider, LlmStream, ToolDispatcherFn};
use super::sdk_tool_registry::build_sdk_tool;
use crate::llm::events::{FinishReason, LlmEvent, ToolResultValue};
use crate::llm::request::{ContentPart, LlmRequest};

/// Harness-side tool dispatcher invoked by the AI SDK for every
/// tool call the model issues. The dispatcher wraps the runner's
/// v2 permission service, doom-loop detector, and sandboxed
/// `run_tool_*` paths so the SDK's native tool-execution loop
/// sees the same guards the hand-rolled dispatcher had.
///
/// The closure receives the AI SDK's `serde_json::Value` input
/// (already JSON-parsed from the model's tool-call arguments)
/// and returns the tool result as a `String` (the SDK wraps
/// `Ok(result)` into `Message::Tool` itself; `Err(message)` is
/// fed back to the model as a tool error).
///
/// Implementations MUST be cheap to clone (we wrap them in an
/// `Arc`) and must be safe to invoke from any thread the SDK
/// spawns tool-execution tasks on.
pub type ToolDispatcher = ToolDispatcherFn;

type SdkModel = OpenAICompatible<DynamicModel>;

pub struct OpenAiCompatibleSdkProvider {
    provider_name: String,
    api_key_env: String,
    api_key: Option<String>,
    base_url: String,
    supported_models: Vec<String>,
    sdk_model_name: String,
    sdk_model: Result<SdkModel, LlmError>,
    /// Harness-side tool dispatcher wired into every
    /// `with_tool(...)` registration. Set by the runner just
    /// before `stream()` is called via `set_tool_dispatcher`,
    /// because the runner owns the v2 permission service,
    /// doom-loop detector, and sandboxed `run_tool_*` context
    /// that the dispatcher closure needs to capture. Wrapped
    /// in a `Mutex` so the SDK provider can be reused across
    /// concurrent session runs without re-allocating the
    /// dispatcher closure.
    tool_dispatcher: Mutex<Option<Arc<ToolDispatcher>>>,
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
            tool_dispatcher: Mutex::new(None),
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

    /// Set the harness-side tool dispatcher for the next
    /// `stream()` call. The runner calls this just before
    /// dispatching each LLM request so the dispatcher's
    /// captured state (v2 permission service, doom-loop
    /// detector, sandbox context) matches the current turn.
    pub fn set_tool_dispatcher(&self, dispatcher: Arc<ToolDispatcher>) {
        let mut guard = self
            .tool_dispatcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(dispatcher);
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

    fn set_tool_dispatcher(&self, dispatcher: Arc<ToolDispatcherFn>) {
        OpenAiCompatibleSdkProvider::set_tool_dispatcher(self, dispatcher);
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
            tool_dispatcher_wired = self
                .tool_dispatcher
                .lock()
                .map(|guard| guard.is_some())
                .unwrap_or(false),
            "aisdk request starting"
        );
        log_sdk_request_structure(
            &self.provider_name,
            &request.model,
            &request,
            &system,
            &messages,
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
            // The AI SDK's native `handle_tool_call` loop runs
            // every tool the model issues through the executor
            // closure we hand it via `with_tool(...)`. The
            // harness-side dispatcher set via
            // `set_tool_dispatcher` is what keeps the v2
            // permission service, doom-loop detector, and
            // sandboxed `run_tool_*` paths on the hot path —
            // i.e. every tool call still gets the harness
            // treatment, the SDK is just doing the wire
            // round-trip on our behalf.
            let dispatcher = self
                .tool_dispatcher
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
                .ok_or_else(|| {
                    LlmError::new(
                        "sdk_no_dispatcher",
                        "OpenAiCompatibleSdkProvider requires set_tool_dispatcher(...) before stream() when tools are registered",
                    )
                    .provider(&self.provider_name)
                    .model(&request.model)
                })?;
            for tool in &request.tools {
                // Each tool is registered with its own typed
                // input struct (`JsonSchema`-derived) and the
                // harness-side dispatcher as the executor. This
                // is the AISDK `Tool::builder().name(...).description(
                // ...).input_schema(...).execute(...)` pattern from
                // the AISDK docs.
                builder = builder.with_tool(build_sdk_tool(&tool.name, dispatcher.clone())?);
            }
            // Continue the loop while the model keeps emitting
            // tool calls. The SDK runs each executor and feeds
            // the result back as `Message::Tool` automatically.
            builder = builder.stop_when(|options| options.tool_calls().is_some());
        }

        let mut sdk_request = builder.build();
        let response = sdk_request.stream_text().await.map_err(|error| {
            // The SDK wraps provider 4xx/5xx as `Error::ApiError`
            // with the response body in `details`. Parse the
            // body so the user sees `[type] message (code, param)`
            // instead of `Model streaming failed: ApiError { .. }`.
            let raw = error.to_string();
            let info = parse_provider_error_body(&raw);
            tracing::error!(
                provider = %self.provider_name,
                model = %request.model,
                error_kind = %info.kind,
                error_type = info.error_type.as_deref().unwrap_or(""),
                error_code = info.error_code.as_deref().unwrap_or(""),
                error_param = info.error_param.as_deref().unwrap_or(""),
                error_message = %info.message,
                response_body_chars = raw.chars().count(),
                "sdk_request failed: provider rejected the request"
            );
            let user_message = format_provider_error(&info, &raw);
            LlmError::new("sdk_request", &user_message)
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
                    // Tool-call argument deltas — the SDK has
                    // already buffered these into the resolved
                    // `ToolCallInfo` we surface post-stream.
                    // Surfacing raw deltas as separate
                    // `ToolInputStart/Delta/End` events is
                    // optional; the runner-side persistence
                    // doesn't need them, and downstream
                    // consumers care about the resolved call.
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

            // The SDK's `handle_tool_call` already ran the
            // executor and inserted the `Message::Tool` for
            // every resolved call. We just need to surface the
            // resolved calls and results as `LlmEvent`s so the
            // runner-side persistence block can keep building
            // the assistant message the same way it always has.
            let tool_calls = response.tool_calls().await.unwrap_or_default();
            let tool_results = response.tool_results().await.unwrap_or_default();
            for call in &tool_calls {
                for event in sdk_tool_call_events(call) {
                    yield event;
                }
            }
            for result in &tool_results {
                if let Some(event) = sdk_tool_result_event(result) {
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
                "sdk llm stream finished: provider={} model={} reason={} tool_calls={} tool_results={}",
                stream_provider,
                stream_model,
                reason,
                tool_calls.len(),
                tool_results.len(),
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
            ContentPart::ToolResult { id, name, result } => {
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
                Some(Message::Tool(sdk_tool_result_info(id, name, result)))
            }
            _ => None,
        })
        .collect()
}

fn sdk_tool_result_info(id: &str, name: &str, result: &serde_json::Value) -> ToolResultInfo {
    let mut tool_result = ToolResultInfo::new(name.to_string());
    tool_result.id(id.to_string());
    let result_text = serde_json::to_string(result).unwrap_or_else(|_| "null".to_string());
    tool_result.output(serde_json::Value::String(result_text));
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

/// Translate a resolved AI SDK `ToolResultInfo` into our
/// `LlmEvent::ToolResult`. The SDK wraps successful outputs as
/// `Ok(Value::String(text))` (see `handle_tool_call` in the
/// AI SDK's `language_model/mod.rs:282-311`) and errors as
/// `Value::String("Error: ...")`; we surface both via
/// `LlmEvent::ToolResult` with the right `ToolResultValue`
/// variant so the runner-side persistence block can keep
/// building the assistant message the way it always has.
fn sdk_tool_result_event(result: &ToolResultInfo) -> Option<LlmEvent> {
    let name = result.tool.name.clone();
    let id = result.tool.id.clone();
    let value = match &result.output {
        Ok(value) => match value {
            serde_json::Value::String(text) => ToolResultValue::Text { value: text.clone() },
            other => ToolResultValue::Json { value: other.clone() },
        },
        Err(message) => ToolResultValue::Error {
            value: format!("Error: {message}"),
        },
    };
    Some(LlmEvent::ToolResult {
        id,
        name,
        result: value,
        output: None,
    })
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
    use aisdk::core::tools::ToolDetails;
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
    fn sdk_tool_registry_builds_every_known_tool() {
        // Every tool the runner advertises to the model must
        // have a matching entry in `sdk_tool_registry` so the
        // AISDK path can register it via the `Tool::builder`
        // shape (name, description, input_schema, execute).
        // The executor closure must be the harness-side
        // dispatcher so every tool call still goes through
        // v2 perms + doom-loop + sandbox.
        let dispatcher: Arc<ToolDispatcherFn> = Arc::new(|_name, _input| Ok(String::new()));
        for name in [
            "apply_patch",
            "edit",
            "glob",
            "grep",
            "plan",
            "read",
            "shell",
            "task",
            "todo",
            "webfetch",
            "websearch",
            "write",
        ] {
            let tool = build_sdk_tool(name, dispatcher.clone())
                .unwrap_or_else(|error| panic!("failed to build tool {name}: {error}"));
            assert_eq!(tool.name, name, "tool name roundtrip");
        }

        // Unknown tool name surfaces a typed error instead of a
        // silent no-op — keeps the SDK path honest.
        let result = build_sdk_tool("does_not_exist", dispatcher);
        assert!(result.is_err());
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

    #[test]
    fn sdk_assistant_messages_lower_tool_results_as_tool_role() {
        let content = vec![
            ContentPart::tool_call("call_1", "glob", serde_json::json!({"path": "/tmp"})),
            ContentPart::tool_result(
                "call_1",
                "glob",
                serde_json::json!({"error": "user rejected request abc"}),
            ),
        ];

        let messages = sdk_assistant_messages(&content);
        assert_eq!(messages.len(), 2);
        assert!(
            matches!(messages[0], Message::Assistant(_)),
            "tool call should remain an assistant tool-call message: {:?}",
            messages
        );
        let Message::Tool(tool_result) = &messages[1] else {
            panic!("tool result must lower to Message::Tool, got: {:?}", messages[1]);
        };
        assert_eq!(tool_result.tool.id, "call_1");
        assert_eq!(tool_result.tool.name, "glob");
        let output = tool_result.output.as_ref().expect("tool output");
        assert_eq!(
            output.as_str().unwrap_or_default(),
            "{\"error\":\"user rejected request abc\"}"
        );
    }

    #[test]
    fn sdk_tool_result_event_does_not_inject_system_reminder_markup() {
        // Regression test: the SDK path's tool-result translation
        // (`sdk_tool_result_event`) must NEVER synthesize a
        // `<system-reminder>` literal in the value it surfaces
        // to the runner. The harness dispatcher returns a plain
        // JSON string (canonical result) or `Err(message)`; the
        // AI SDK wraps `Ok(s)` as `Value::String(s)` and
        // `Err(msg)` as `Value::String("Error: msg")`. Both
        // round-trip cleanly through `ToolResultValue` without
        // any markup injection.
        let ok = ToolResultInfo {
            tool: ToolDetails {
                name: "read".to_string(),
                id: "call_1".to_string(),
            },
            output: Ok(serde_json::Value::String(
                "{\"text\":\"hello\"}".to_string(),
            )),
        };
        let event = sdk_tool_result_event(&ok).expect("event");
        let LlmEvent::ToolResult { result, .. } = event else {
            panic!("expected ToolResult event");
        };
        match result {
            ToolResultValue::Text { value } => {
                assert!(!value.contains("<system-reminder>"));
                assert!(!value.contains("</system-reminder>"));
                assert_eq!(value, "{\"text\":\"hello\"}");
            }
            other => panic!("expected Text variant, got {other:?}"),
        }

        let err = ToolResultInfo {
            tool: ToolDetails {
                name: "read".to_string(),
                id: "call_1".to_string(),
            },
            output: Err(aisdk::error::Error::ToolCallError(
                "denied by v2 permission service".to_string(),
            )),
        };
        let event = sdk_tool_result_event(&err).expect("event");
        let LlmEvent::ToolResult { result, .. } = event else {
            panic!("expected ToolResult event");
        };
        match result {
            ToolResultValue::Error { value } => {
                assert!(!value.contains("<system-reminder>"));
                assert!(!value.contains("</system-reminder>"));
                // The AISDK wraps `Err(message)` as
                // `aisdk::Error::ToolCallError(message)`, which
                // `Display`s as `"Tool error: {message}"`. Our
                // translator prefixes that with `"Error: "` so
                // the runner-side persistence block gets a
                // clear, structured error string.
                assert_eq!(value, "Error: Tool error: denied by v2 permission service");
            }
            other => panic!("expected Error variant, got {other:?}"),
        }
    }

    #[test]
    fn sdk_tool_executor_returns_canonical_json_or_error_string() {
        // The harness dispatcher returns either `Ok(text)` or
        // `Err(message)` — both plain strings. The AISDK wraps
        // `Ok(s)` as `Value::String(s)` and `Err(msg)` as
        // `aisdk::Error::ToolCallError(msg)`. Neither path
        // synthesizes `<system-reminder>` markup.
        let dispatcher: Arc<ToolDispatcherFn> = Arc::new(|name, input| {
            if name == "read" && input.get("force_error").is_some() {
                Err("permission denied".to_string())
            } else {
                serde_json::to_string(&input).map_err(|err| err.to_string())
            }
        });

        let read_tool = build_sdk_tool("read", dispatcher.clone()).unwrap();
        let result = read_tool
            .execute
            .call(serde_json::json!({"path": "src/lib.rs"}));
        let output = result.expect("read should succeed");
        assert!(
            !output.contains("<system-reminder>"),
            "system-reminder leaked into executor Ok output: {output}"
        );
        assert_eq!(output, "{\"path\":\"src/lib.rs\"}");

        // Same dispatcher routed through the same `read` tool
        // but with `force_error` set — exercises the `Err` path.
        let err = read_tool
            .execute
            .call(serde_json::json!({"path": "src/lib.rs", "force_error": true}))
            .expect_err("read should error");
        let err_display = err.to_string();
        assert!(
            !err_display.contains("<system-reminder>"),
            "system-reminder leaked into executor Err output: {err_display}"
        );
        assert_eq!(err_display, "Tool error: permission denied");
    }
}

/// Debug-level dump of the SDK-bound request structure and the
/// last user-role input text. Mirrors the HTTP-side
/// `log_request_structure_and_input` so the developer gets the
/// same inspection surface on both backends. The full payload
/// lives on the AI SDK side and isn't serialized here; the
/// role/id summary is enough to confirm ordering, and the
/// truncated user input shows what the model actually sees.
///
/// We read the user/tool content straight off our own
/// `LlmRequest` instead of the SDK's `Message` enum so the log
/// doesn't depend on the SDK's internal field names (which
/// aren't visible from this crate).
fn log_sdk_request_structure(
    provider: &str,
    model: &str,
    request: &LlmRequest,
    system: &str,
    sdk_messages: &[Message],
) {
    let mut role_counts: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    let mut tool_call_ids: Vec<String> = Vec::new();
    let mut last_user_input: Option<String> = None;
    let mut last_tool_result: Option<String> = None;

    for message in &request.messages {
        *role_counts.entry(message.role.clone()).or_insert(0) += 1;
        for part in &message.content {
            if let ContentPart::ToolCall { id, .. } = part {
                tool_call_ids.push(id.clone());
            }
            if let ContentPart::ToolResult { result, .. } = part {
                last_tool_result = Some(serde_json::to_string(result).unwrap_or_default());
            }
        }
    }

    for message in request.messages.iter().rev() {
        if message.role == "user" {
            last_user_input = Some(prompt_text(&message.content));
            break;
        }
    }

    // `sdk_messages` is the same vector handed to the SDK. We log
    // its length for symmetry with the HTTP path; the structural
    // breakdown above is sourced from `request.messages` so we
    // don't depend on the SDK's internal field names.
    const INPUT_LOG_CHARS: usize = 1024;
    let last_user_input = last_user_input.map(|text| truncate_chars(&text, INPUT_LOG_CHARS));
    const TOOL_RESULT_LOG_CHARS: usize = 512;
    let last_tool_result = last_tool_result.map(|text| truncate_chars(&text, TOOL_RESULT_LOG_CHARS));

    tracing::debug!(
        provider = %provider,
        model = %model,
        role_counts = ?role_counts,
        tool_call_ids = ?tool_call_ids,
        system_chars = system.chars().count(),
        message_count = sdk_messages.len(),
        last_user_input = %last_user_input.as_deref().unwrap_or(""),
        last_user_input_chars = last_user_input.as_deref().map(str::len).unwrap_or(0),
        last_tool_result = %last_tool_result.as_deref().unwrap_or(""),
        last_tool_result_chars = last_tool_result.as_deref().map(str::len).unwrap_or(0),
        "aisdk request structure + truncated input"
    );

    // Opt-in full wire-shape dump. Set ARACHNE_DUMP_LLM=1 in the
    // environment (or `arachne_agents::llm=trace` filter) to log
    // the exact JSON body we hand to the AI SDK builder, which
    // is what the OpenAI-compatible provider serializes on the
    // wire. Lets you diff against what the provider expects
    // when a 4xx lands.
    if std::env::var_os("ARACHNE_DUMP_LLM").is_some() {
        let body = build_sdk_wire_shape(request, system);
        let body_pretty = serde_json::to_string_pretty(&body)
            .unwrap_or_else(|_| "<unserializable>".to_string());
        const MAX_BODY_BYTES: usize = 256 * 1024;
        let body_truncated = body_pretty.len() > MAX_BODY_BYTES;
        let body_display: String = if body_truncated {
            body_pretty.chars().take(MAX_BODY_BYTES).collect()
        } else {
            body_pretty
        };
        tracing::warn!(
            provider = %provider,
            model = %model,
            body_bytes = body_display.len(),
            body_truncated,
            env = "ARACHNE_DUMP_LLM=1",
            body = %body_display,
            "aisdk wire-shape body (opt-in dump; set ARACHNE_DUMP_LLM=0 to silence)"
        );
    }
}

/// Build the exact JSON shape the AI SDK's OpenAI-compatible
/// provider serializes on the wire for chat-completions. We
/// can't read the SDK's internal `options` struct, so we
/// reconstruct the body from `LlmRequest` plus the system
/// prompt and tool definitions the SDK builder consumed. The
/// shape matches `OpenAIChatCompletionsOptions` in
/// `aisdk-0.5.2/src/providers/openai_chat_completions.rs`.
fn build_sdk_wire_shape(request: &LlmRequest, system: &str) -> serde_json::Value {
    let mut out_messages: Vec<serde_json::Value> = Vec::new();
    for message in &request.messages {
        match message.role.as_str() {
            "tool" => {
                let mut entry = serde_json::Map::new();
                entry.insert(
                    "role".to_string(),
                    serde_json::Value::String("tool".to_string()),
                );
                if let Some(ContentPart::ToolResult { id, name, result }) = message
                    .content
                    .iter()
                    .find(|part| matches!(part, ContentPart::ToolResult { .. }))
                {
                    entry.insert(
                        "tool_call_id".to_string(),
                        serde_json::Value::String(id.clone()),
                    );
                    entry.insert(
                        "name".to_string(),
                        serde_json::Value::String(name.clone()),
                    );
                    entry.insert(
                        "content".to_string(),
                        serde_json::Value::String(
                            serde_json::to_string(result)
                                .unwrap_or_else(|_| "null".to_string()),
                        ),
                    );
                } else {
                    entry.insert(
                        "content".to_string(),
                        serde_json::Value::String(
                            message
                                .content
                                .iter()
                                .filter_map(ContentPart::as_prompt_text)
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ),
                    );
                }
                out_messages.push(serde_json::Value::Object(entry));
            }
            "assistant" => {
                let mut text_parts: Vec<String> = Vec::new();
                let mut tool_calls: Vec<serde_json::Value> = Vec::new();
                for part in &message.content {
                    match part {
                        ContentPart::ToolCall { id, name, input } => {
                            tool_calls.push(serde_json::json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": serde_json::to_string(input)
                                        .unwrap_or_else(|_| "null".to_string()),
                                },
                            }));
                        }
                        ContentPart::Text { text } => {
                            if !text.is_empty() {
                                text_parts.push(text.clone());
                            }
                        }
                        _ => {}
                    }
                }
                let mut entry = serde_json::Map::new();
                entry.insert(
                    "role".to_string(),
                    serde_json::Value::String("assistant".to_string()),
                );
                let text = text_parts.join("\n");
                let content_value = if text.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::String(text)
                };
                entry.insert("content".to_string(), content_value);
                if !tool_calls.is_empty() {
                    entry.insert(
                        "tool_calls".to_string(),
                        serde_json::Value::Array(tool_calls),
                    );
                }
                out_messages.push(serde_json::Value::Object(entry));
            }
            _ => {
                let text = prompt_text(&message.content);
                out_messages.push(serde_json::json!({
                    "role": message.role,
                    "content": text,
                }));
            }
        }
    }

    let tools: Vec<serde_json::Value> = request
        .tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                }
            })
        })
        .collect();

    let mut body = serde_json::Map::new();
    body.insert(
        "model".to_string(),
        serde_json::Value::String(request.model.clone()),
    );
    body.insert("stream".to_string(), serde_json::Value::Bool(true));
    body.insert(
        "messages".to_string(),
        serde_json::Value::Array(out_messages),
    );
    body.insert("tools".to_string(), serde_json::Value::Array(tools));
    if !system.is_empty() {
        body.insert(
            "system".to_string(),
            serde_json::Value::String(system.to_string()),
        );
    }
    if let Some(max_tokens) = request.max_tokens {
        body.insert(
            "max_tokens".to_string(),
            serde_json::Value::Number(max_tokens.into()),
        );
    }
    if let Some(temperature) = request.temperature {
        body.insert("temperature".to_string(), serde_json::json!(temperature));
    }
    if let Some(top_p) = request.top_p {
        body.insert("top_p".to_string(), serde_json::json!(top_p));
    }

    serde_json::Value::Object(body)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated = text.chars().take(max_chars).collect::<String>();
    truncated.push_str("…[truncated]");
    truncated
}
