use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::oneshot;

use super::{log_sse_event_body, LlmError, LlmProvider, LlmStream};
use crate::llm::events::{FinishReason, LlmEvent, ToolDefinition, Usage};
use crate::llm::request::{ContentPart, LlmRequest};

pub struct AnthropicProvider {
    api_key: Option<String>,
    base_url: String,
    http_client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(api_key: Option<String>, base_url: Option<String>) -> Self {
        let base_url = base_url.unwrap_or_else(|| "https://api.anthropic.com".to_string());
        Self {
            api_key: api_key.or_else(|| std::env::var("ANTHROPIC_API_KEY").ok()),
            base_url,
            http_client: reqwest::Client::builder()
                .default_headers({
                    let mut headers = reqwest::header::HeaderMap::new();
                    headers.insert(
                        "anthropic-version",
                        reqwest::header::HeaderValue::from_static("2023-06-01"),
                    );
                    headers
                })
                .build()
                .unwrap_or_default(),
        }
    }

    pub fn with_base_url(mut self, url: &str) -> Self {
        self.base_url = url.to_string();
        self
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn provider_name(&self) -> &str {
        "anthropic"
    }

    fn supported_models(&self) -> Vec<String> {
        vec![
            "claude-sonnet-4-20250514".to_string(),
            "claude-3-5-sonnet-latest".to_string(),
            "claude-3-5-haiku-latest".to_string(),
            "claude-3-opus-latest".to_string(),
            "claude-3-haiku-latest".to_string(),
        ]
    }

    fn model_base_url(&self) -> Option<&str> {
        Some(&self.base_url)
    }

    fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    async fn stream(&self, request: LlmRequest) -> Result<LlmStream, LlmError> {
        let api_key = self.api_key.as_ref().ok_or_else(|| {
            LlmError::new("auth", "ANTHROPIC_API_KEY not set").provider("anthropic")
        })?;

        let url = format!("{}/v1/messages", self.base_url);

        let (chat_messages, system) = {
            let mut system_parts: Vec<serde_json::Value> = Vec::new();
            let mut chat_messages: Vec<serde_json::Value> = Vec::new();

            for msg in &request.messages {
                match msg.role.as_str() {
                    "system" => {
                        for part in &msg.content {
                            if let ContentPart::Text { text } = part {
                                system_parts.push(serde_json::json!({
                                    "type": "text",
                                    "text": text
                                }));
                            }
                        }
                    }
                    "user" | "assistant" => {
                        let mut content_parts: Vec<serde_json::Value> = Vec::new();
                        for part in &msg.content {
                            match part {
                                ContentPart::Text { text } => {
                                    content_parts.push(serde_json::json!({
                                        "type": "text",
                                        "text": text
                                    }));
                                }
                                ContentPart::ToolCall { id, name, input } => {
                                    content_parts.push(serde_json::json!({
                                        "type": "tool_use",
                                        "id": id,
                                        "name": name,
                                        "input": input
                                    }));
                                }
                                ContentPart::ToolResult { id, result, .. } => {
                                    content_parts.push(serde_json::json!({
                                        "type": "tool_result",
                                        "tool_use_id": id,
                                        "content": result
                                    }));
                                }
                                ContentPart::Reasoning { .. } => {
                                    // Skip: reasoning is not sent
                                    // back to Anthropic in the
                                    // assistant's content.
                                }
                            }
                        }
                        chat_messages.push(serde_json::json!({
                            "role": msg.role,
                            "content": content_parts
                        }));
                    }
                    "tool" => {
                        // Anthropic-Messages carries tool results as
                        // user-role messages with `tool_result`
                        // content blocks (handled above).
                        let mut content_parts: Vec<serde_json::Value> = Vec::new();
                        for part in &msg.content {
                            if let ContentPart::ToolResult { id, result, .. } = part {
                                content_parts.push(serde_json::json!({
                                    "type": "tool_result",
                                    "tool_use_id": id,
                                    "content": result
                                }));
                            }
                        }
                        if !content_parts.is_empty() {
                            chat_messages.push(serde_json::json!({
                                "role": "user",
                                "content": content_parts
                            }));
                        }
                    }
                    _ => {}
                }
            }

            let system = if system_parts.len() == 1
                && system_parts[0].get("type").and_then(|t| t.as_str()) == Some("text")
            {
                system_parts[0]
                    .get("text")
                    .and_then(|t| t.as_str())
                    .map(|s| serde_json::json!(s))
            } else if !system_parts.is_empty() {
                Some(serde_json::json!(system_parts))
            } else {
                None
            };

            (chat_messages, system)
        };

        let mut body = serde_json::json!({
            "model": request.model,
            "messages": chat_messages,
            "stream": true,
        });

        if let Some(temp) = request.temperature {
            body["temperature"] = serde_json::json!(temp);
        }
        if let Some(max_tok) = request.max_tokens {
            body["max_tokens"] = serde_json::json!(max_tok);
        } else {
            body["max_tokens"] = serde_json::json!(8192);
        }
        if let Some(top_p) = request.top_p {
            body["top_p"] = serde_json::json!(top_p);
        }

        if let Some(system_val) = system {
            body["system"] = system_val;
        }

        // Tools: advertise JSON-Schema definitions on the wire.
        // The model returns tool calls as structured
        // `content_block` events with `type: "tool_use"`.
        if !request.tools.is_empty() {
            body["tools"] = serde_json::Value::Array(lower_tools(&request.tools));
        }

        // Opencode's transform.ts forces `thinking: { type:
        // "adaptive" }` for minimax-M3 on the Anthropic-Messages
        // route because minimax's Anthropic interface defaults
        // thinking off, unlike Chat Completions. Mirror that.
        if request.model.to_lowercase().contains("minimax-m3") {
            body["thinking"] = serde_json::json!({ "type": "adaptive" });
        }

        let (abort_tx, mut abort_rx) = oneshot::channel();
        let abort_tx = Arc::new(abort_tx);

        let response = self
            .http_client
            .post(&url)
            .header("x-api-key", api_key.as_str())
            .header("anthropic-version", "2023-06-01")
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                LlmError::from(e)
                    .provider("anthropic")
                    .model(&request.model)
            })?;

        tracing::info!(
            "llm response received: provider=anthropic url={} model={} status={}",
            url,
            request.model,
            response.status().as_u16(),
        );

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(LlmError::new(&format!("http_{}", status.as_u16()), &text)
                .provider("anthropic")
                .model(&request.model));
        }

        let stream_model = request.model.clone();
        let stream = async_stream::stream! {
            let mut event_stream = response.bytes_stream();
            let mut line_buffer = String::new();

            // Tracks tool_use blocks by content-block index. The
            // Anthropic protocol is index-driven: the model can
            // emit text + tool_use + thinking interleaved.
            let mut tool_state = AnthropicToolStreamState::default();

            // Tool results are produced by the runner after the model
            // stream ends and are included in the next request via
            // persisted conversation history.
            while let Some(chunk) = event_stream.next().await {
                if matches!(abort_rx.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Closed)) {
                    break;
                }

                let Ok(bytes) = chunk else { continue };
                let text = String::from_utf8_lossy(&bytes);

                for ch in text.chars() {
                    if ch == '\n' {
                        let line = line_buffer.trim();
                        let line_owned = line.to_string();
                        line_buffer.clear();

                        if line_owned.is_empty() {
                            continue;
                        }
                        log_sse_event_body("anthropic", &stream_model, &line_owned);

                        for event in parse_anthropic_sse_line(&line_owned, &mut tool_state) {
                            yield event;
                        }
                    } else {
                        line_buffer.push(ch);
                    }
                }
            }

            // Flush any still-open tool_use blocks.
            for event in tool_state.flush_all() {
                yield event;
            }
        };

        Ok(LlmStream {
            events: Box::pin(stream),
            abort_tx: Some(abort_tx),
        })
    }
}

/// State for accumulating `input_json_delta` fragments into
/// per-tool-use complete inputs. Keyed by content-block `index`
/// because Anthropic uses index as the primary correlation id (the
/// model's `id` is stable across the whole call but the `index`
/// arrives on every delta).
#[derive(Default)]
struct AnthropicToolStreamState {
    /// Map from content-block `index` to the in-flight tool use.
    by_index: HashMap<u32, InFlightToolUse>,
}

#[derive(Default)]
struct InFlightToolUse {
    id: String,
    name: String,
    raw_input: String,
    started: bool,
    closed: bool,
}

impl AnthropicToolStreamState {
    fn on_block_start(&mut self, index: u32, id: String, name: String) -> Vec<LlmEvent> {
        let entry = self.by_index.entry(index).or_default();
        entry.id = id.clone();
        entry.name = name.clone();
        entry.started = true;
        vec![LlmEvent::ToolInputStart { id, name }]
    }

    fn on_input_delta(&mut self, index: u32, partial_json: &str) -> Vec<LlmEvent> {
        let Some(entry) = self.by_index.get_mut(&index) else {
            return Vec::new();
        };
        entry.raw_input.push_str(partial_json);
        vec![LlmEvent::ToolInputDelta {
            id: entry.id.clone(),
            name: entry.name.clone(),
            text: partial_json.to_string(),
        }]
    }

    fn on_block_stop(&mut self, index: u32) -> Vec<LlmEvent> {
        let Some(entry) = self.by_index.get_mut(&index) else {
            return Vec::new();
        };
        if entry.closed {
            return Vec::new();
        }
        entry.closed = true;
        let id = entry.id.clone();
        let name = entry.name.clone();
        let raw = std::mem::take(&mut entry.raw_input);
        let input: Value = if raw.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&raw).unwrap_or(Value::Null)
        };
        vec![
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

    fn flush_all(&mut self) -> Vec<LlmEvent> {
        let indices: Vec<u32> = self
            .by_index
            .iter()
            .filter_map(|(idx, entry)| if !entry.closed { Some(*idx) } else { None })
            .collect();
        let mut out = Vec::new();
        for idx in indices {
            out.extend(self.on_block_stop(idx));
        }
        out
    }
}

fn parse_anthropic_sse_line(
    line: &str,
    tool_state: &mut AnthropicToolStreamState,
) -> Vec<LlmEvent> {
    let data = match super::parse_sse_line(line) {
        Some(d) => d,
        None => return Vec::new(),
    };
    // Anthropic SSE: lines look like
    //   event: content_block_start
    //   data: {"type":"content_block_start","index":0, ...}
    // The event type is in the JSON's top-level `type` field,
    // which matches the `event:` line. We rely on the JSON alone
    // to avoid having to track two line buffers.
    let json: Value = match serde_json::from_str(&data) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let event_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let mut events = Vec::new();
    match event_type {
        "message_start" => {
            events.push(LlmEvent::StepStart { index: 0 });
        }
        "content_block_start" => {
            let block_type = json
                .get("content_block")
                .and_then(|b| b.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let index = json.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            match block_type {
                "text" => {
                    events.push(LlmEvent::TextStart {
                        id: format!("text-{index}"),
                    });
                    if let Some(text) = json
                        .get("content_block")
                        .and_then(|b| b.get("text"))
                        .and_then(|v| v.as_str())
                    {
                        if !text.is_empty() {
                            events.push(LlmEvent::TextDelta {
                                id: format!("text-{index}"),
                                text: text.to_string(),
                            });
                        }
                    }
                }
                "thinking" => {
                    events.push(LlmEvent::ReasoningStart {
                        id: format!("reasoning-{index}"),
                    });
                    if let Some(thinking) = json
                        .get("content_block")
                        .and_then(|b| b.get("thinking"))
                        .and_then(|v| v.as_str())
                    {
                        if !thinking.is_empty() {
                            events.push(LlmEvent::ReasoningDelta {
                                id: format!("reasoning-{index}"),
                                text: thinking.to_string(),
                            });
                        }
                    }
                }
                "tool_use" => {
                    let id = json
                        .get("content_block")
                        .and_then(|b| b.get("id"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = json
                        .get("content_block")
                        .and_then(|b| b.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    events.extend(tool_state.on_block_start(index, id, name));
                }
                _ => {}
            }
        }
        "content_block_delta" => {
            let delta_type = json
                .get("delta")
                .and_then(|d| d.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let index = json.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            match delta_type {
                "text_delta" => {
                    if let Some(text) = json
                        .get("delta")
                        .and_then(|d| d.get("text"))
                        .and_then(|v| v.as_str())
                    {
                        if !text.is_empty() {
                            events.push(LlmEvent::TextDelta {
                                id: format!("text-{index}"),
                                text: text.to_string(),
                            });
                        }
                    }
                }
                "thinking_delta" => {
                    if let Some(text) = json
                        .get("delta")
                        .and_then(|d| d.get("thinking"))
                        .and_then(|v| v.as_str())
                    {
                        if !text.is_empty() {
                            events.push(LlmEvent::ReasoningDelta {
                                id: format!("reasoning-{index}"),
                                text: text.to_string(),
                            });
                        }
                    }
                }
                "input_json_delta" => {
                    if let Some(partial) = json
                        .get("delta")
                        .and_then(|d| d.get("partial_json"))
                        .and_then(|v| v.as_str())
                    {
                        events.extend(tool_state.on_input_delta(index, partial));
                    }
                }
                "signature_delta" => {
                    // Anthropic sends a signature alongside the
                    // last thinking delta for verification; we
                    // don't surface it (the LLMEvent schema
                    // doesn't model it).
                }
                _ => {}
            }
        }
        "content_block_stop" => {
            let index = json.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            // Try tool_state first; if there's no tool_use at this
            // index, fall back to a text/reasoning end by sniffing
            // the prior content. Since we don't track per-index
            // text/reasoning, the simplest correct behavior is to
            // emit a tool close if we have one, otherwise nothing.
            let tool_events = tool_state.on_block_stop(index);
            if !tool_events.is_empty() {
                events.extend(tool_events);
            } else {
                events.push(LlmEvent::TextEnd {
                    id: format!("block-{index}"),
                });
            }
        }
        "message_delta" => {
            if let Some(stop_reason) = json
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(|v| v.as_str())
            {
                let reason = match stop_reason {
                    "end_turn" | "stop_sequence" => FinishReason::Stop,
                    "max_tokens" | "model_context_window_exceeded" => FinishReason::Length,
                    "tool_use" => FinishReason::ToolCalls,
                    _ => FinishReason::Unknown,
                };
                let usage = json.get("usage").and_then(|u| parse_anthropic_usage(u));
                events.push(LlmEvent::Finish { reason, usage });
            }
        }
        "message_stop" => {
            events.push(LlmEvent::Finish {
                reason: FinishReason::Stop,
                usage: None,
            });
        }
        "ping" => {
            // Heartbeat; ignore.
        }
        "error" => {
            if let Some(message) = json
                .get("error")
                .and_then(|e| e.get("message"))
                .and_then(|v| v.as_str())
            {
                events.push(LlmEvent::ProviderError {
                    message: message.to_string(),
                });
            }
        }
        _ => {}
    }
    events
}

fn parse_anthropic_usage(value: &Value) -> Option<Usage> {
    Some(Usage {
        input_tokens: value.get("input_tokens").and_then(|v| v.as_u64()),
        output_tokens: value.get("output_tokens").and_then(|v| v.as_u64()),
        total_tokens: value.get("total_tokens").and_then(|v| v.as_u64()),
        reasoning_tokens: value.get("thinking_tokens").and_then(|v| v.as_u64()),
        cache_read_input_tokens: value
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_u64()),
        cache_write_input_tokens: value
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64()),
    })
}

fn lower_tools(tools: &[ToolDefinition]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.parameters,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::events::LlmEvent;
    use crate::llm::request::LlmMessage;
    use futures_util::StreamExt;

    #[test]
    fn provider_creation() {
        let provider = AnthropicProvider::new(Some("test-key".to_string()), None);
        assert_eq!(provider.provider_name(), "anthropic");
        assert!(provider.api_key().is_some());
    }

    #[test]
    fn provider_from_env() {
        std::env::set_var("ANTHROPIC_API_KEY", "env-key");
        let provider = AnthropicProvider::new(None, None);
        assert_eq!(provider.api_key(), Some("env-key"));
        std::env::remove_var("ANTHROPIC_API_KEY");
    }

    #[test]
    fn lower_tools_emits_anthropic_shape() {
        let tools = vec![ToolDefinition::new(
            "read",
            "Read a file",
            serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        )];
        let lowered = lower_tools(&tools);
        assert_eq!(lowered[0]["name"], "read");
        assert_eq!(lowered[0]["input_schema"]["type"], "object");
    }

    #[test]
    fn parses_tool_use_block_start() {
        let mut state = AnthropicToolStreamState::default();
        let line = "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"glob\"}}";
        let events = parse_anthropic_sse_line(line, &mut state);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, LlmEvent::ToolInputStart { id, name } if id == "toolu_1" && name == "glob")),
            "expected ToolInputStart, got: {events:?}"
        );
    }

    #[test]
    fn parses_input_json_delta_and_block_stop() {
        let mut state = AnthropicToolStreamState::default();
        let start = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read\"}}";
        let d1 = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}";
        let d2 = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"/tmp/x\\\"}\"}}";
        let stop = "data: {\"type\":\"content_block_stop\",\"index\":0}";

        let _ = parse_anthropic_sse_line(start, &mut state);
        let e1 = parse_anthropic_sse_line(d1, &mut state);
        let e2 = parse_anthropic_sse_line(d2, &mut state);
        let stop_events = parse_anthropic_sse_line(stop, &mut state);

        // First delta: one ToolInputDelta.
        assert!(matches!(e1[0], LlmEvent::ToolInputDelta { .. }));
        assert_eq!(e1.len(), 1);
        // Second delta: one ToolInputDelta.
        assert!(matches!(e2[0], LlmEvent::ToolInputDelta { .. }));
        // Stop: ToolInputEnd + ToolCall with parsed input.
        assert!(
            matches!(stop_events[0], LlmEvent::ToolInputEnd { ref name, .. } if name == "read")
        );
        let LlmEvent::ToolCall { name, input, .. } = &stop_events[1] else {
            panic!("expected ToolCall, got: {stop_events:?}");
        };
        assert_eq!(name, "read");
        assert_eq!(input["path"], "/tmp/x");
    }

    #[test]
    fn parses_thinking_block() {
        let mut state = AnthropicToolStreamState::default();
        let start = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}";
        let delta = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me think.\"}}";
        let stop = "data: {\"type\":\"content_block_stop\",\"index\":0}";

        let start_events = parse_anthropic_sse_line(start, &mut state);
        let d_events = parse_anthropic_sse_line(delta, &mut state);
        let _ = parse_anthropic_sse_line(stop, &mut state);

        assert!(matches!(start_events[0], LlmEvent::ReasoningStart { .. }));
        assert!(
            matches!(d_events[0], LlmEvent::ReasoningDelta { ref text, .. } if text == "Let me think.")
        );
    }

    #[test]
    fn parses_message_delta_stop_reason_tool_use() {
        let mut state = AnthropicToolStreamState::default();
        let line = "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}";
        let events = parse_anthropic_sse_line(line, &mut state);
        assert!(
            events.iter().any(|e| matches!(
                e,
                LlmEvent::Finish {
                    reason: FinishReason::ToolCalls,
                    ..
                }
            )),
            "expected Finish ToolCalls, got: {events:?}"
        );
    }

    #[tokio::test]
    async fn stream_produces_events_or_http_error() {
        let provider = AnthropicProvider::new(Some("invalid-anthropic-key".to_string()), None);
        let request = LlmRequest::new("claude-3-5-sonnet-20241022", "anthropic")
            .with_message(LlmMessage::user("say hello"));

        let result = provider.stream(request).await;

        if let Ok(stream) = result {
            let events: Vec<_> = stream.events.collect().await;
            assert!(
                !events.is_empty(),
                "stream should produce at least one event"
            );
            let has_text_or_error = events.iter().any(|e| {
                matches!(e, LlmEvent::TextDelta { .. })
                    || matches!(e, LlmEvent::ReasoningDelta { .. })
                    || matches!(e, LlmEvent::ProviderError { .. })
                    || matches!(e, LlmEvent::Finish { .. })
            });
            assert!(
                has_text_or_error,
                "stream should contain text, reasoning, error, or finish event: {events:?}"
            );
        } else {
            let err = match result {
                Err(e) => e,
                _ => unreachable!("expected error"),
            };
            assert!(
                err.code.starts_with("http_"),
                "should be an HTTP error, got: {} - {}",
                err.code,
                err.message
            );
        }
    }
}
