use async_trait::async_trait;
use futures_util::StreamExt;
use std::sync::{Arc, Mutex};

use aisdk::core::language_model::{LanguageModelResponseContentType, Step, StopReason};
use aisdk::core::{
    AssistantMessage, DynamicModel, LanguageModelRequest, LanguageModelStreamChunkType, Message,
    ToolCallInfo, ToolResultInfo, UserMessage,
};
use aisdk::providers::OpenAICompatible;

use super::error_parsing::{format_provider_error, parse_provider_error_body};
use super::tool_registry::build_sdk_tool;
use super::{LlmError, LlmProvider, LlmStream, ToolDispatcherFn};
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
    api_key_source: &'static str,
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
        let env_api_key = std::env::var(api_key_env).ok();
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
        let api_key = api_key.or(env_api_key);
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
        let has_api_key = api_key.as_deref().is_some_and(|key| !key.trim().is_empty());
        if has_api_key {
            tracing::debug!(
                provider = %provider_name,
                base_url = %base_url,
                api_key_env = %api_key_env,
                api_key_source,
                has_api_key,
                "created OpenAI-compatible SDK provider auth config"
            );
        } else {
            tracing::trace!(
                provider = %provider_name,
                base_url = %base_url,
                api_key_env = %api_key_env,
                api_key_source,
                has_api_key,
                "created OpenAI-compatible SDK provider auth config"
            );
        }

        Self {
            provider_name: provider_name.to_string(),
            api_key_env: api_key_env.to_string(),
            api_key,
            api_key_source,
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
            api_key_source = self.api_key_source,
            tool_count = request.tools.len(),
            tool_dispatcher_wired = self
                .tool_dispatcher
                .lock()
                .map(|guard| guard.is_some())
                .unwrap_or(false),
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
            // Stop after one tool-call round so the runner drives the
            // next round-trip. The runner already persists `ToolCall`
            // and `ToolResult` parts into the assistant message and
            // re-issues the request with the updated history; letting
            // the SDK drive its own loop means transient mid-stream
            // drops inside the SDK's internal follow-up request fail
            // the whole turn, whereas a per-round runner step can be
            // retried by the user.
            builder = builder.stop_when(|options| {
                options
                    .last_step()
                    .is_some_and(|step| sdk_generated_step_has_tool_calls(&step))
            });
        }

        let mut sdk_request = builder.build();
        let response = stream_text_with_retry(&mut sdk_request)
            .await
            .map_err(|error| {
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
                        tracing::warn!(
                            provider = %stream_provider,
                            model = %stream_model,
                            error = %message,
                            "sdk llm stream chunk: failed"
                        );
                        yield LlmEvent::ProviderError { message };
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
                    LanguageModelStreamChunkType::NotSupported(message) => {
                        saw_provider_error = true;
                        tracing::warn!(
                            provider = %stream_provider,
                            model = %stream_model,
                            error = %message,
                            "sdk llm stream chunk: not_supported"
                        );
                        yield LlmEvent::ProviderError { message };
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

            // The SDK's `handle_tool_call` already ran the
            // executor and inserted the `Message::Tool` for
            // every resolved call. We just need to surface the
            // resolved calls and results as `LlmEvent`s so the
            // runner-side persistence block can keep building
            // the assistant message the same way it always has.
            let (tool_calls, tool_results) = sdk_generated_step_tool_activity(response.last_step().await);
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
                tool_results = tool_results.len(),
                "sdk llm stream finished after stream exhaustion"
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

fn sdk_generated_step_has_tool_calls(step: &Step) -> bool {
    step.step_id > 0 && step.tool_calls().is_some_and(|calls| !calls.is_empty())
}

fn sdk_generated_step_tool_activity(
    step: Option<Step>,
) -> (Vec<ToolCallInfo>, Vec<ToolResultInfo>) {
    let Some(step) = step else {
        return (Vec::new(), Vec::new());
    };

    if step.step_id == 0 {
        return (Vec::new(), Vec::new());
    }

    (
        step.tool_calls().unwrap_or_default(),
        step.tool_results().unwrap_or_default(),
    )
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
            serde_json::Value::String(text) => {
                match serde_json::from_str::<serde_json::Value>(text) {
                    Ok(parsed) => ToolResultValue::Json { value: parsed },
                    Err(_) => ToolResultValue::Text {
                        value: text.clone(),
                    },
                }
            }
            other => ToolResultValue::Json {
                value: other.clone(),
            },
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
    use crate::llm::request::LlmMessage;
    use aisdk::core::tools::ToolDetails;

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
    fn sdk_generated_step_tool_activity_ignores_initial_history_step() {
        let mut call = ToolCallInfo::new("read");
        call.id("call_history");
        call.input(serde_json::json!({"path":"README.md"}));

        let step = Step::new(
            0,
            vec![
                Message::Assistant(AssistantMessage::new(
                    LanguageModelResponseContentType::ToolCall(call),
                    None,
                )),
                Message::Tool(sdk_tool_result_info(
                    "call_history",
                    "read",
                    &serde_json::json!({"text":"history"}),
                )),
            ],
        );

        assert!(!sdk_generated_step_has_tool_calls(&step));
        let (calls, results) = sdk_generated_step_tool_activity(Some(step));
        assert!(calls.is_empty());
        assert!(results.is_empty());
    }

    #[test]
    fn sdk_generated_step_tool_activity_keeps_generated_step() {
        let mut call = ToolCallInfo::new("glob");
        call.id("call_new");
        call.input(serde_json::json!({"path":"src"}));

        let step = Step::new(
            1,
            vec![
                Message::Assistant(AssistantMessage::new(
                    LanguageModelResponseContentType::ToolCall(call),
                    None,
                )),
                Message::Tool(sdk_tool_result_info(
                    "call_new",
                    "glob",
                    &serde_json::json!({"matches":[]}),
                )),
            ],
        );

        assert!(sdk_generated_step_has_tool_calls(&step));
        let (calls, results) = sdk_generated_step_tool_activity(Some(step));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool.id, "call_new");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tool.id, "call_new");
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
            panic!(
                "tool result must lower to Message::Tool, got: {:?}",
                messages[1]
            );
        };
        assert_eq!(tool_result.tool.id, "call_1");
        assert_eq!(tool_result.tool.name, "glob");
        let output = tool_result.output.as_ref().expect("tool output");
        assert_eq!(
            output,
            &serde_json::json!({"error": "user rejected request abc"})
        );
    }

    #[test]
    fn sdk_assistant_messages_pair_each_tool_call_before_its_result() {
        let content = vec![
            ContentPart::tool_call("call_1", "read", serde_json::json!({"path": "README.md"})),
            ContentPart::tool_call("call_2", "glob", serde_json::json!({"path": "/tmp"})),
            ContentPart::tool_result("call_1", "read", serde_json::json!({"text": "readme"})),
            ContentPart::tool_result("call_2", "glob", serde_json::json!({"text": "README.md"})),
        ];

        let messages = sdk_assistant_messages(&content);
        assert_eq!(messages.len(), 4);
        assert_tool_call_message(&messages[0], "call_1");
        assert_tool_result_message(&messages[1], "call_1");
        assert_tool_call_message(&messages[2], "call_2");
        assert_tool_result_message(&messages[3], "call_2");
    }

    #[test]
    fn sdk_wire_shape_log_uses_actual_tool_role_messages() {
        let request = LlmRequest::new("gpt-4o-mini", "openai")
            .with_message(LlmMessage::user("inspect"))
            .with_message(LlmMessage {
                role: "assistant".to_string(),
                content: vec![
                    ContentPart::tool_call("call_1", "glob", serde_json::json!({"path": "/tmp"})),
                    ContentPart::tool_result(
                        "call_1",
                        "glob",
                        serde_json::json!({"text": "Cargo.toml"}),
                    ),
                ],
            });
        let sdk_messages = sdk_messages(&request);
        let body = build_sdk_wire_shape(&request, "system", &sdk_messages);
        let messages = body["messages"].as_array().expect("messages array");

        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["tool_calls"][0]["id"], "call_1");
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call_1");
        assert_eq!(messages[2]["name"], "glob");
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
            ToolResultValue::Json { value } => {
                assert_eq!(value, serde_json::json!({"text": "hello"}));
            }
            other => panic!("expected Json variant, got {other:?}"),
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

    fn assert_tool_call_message(message: &Message, expected_id: &str) {
        let Message::Assistant(message) = message else {
            panic!("expected assistant tool-call message, got: {message:?}");
        };
        let LanguageModelResponseContentType::ToolCall(call) = &message.content else {
            panic!("expected tool-call content, got: {:?}", message.content);
        };
        assert_eq!(call.tool.id, expected_id);
    }

    fn assert_tool_result_message(message: &Message, expected_id: &str) {
        let Message::Tool(result) = message else {
            panic!("expected tool result message, got: {message:?}");
        };
        assert_eq!(result.tool.id, expected_id);
    }
}

/// Build the exact JSON shape the AI SDK's OpenAI-compatible
/// provider serializes on the wire for chat-completions. We
/// can't read the SDK's internal `options` struct, so we
/// reconstruct the body from the same SDK `Message` values,
/// system prompt, and tool definitions the SDK builder consumed. The
/// shape matches `OpenAIChatCompletionsOptions` in
/// `aisdk-0.5.2/src/providers/openai_chat_completions.rs`.
#[cfg(test)]
fn build_sdk_wire_shape(
    request: &LlmRequest,
    system: &str,
    sdk_messages: &[Message],
) -> serde_json::Value {
    let mut out_messages: Vec<serde_json::Value> = Vec::new();
    for message in sdk_messages {
        match message {
            Message::System(message) => out_messages.push(serde_json::json!({
                "role": "system",
                "content": &message.content,
            })),
            Message::User(message) => out_messages.push(serde_json::json!({
                "role": "user",
                "content": &message.content,
            })),
            Message::Assistant(message) => {
                out_messages.push(sdk_assistant_wire_message(message));
            }
            Message::Tool(tool_result) => {
                let content = tool_result
                    .output
                    .clone()
                    .unwrap_or_else(|error| serde_json::Value::String(error.to_string()))
                    .to_string();
                out_messages.push(serde_json::json!({
                    "role": "tool",
                    "content": content,
                    "name": &tool_result.tool.name,
                    "tool_call_id": &tool_result.tool.id,
                }));
            }
            Message::Developer(content) => out_messages.push(serde_json::json!({
                "role": "developer",
                "content": content,
            })),
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

#[cfg(test)]
fn sdk_assistant_wire_message(message: &AssistantMessage) -> serde_json::Value {
    match &message.content {
        LanguageModelResponseContentType::Text(text) => serde_json::json!({
            "role": "assistant",
            "content": text,
        }),
        LanguageModelResponseContentType::ToolCall(tool_info) => serde_json::json!({
            "role": "assistant",
            "content": "",
            "tool_calls": [{
                "id": &tool_info.tool.id,
                "type": "function",
                "function": {
                    "name": &tool_info.tool.name,
                    "arguments": tool_info.input.to_string(),
                },
            }],
        }),
        LanguageModelResponseContentType::Reasoning { content, .. } => serde_json::json!({
            "role": "assistant",
            "content": format!("[Reasoning]: {content}"),
        }),
        LanguageModelResponseContentType::NotSupported(_) => serde_json::json!({
            "role": "assistant",
            "content": null,
        }),
    }
}

/// Call `stream_text()` with a single retry for transient
/// mid-stream drops. AISDK surfaces a dropped connection as
/// `Error::ApiError { status_code: None, details }` (rendered as
/// `API error: None - Stream ended`); re-issuing the same request
/// recovers from a flapping gateway without bubbling the error up
/// to the user. We retry exactly once with a short backoff so a
/// sustained outage still fails fast.
async fn stream_text_with_retry(
    request: &mut LanguageModelRequest<SdkModel>,
) -> aisdk::error::Result<aisdk::core::language_model::stream_text::StreamTextResponse> {
    match request.stream_text().await {
        Ok(response) => Ok(response),
        Err(aisdk::error::Error::ApiError {
            status_code: None, ..
        }) => {
            tracing::warn!(
                "sdk stream_text returned ApiError with no status code; retrying once after 250ms"
            );
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            request.stream_text().await
        }
        Err(other) => Err(other),
    }
}
