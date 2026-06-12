use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;

use super::{log_sse_event_body, LlmError, LlmProvider, LlmStream};
use crate::llm::events::{FinishReason, LlmEvent, ToolDefinition};
use crate::llm::request::{ContentPart, LlmRequest};

pub struct OpenAiCompatibleChatProvider {
    provider_name: String,
    api_key_env: String,
    api_key: Option<String>,
    base_url: String,
    supported_models: Vec<String>,
    http_client: reqwest::Client,
}

impl OpenAiCompatibleChatProvider {
    pub fn new(
        provider_name: &str,
        api_key: Option<String>,
        base_url: Option<String>,
        default_base_url: &str,
        api_key_env: &str,
        supported_models: &[&str],
    ) -> Self {
        Self {
            provider_name: provider_name.to_string(),
            api_key_env: api_key_env.to_string(),
            api_key: api_key.or_else(|| std::env::var(api_key_env).ok()),
            base_url: base_url.unwrap_or_else(|| default_base_url.to_string()),
            supported_models: supported_models
                .iter()
                .map(|model| model.to_string())
                .collect(),
            http_client: reqwest::Client::new(),
        }
    }

    pub fn with_base_url(mut self, url: &str) -> Self {
        self.base_url = url.to_string();
        self
    }

    pub fn chat_completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    pub async fn endpoint_status(&self, model: &str) -> Result<reqwest::StatusCode, LlmError> {
        let body = serde_json::json!({
            "model": model,
            "messages": [{ "role": "user", "content": "ping" }],
            "stream": false,
            "max_tokens": 1,
        });

        let mut request = self
            .http_client
            .post(self.chat_completions_url())
            .timeout(Duration::from_secs(15))
            .header("Content-Type", "application/json")
            .json(&body);

        if let Some(api_key) = self.api_key.as_deref() {
            request = request.header("Authorization", format!("Bearer {api_key}"));
        }

        request
            .send()
            .await
            .map(|response| response.status())
            .map_err(|error| {
                LlmError::from(error)
                    .provider(&self.provider_name)
                    .model(model)
            })
    }

    fn auth_error(&self) -> LlmError {
        LlmError::new("auth", &format!("{} not set", self.api_key_env))
            .provider(&self.provider_name)
    }
}

#[async_trait]
impl LlmProvider for OpenAiCompatibleChatProvider {
    fn provider_name(&self) -> &str {
        &self.provider_name
    }

    fn supported_models(&self) -> Vec<String> {
        self.supported_models.clone()
    }

    fn model_base_url(&self) -> Option<&str> {
        Some(&self.base_url)
    }

    fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    async fn stream(&self, request: LlmRequest) -> Result<LlmStream, LlmError> {
        let api_key = self.api_key.as_ref().ok_or_else(|| self.auth_error())?;
        let body = build_request_body(&request, lower_messages);

        tracing::debug!(
            "llm request: provider={} url={} model={} has_api_key={} tool_count={}",
            self.provider_name,
            self.chat_completions_url(),
            request.model,
            !api_key.is_empty(),
            request.tools.len(),
        );

        let (abort_tx, mut abort_rx) = oneshot::channel();
        let abort_tx = Arc::new(abort_tx);
        let auth_header = format!("Bearer {api_key}");
        let body_bytes = serde_json::to_vec(&body)
            .map(|body| body.len())
            .unwrap_or(0);
        tracing::info!(
            "llm request body prepared: provider={} url={} model={} body_bytes={}",
            self.provider_name,
            self.chat_completions_url(),
            request.model,
            body_bytes,
        );

        let response = self
            .http_client
            .post(self.chat_completions_url())
            .header("Authorization", &auth_header)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                LlmError::from(error)
                    .provider(&self.provider_name)
                    .model(&request.model)
            })?;

        tracing::info!(
            "llm response received: provider={} url={} model={} status={}",
            self.provider_name,
            self.chat_completions_url(),
            request.model,
            response.status().as_u16(),
        );

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            tracing::warn!(
                "llm http error: provider={} url={} model={} status={} body={}",
                self.provider_name,
                self.chat_completions_url(),
                request.model,
                status.as_u16(),
                &text.chars().take(500).collect::<String>(),
            );
            return Err(LlmError::new(&format!("http_{}", status.as_u16()), &text)
                .provider(&self.provider_name)
                .model(&request.model));
        }

        let stream_provider = self.provider_name.clone();
        let stream_model = request.model.clone();
        let stream = async_stream::stream! {
            let mut event_stream = response.bytes_stream();

            // Per-turn parser state. Tool-call arguments stream
            // across many `data: ...` chunks (one JSON fragment per
            // chunk). The model only signals a tool call is "done"
            // implicitly: either the next chunk's
            // `delta.tool_calls[*]` is empty/absent, or the
            // `finish_reason` is `tool_calls` (or `stop` after a
            // tool call). We flush any pending tool calls at finish.
            let mut tool_state = OpenAiToolStreamState::default();
            // Tracks whether we've already emitted a terminal
            // `Finish` event for this turn. Used to guarantee
            // exactly one `Finish` per turn even if the stream
            // ends abruptly (truncated response, network drop)
            // after the per-chunk branch already emitted one.
            let mut finished = false;

            // The HTTP stream is the single source of truth for this
            // provider turn. Tool results are produced by the runner
            // after the stream ends and are included in the next request
            // via persisted conversation history.
            while let Some(chunk) = event_stream.next().await {
                if matches!(abort_rx.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Closed)) {
                    break;
                }

                let Ok(bytes) = chunk else { continue };
                let text = String::from_utf8_lossy(&bytes);

                let mut saw_done = false;
                let mut finish_reason_seen: Option<FinishReason> = None;
                let mut usage_seen: Option<crate::llm::events::Usage> = None;

                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }

                    if line == "data: [DONE]" || line == "[DONE]" {
                        log_sse_event_body(&stream_provider, &stream_model, line);
                        saw_done = true;
                        break;
                    }

                    if let Some(data) = line.strip_prefix("data: ") {
                        let data = data.trim();
                        if data.is_empty() || data == "[DONE]" {
                            continue;
                        }
                        log_sse_event_body(&stream_provider, &stream_model, data);

                        // Parse the chunk into (events, finish-metadata).
                        // The parser no longer emits `Finish` events
                        // itself — it returns the finish reason +
                        // usage as out-of-band metadata so the
                        // stream loop is the single source of truth
                        // for the terminal `Finish` event.
                        let parsed = parse_openai_chunk_into_events(data, &mut tool_state);
                        if let Some(reason) = parsed.finish_reason {
                            finish_reason_seen = Some(reason);
                        }
                        if parsed.usage.is_some() {
                            usage_seen = parsed.usage;
                        }
                        for event in parsed.events {
                            yield event;
                        }
                    }
                }

                if saw_done || finish_reason_seen.is_some() {
                    // Flush any tool calls the model streamed but
                    // didn't terminate. OpenAI's wire format only
                    // completes a tool call when the arguments JSON
                    // closes; we detect "closed" by the chunk's
                    // `delta.tool_calls[*].function.arguments` not
                    // being present OR by a terminal finish reason.
                    for event in tool_state.flush_all() {
                        yield event;
                    }
                    let reason = finish_reason_seen.unwrap_or(FinishReason::Stop);
                    yield LlmEvent::Finish { reason, usage: usage_seen };
                    finished = true;
                    break;
                }
            }

            // Belt-and-suspenders: if the stream ended without a
            // finish reason (e.g. truncated response), flush
            // anything that was open. The terminal `Finish` is
            // already yielded by the per-chunk branch above
            // (which sets `finished = true`), so we only
            // emit a `Finish` here if the stream ended without
            // one being observed.
            for event in tool_state.flush_all() {
                yield event;
            }
            if !finished {
                yield LlmEvent::Finish {
                    reason: FinishReason::Stop,
                    usage: None,
                };
            }
        };

        Ok(LlmStream {
            events: Box::pin(stream),
            abort_tx: Some(abort_tx),
        })
    }
}

/// State for accumulating tool-call argument deltas across SSE chunks.
#[derive(Default)]
struct OpenAiToolStreamState {
    /// Active tool calls, keyed by the provider-assigned id (or a
    /// synthesized `__index:N` key when the provider omits ids). The
    /// first chunk for an id has `function.name` set; subsequent
    /// chunks append to `arguments`. The `started` flag distinguishes
    /// "we already emitted ToolInputStart for this id" so we don't
    /// emit duplicates when the model re-sends the name.
    calls: HashMap<String, InFlightToolCall>,
    /// Aliases from `tool_calls[*].index` to the resolved key.
    /// We populate this whenever a delta includes both an `id` and
    /// an `index` so subsequent index-only deltas correlate to the
    /// same call.
    index_aliases: HashMap<u64, String>,
    /// Monotonic counter for synthetic keys when neither `id` nor
    /// `index` is present. The OpenAI streaming spec requires at
    /// least one of the two, so this is just a defensive fallback.
    anon_seq: u64,
}

#[derive(Clone, Default, Debug)]
struct InFlightToolCall {
    name: String,
    arguments: String,
    started: bool,
    closed: bool,
}

impl OpenAiToolStreamState {
    fn ingest(&mut self, delta: &Value) -> Vec<LlmEvent> {
        let mut events = Vec::new();
        let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) else {
            return events;
        };

        for call in tool_calls {
            // Resolve a stable key for this tool call. The OpenAI
            // Chat Completions streaming protocol puts the
            // provider-assigned `id` on the first delta and then
            // sends the rest without it. Some providers (notably
            // minimax and the OpenAI Responses API) only send
            // `index` on every delta. To support both shapes we
            // prefer `id` when present, otherwise fall back to
            // `index`. We also remember the resolved key against
            // the index so subsequent deltas that finally do send
            // an `id` still correlate to the same call.
            let key = self.resolve_call_key(call);

            if let Some(name) = call
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
            {
                if !name.is_empty() {
                    // Bind the name to the index/alias so deltas
                    // that omit the name still resolve to the
                    // correct call.
                    self.bind_name_for_key(&key, name);
                }
            }
            // Always append the `arguments` fragment. This is the
            // critical fix: the model streams the JSON as a
            // sequence of partial strings and we must concatenate
            // them all per tool-call `index` before parsing.
            let args_fragment = call
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .map(|s| s.to_string());
            if let Some(fragment) = args_fragment.as_ref() {
                if !fragment.is_empty() {
                    self.append_arguments(&key, fragment);
                }
            }

            // Emit the lifecycle events: ToolInputStart when the
            // name first arrives, ToolInputDelta for each argument
            // fragment.
            let entry = self.calls.get(&key).cloned();
            if let Some(entry) = entry {
                if !entry.closed {
                    if !entry.started && !entry.name.is_empty() {
                        if let Some(slot) = self.calls.get_mut(&key) {
                            slot.started = true;
                        }
                        events.push(LlmEvent::ToolInputStart {
                            id: key.clone(),
                            name: entry.name.clone(),
                        });
                    }
                    if let Some(fragment) = args_fragment {
                        if !fragment.is_empty() {
                            events.push(LlmEvent::ToolInputDelta {
                                id: key.clone(),
                                name: entry.name.clone(),
                                text: fragment,
                            });
                        }
                    }
                }
            }
        }
        events
    }

    /// Resolve a stable key for a single `tool_calls[*]` entry. We
    /// prefer `id` (the provider-assigned identifier) and fall
    /// back to `index` (the integer position in the array). We
    /// also alias the index → id mapping so later deltas that
    /// finally include the id still correlate to the same call.
    fn resolve_call_key(&mut self, call: &Value) -> String {
        if let Some(id) = call.get("id").and_then(|v| v.as_str()) {
            if !id.is_empty() {
                if let Some(index) = call.get("index").and_then(|i| i.as_u64()) {
                    self.index_aliases.insert(index, id.to_string());
                }
                return id.to_string();
            }
        }
        if let Some(index) = call.get("index").and_then(|i| i.as_u64()) {
            if let Some(existing) = self.index_aliases.get(&index) {
                return existing.clone();
            }
            return format!("__index:{index}");
        }
        // Last resort: synthesize a key from the call index in
        // the array. Acceptable for one-off deltas but the same
        // call can land under different synthetic keys if the
        // provider alternates between index and no-index.
        let fallback = format!("__anon:{}", self.anon_seq);
        self.anon_seq += 1;
        fallback
    }

    fn bind_name_for_key(&mut self, key: &str, name: &str) {
        let entry = self.calls.entry(key.to_string()).or_default();
        entry.name = name.to_string();
    }

    fn append_arguments(&mut self, key: &str, fragment: &str) {
        let entry = self.calls.entry(key.to_string()).or_default();
        entry.arguments.push_str(fragment);
    }

    /// Mark a tool call as complete and emit `ToolInputEnd` + `ToolCall`.
    /// The `input` is parsed from the accumulated `arguments` string;
    /// an unparseable string becomes `Value::Null` (matches opencode's
    /// `parseToolInput` empty-input behavior).
    fn finalize(&mut self, id: &str) -> Vec<LlmEvent> {
        let mut events = Vec::new();
        if let Some(entry) = self.calls.get_mut(id) {
            if entry.closed {
                return events;
            }
            entry.closed = true;
            let name = entry.name.clone();
            let raw = std::mem::take(&mut entry.arguments);
            let input: Value = if raw.is_empty() {
                Value::Null
            } else {
                serde_json::from_str(&raw).unwrap_or(Value::Null)
            };

            events.push(LlmEvent::ToolInputEnd {
                id: id.to_string(),
                name: name.clone(),
            });
            events.push(LlmEvent::ToolCall {
                id: id.to_string(),
                name,
                input,
                provider_executed: Some(false),
            });
        }
        events
    }

    /// Flush every still-open tool call (e.g. at end-of-stream when
    /// the model didn't explicitly close them).
    fn flush_all(&mut self) -> Vec<LlmEvent> {
        let ids: Vec<String> = self
            .calls
            .iter()
            .filter_map(|(id, entry)| {
                if !entry.closed {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect();
        let mut out = Vec::new();
        for id in ids {
            out.extend(self.finalize(&id));
        }
        out
    }
}

/// One SSE chunk's parsed output. The stream-of-consciousness
/// events (text, reasoning, tool-call lifecycle) go in `events`;
/// the terminal signals (finish_reason, usage) go in the side
/// fields. The stream loop merges everything into exactly one
/// `Finish` event per turn.
#[derive(Debug)]
struct ParsedChunk {
    events: Vec<LlmEvent>,
    finish_reason: Option<FinishReason>,
    usage: Option<crate::llm::events::Usage>,
}

impl ParsedChunk {
    fn empty() -> Self {
        Self {
            events: Vec::new(),
            finish_reason: None,
            usage: None,
        }
    }
}

fn parse_openai_chunk_into_events(
    text: &str,
    tool_state: &mut OpenAiToolStreamState,
) -> ParsedChunk {
    let json: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return ParsedChunk::empty(),
    };
    let mut events = Vec::new();
    let mut finish_reason: Option<FinishReason> = None;
    let mut usage: Option<crate::llm::events::Usage> = None;

    // Tool-call deltas first: opencode ingests them into the
    // tool-stream accumulator regardless of whether there's also
    // text or reasoning in the same chunk.
    for delta in json
        .get("choices")
        .and_then(|c| c.as_array())
        .into_iter()
        .flatten()
        .filter_map(|c| c.get("delta"))
    {
        events.extend(tool_state.ingest(delta));
    }

    // Some OpenAI-compatible providers (notably the original
    // minimax M2) only return a single "complete" tool call per
    // chunk with the full arguments. In that case
    // `delta.tool_calls[*].function.arguments` arrives as one
    // string. We treat that as a complete call: emit
    // Start/Delta(empty)/End+ToolCall in one batch.
    for choice in json
        .get("choices")
        .and_then(|c| c.as_array())
        .into_iter()
        .flatten()
    {
        if let Some(message) = choice.get("message") {
            for event in ingest_complete_tool_message(message, tool_state) {
                events.push(event);
            }
        }
        if let Some(delta) = choice.get("delta") {
            // reasoning + text below
            let reasoning = delta
                .get("reasoning_content")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| {
                    delta
                        .get("reasoning_details")
                        .and_then(|v| v.as_array())
                        .and_then(|arr| arr.first())
                        .and_then(|d| d.get("text"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                });
            if let Some(reason) = reasoning {
                if !reason.is_empty() {
                    events.push(LlmEvent::ReasoningDelta {
                        id: "reasoning".to_string(),
                        text: reason,
                    });
                }
            }
            if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    events.push(LlmEvent::TextDelta {
                        id: "text".to_string(),
                        text: text.to_string(),
                    });
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
            finish_reason = Some(FinishReason::from(reason));
        }
    }

    // Usage is reported on its own chunk (often the last
    // `data: ...` line) when `stream_options.include_usage` is
    // set. The stream loop merges the usage into the terminal
    // `Finish` event.
    if let Some(u) = json.get("usage").and_then(parse_openai_usage) {
        usage = Some(u);
    }

    ParsedChunk {
        events,
        finish_reason,
        usage,
    }
}

fn ingest_complete_tool_message(
    message: &Value,
    tool_state: &mut OpenAiToolStreamState,
) -> Vec<LlmEvent> {
    let mut events = Vec::new();
    let Some(tool_calls) = message.get("tool_calls").and_then(|v| v.as_array()) else {
        return events;
    };
    for call in tool_calls {
        let id = call
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("tool-{}", tool_state.calls.len()));
        let name = call
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        let args = call
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(|a| a.as_str())
            .unwrap_or("")
            .to_string();
        // Single-chunk complete tool call. Emit the full lifecycle.
        let entry = tool_state.calls.entry(id.clone()).or_default();
        entry.name = name.clone();
        entry.arguments = args.clone();
        entry.started = true;
        entry.closed = false;
        events.push(LlmEvent::ToolInputStart {
            id: id.clone(),
            name: name.clone(),
        });
        if !args.is_empty() {
            events.push(LlmEvent::ToolInputDelta {
                id: id.clone(),
                name: name.clone(),
                text: args.clone(),
            });
        }
        events.extend(tool_state.finalize(&id));
    }
    events
}

fn parse_openai_usage(value: &Value) -> Option<crate::llm::events::Usage> {
    Some(crate::llm::events::Usage {
        input_tokens: value.get("prompt_tokens").and_then(|v| v.as_u64()),
        output_tokens: value.get("completion_tokens").and_then(|v| v.as_u64()),
        total_tokens: value.get("total_tokens").and_then(|v| v.as_u64()),
        reasoning_tokens: value
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|v| v.as_u64()),
        cache_read_input_tokens: value
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|v| v.as_u64()),
        cache_write_input_tokens: None,
    })
}

fn build_request_body(
    request: &LlmRequest,
    lower_messages: impl Fn(&LlmRequest) -> Vec<serde_json::Value>,
) -> serde_json::Value {
    let messages = lower_messages(request);

    let mut body = serde_json::json!({
        "model": request.model,
        "messages": messages,
        "stream": true,
        "stream_options": { "include_usage": true },
    });

    if let Some(temp) = request.temperature {
        body["temperature"] = serde_json::json!(temp);
    }
    if let Some(max_tok) = request.max_tokens {
        body["max_tokens"] = serde_json::json!(max_tok);
    }
    if let Some(top_p) = request.top_p {
        body["top_p"] = serde_json::json!(top_p);
    }
    if let Some(stop) = &request.stop {
        body["stop"] = serde_json::json!(stop);
    }

    // Tools: advertise the JSON-Schema definitions on the wire. The
    // model is expected to return tool calls as structured
    // `delta.tool_calls` events. We DO NOT also inject an XML tool
    // description into the system prompt — that would be
    // contradictory and would let the model emit text-only tool
    // calls that we'd have to parse out of the stream.
    if !request.tools.is_empty() {
        body["tools"] = serde_json::Value::Array(lower_tools(&request.tools));
    }

    body
}

fn lower_tools(tools: &[ToolDefinition]) -> Vec<serde_json::Value> {
    tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod body_tests {
    use super::*;
    use crate::llm::events::ToolDefinition;
    use crate::llm::request::LlmMessage;

    fn sample_request() -> LlmRequest {
        LlmRequest::new("gpt-4o-mini", "openai")
            .with_message(LlmMessage::user("hi"))
            .with_tools(std::iter::once(ToolDefinition::new(
                "read",
                "Read a file",
                serde_json::json!({"type": "object", "properties": {"path": {"type":"string"}}}),
            )))
    }

    /// Build a minimal SSE chunk that carries only an `index=0`
    /// `tool_calls[*].function.arguments` fragment (the shape used
    /// by minimax after the first delta). The fragment is the string
    /// value returned by the provider after JSON-decoding the SSE
    /// chunk, so callers pass raw pieces like `"\"command\":"`.
    fn build_index_only_chunk(arguments_fragment: &str) -> String {
        serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": { "arguments": arguments_fragment }
                    }]
                }
            }]
        })
        .to_string()
    }

    #[test]
    fn build_request_body_includes_tools_field() {
        // After the opencode-style refactor, tools ARE sent on the
        // wire. The model returns structured `delta.tool_calls`
        // events.
        let body = build_request_body(&sample_request(), |_req| {
            vec![serde_json::json!({"role": "user", "content": "hi"})]
        });
        let tools = body.get("tools").and_then(|t| t.as_array()).expect("tools");
        assert_eq!(tools.len(), 1, "expected one tool, got: {body}");
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "read");
    }

    #[test]
    fn build_request_body_omits_tools_field_when_empty() {
        let req = LlmRequest::new("gpt-4o-mini", "openai").with_message(LlmMessage::user("hi"));
        let body = build_request_body(&req, |_| vec![]);
        assert!(
            body.get("tools").is_none(),
            "tools must not be sent when none are defined; body was: {body}"
        );
    }

    #[test]
    fn build_request_body_includes_messages_model_stream() {
        let body = build_request_body(&sample_request(), |_req| {
            vec![serde_json::json!({"role": "user", "content": "hi"})]
        });
        assert_eq!(body["model"], "gpt-4o-mini");
        assert_eq!(body["stream"], true);
        assert!(body["stream_options"]["include_usage"]
            .as_bool()
            .unwrap_or(false));
        assert!(body["messages"]
            .as_array()
            .map(|a| a.len() == 1)
            .unwrap_or(false));
    }

    #[test]
    fn build_request_body_passes_through_optional_params() {
        let mut req = sample_request();
        req.temperature = Some(0.5);
        req.max_tokens = Some(256);
        req.top_p = Some(0.9);
        req.stop = Some(vec!["STOP".to_string()]);
        let body = build_request_body(&req, |_| vec![]);
        assert_eq!(body["temperature"].as_f64().unwrap(), 0.5);
        assert_eq!(body["max_tokens"], 256);
        let top_p = body["top_p"].as_f64().unwrap();
        assert!((top_p - 0.9).abs() < 1e-5, "top_p was {top_p}");
        assert_eq!(body["stop"][0], "STOP");
    }

    // ---- tool-stream parser ----

    #[test]
    fn tool_stream_accumulates_arguments_across_chunks() {
        let mut state = OpenAiToolStreamState::default();
        let chunk1 = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "read", "arguments": "{\"path\":\"" }
                    }]
                }
            }]
        });
        let chunk2 = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "arguments": "src/lib.rs\"}" }
                    }]
                }
            }]
        });

        let e1 = parse_openai_chunk_into_events(&chunk1.to_string(), &mut state);
        let e2 = parse_openai_chunk_into_events(&chunk2.to_string(), &mut state);

        // First chunk: Start, Delta.
        assert!(
            matches!(e1.events[0], LlmEvent::ToolInputStart { ref name, .. } if name == "read")
        );
        assert!(matches!(e1.events[1], LlmEvent::ToolInputDelta { .. }));
        assert_eq!(e1.events.len(), 2, "chunk1 events: {e1:?}");
        assert!(e1.finish_reason.is_none());
        assert!(e1.usage.is_none());

        // Second chunk: Delta only (start was already emitted).
        assert!(matches!(e2.events[0], LlmEvent::ToolInputDelta { .. }));
        assert_eq!(e2.events.len(), 1, "chunk2 events: {e2:?}");

        // Flush.
        let final_events = state.flush_all();
        assert!(
            matches!(final_events[0], LlmEvent::ToolInputEnd { ref name, .. } if name == "read")
        );
        let LlmEvent::ToolCall { name, input, .. } = &final_events[1] else {
            panic!("expected ToolCall, got: {final_events:?}");
        };
        assert_eq!(name, "read");
        assert_eq!(input["path"], "src/lib.rs");
    }

    /// Regression: the live provider (minimax) streams the `arguments`
    /// JSON as a long sequence of partial deltas. The first delta
    /// carries the provider-assigned `id`; every subsequent delta
    /// carries only `index` and a fragment. We must correlate by
    /// `index` after the first delta and concatenate every
    /// fragment for the same `index` before parsing.
    #[test]
    fn tool_stream_correlates_by_index_when_id_drops() {
        let mut state = OpenAiToolStreamState::default();
        let chunks = vec![
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"call_abc","index":0,"function":{"name":"shell","arguments":"{\"command\":\""}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"echo "}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"hello \\\\"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":" world\"}"}}]}}]}"#,
        ];
        for chunk in &chunks {
            parse_openai_chunk_into_events(chunk, &mut state);
        }
        let final_events = state.flush_all();
        let tool_call = final_events
            .iter()
            .find_map(|e| match e {
                LlmEvent::ToolCall { name, input, .. } => Some((name.clone(), input.clone())),
                _ => None,
            })
            .expect("expected ToolCall event");
        let (name, input) = tool_call;
        assert_eq!(name, "shell");
        assert_eq!(
            input["command"], "echo hello \\ world",
            "args did not concatenate in order; got: {input}"
        );
    }

    #[test]
    fn tool_stream_complete_tool_message_ingested() {
        let mut state = OpenAiToolStreamState::default();
        let chunk = serde_json::json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "glob",
                            "arguments": "{\"pattern\":\"*.rs\"}"
                        }
                    }]
                }
            }]
        });
        let parsed = parse_openai_chunk_into_events(&chunk.to_string(), &mut state);
        // Expect Start, Delta, End, ToolCall in `parsed.events`.
        assert!(parsed
            .events
            .iter()
            .any(|e| matches!(e, LlmEvent::ToolInputStart { name, .. } if name == "glob")));
        let tool_call = parsed
            .events
            .iter()
            .find_map(|e| match e {
                LlmEvent::ToolCall { name, input, .. } => Some((name, input)),
                _ => None,
            })
            .expect("tool call");
        assert_eq!(tool_call.0, "glob");
        assert_eq!(tool_call.1["pattern"], "*.rs");
    }

    #[test]
    fn tool_stream_finish_reason_tool_calls_flushes() {
        let mut state = OpenAiToolStreamState::default();
        // Open a tool call.
        let open = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "read", "arguments": "{\"path\":\"/tmp/x\"}" }
                    }]
                }
            }]
        });
        let _ = parse_openai_chunk_into_events(&open.to_string(), &mut state);

        // Finish with `tool_calls` reason. The parser surfaces
        // the reason as a side-field; the stream loop is the
        // one that actually emits the `Finish` event.
        let fin = serde_json::json!({
            "choices": [{ "finish_reason": "tool_calls" }]
        });
        let parsed = parse_openai_chunk_into_events(&fin.to_string(), &mut state);
        assert!(
            matches!(parsed.finish_reason, Some(FinishReason::ToolCalls)),
            "expected finish_reason=ToolCalls, got: {:?}",
            parsed.finish_reason
        );
        // No `Finish` event from the parser itself.
        assert!(
            !parsed
                .events
                .iter()
                .any(|e| matches!(e, LlmEvent::Finish { .. })),
            "parser must not emit Finish events, got: {parsed:?}"
        );
    }

    #[test]
    fn tool_stream_arguments_parse_failure_yields_null_input() {
        let mut state = OpenAiToolStreamState::default();
        let open = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "read", "arguments": "not-json" }
                    }]
                }
            }]
        });
        let _ = parse_openai_chunk_into_events(&open.to_string(), &mut state);
        let fin = state.flush_all();
        let LlmEvent::ToolCall { input, .. } = &fin[1] else {
            panic!("expected ToolCall, got: {fin:?}");
        };
        assert!(
            input.is_null(),
            "expected Null on parse failure, got: {input}"
        );
    }

    /// Regression: the exact `shell` tool call shape from the
    /// production log. The `arguments` JSON is streamed as ~25
    /// tiny fragments; the first fragment carries the provider id
    /// and the rest carry only `index=0`. The final assembled
    /// `arguments` must parse cleanly and the parsed `command`
    /// must match the original string byte-for-byte.
    #[test]
    fn tool_stream_shell_command_with_long_path_extracts_correctly() {
        let mut state = OpenAiToolStreamState::default();
        let first = concat!(
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"call_abc","index":0,"type":"function","#,
            r#""function":{"name":"shell","arguments":"{"}}]}}]}"#
        );
        // Subsequent deltas: only `index` plus a fragment.
        // Each fragment is a *raw JSON string literal fragment* —
        // the same bytes that would appear inside the surrounding
        // quotes on the wire. The full target JSON is:
        //   {"command":"cd C:\\Users\\mrowe\\AppData\\...\\Cabinet-Factory && ls"}
        // and after JSON parsing the value of `command` is the
        // single-backslash string.
        let fragments = [
            "\"command\":",
            "\"cd C:\\\\Users\\\\mrowe\\\\AppData\\\\Roaming\\\\PYTHA\\\\.configurator-Configurator-Dev\\\\plugins\\\\Cabinet-Factory && ls\"",
            "}",
        ];
        let mut chunks = vec![first.to_string()];
        for fragment in fragments {
            let chunk = build_index_only_chunk(fragment);
            chunks.push(chunk);
        }
        for chunk in &chunks {
            parse_openai_chunk_into_events(chunk, &mut state);
        }
        let final_events = state.flush_all();
        let tool_call = final_events
            .iter()
            .find_map(|e| match e {
                LlmEvent::ToolCall { name, input, .. } => Some((name.clone(), input.clone())),
                _ => None,
            })
            .expect("expected ToolCall event");
        let (name, input) = tool_call;
        assert_eq!(name, "shell");
        let command = input
            .get("command")
            .and_then(|v| v.as_str())
            .expect("command should be a string");
        // The expected parsed string: each `\\` in the JSON literal
        // collapses to a single `\` in the actual string.
        let expected = "cd C:\\Users\\mrowe\\AppData\\Roaming\\PYTHA\\.configurator-Configurator-Dev\\plugins\\Cabinet-Factory && ls";
        assert_eq!(
            command, expected,
            "command mismatch; got: {command:?}, full input: {input}"
        );
    }

    /// Regression: the `glob` tool call from a real session.
    /// Verifies the `pattern` field is recovered intact.
    #[test]
    fn tool_stream_glob_pattern_extracts_correctly() {
        let mut state = OpenAiToolStreamState::default();
        let first = concat!(
            r#"{"choices":[{"delta":{"tool_calls":[{"id":"call_glob","index":0,"type":"function","#,
            r#""function":{"name":"glob","arguments":"{"}}]}}]}"#
        );
        let fragments = ["\"pattern\":\"src/**/*.ts\"", "}"];
        let mut chunks = vec![first.to_string()];
        for fragment in fragments {
            let chunk = build_index_only_chunk(fragment);
            chunks.push(chunk);
        }
        for chunk in &chunks {
            parse_openai_chunk_into_events(chunk, &mut state);
        }
        let final_events = state.flush_all();
        let tool_call = final_events
            .iter()
            .find_map(|e| match e {
                LlmEvent::ToolCall { name, input, .. } => Some((name.clone(), input.clone())),
                _ => None,
            })
            .expect("expected ToolCall event");
        let (name, input) = tool_call;
        assert_eq!(name, "glob");
        let pattern = input
            .get("pattern")
            .and_then(|v| v.as_str())
            .expect("pattern should be a string");
        assert_eq!(pattern, "src/**/*.ts");
    }

    /// Regression: the production log showed a chunk that carries
    /// `finish_reason: "tool_calls"` together with a `<think>`
    /// `delta.content`. The parser must surface the finish reason
    /// (so the stream loop can stop accepting more deltas) and
    /// the text event (so the runner can render the reasoning).
    #[test]
    fn tool_stream_finish_reason_alongside_text_delta() {
        let mut state = OpenAiToolStreamState::default();
        let chunk = r#"{"choices":[{"finish_reason":"tool_calls","index":0,"delta":{"content":"\n</think>\n","role":"assistant"}}]}"#;
        let parsed = parse_openai_chunk_into_events(chunk, &mut state);
        // Finish reason must be surfaced as out-of-band metadata.
        assert!(
            matches!(parsed.finish_reason, Some(FinishReason::ToolCalls)),
            "expected finish_reason=ToolCalls, got: {:?}",
            parsed.finish_reason
        );
        // The `<think>` text must be emitted as a TextDelta so
        // the runner can route it to the think-block.
        let text_deltas: Vec<&str> = parsed
            .events
            .iter()
            .filter_map(|e| match e {
                LlmEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            text_deltas,
            vec!["\n</think>\n"],
            "expected single TextDelta for the think close, got: {parsed:?}"
        );
    }
}

fn lower_messages(request: &LlmRequest) -> Vec<serde_json::Value> {
    request
        .messages
        .iter()
        .map(|msg| {
            let role = match msg.role.as_str() {
                "user" => "user",
                "assistant" => "assistant",
                "system" => "system",
                "tool" => "tool",
                _ => "user",
            };
            serde_json::json!({ "role": role, "content": lower_content(&msg.content) })
        })
        .collect()
}

fn lower_content(content: &[ContentPart]) -> serde_json::Value {
    if content.is_empty() {
        return serde_json::Value::String(String::new());
    }

    let parts = content
        .iter()
        .filter_map(|part| {
            part.as_prompt_text()
                .map(|text| serde_json::json!({ "type": "text", "text": text }))
        })
        .collect::<Vec<_>>();

    if parts.len() == 1 && parts[0].get("type").and_then(|value| value.as_str()) == Some("text") {
        return serde_json::Value::String(
            parts[0]
                .get("text")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string(),
        );
    }

    serde_json::Value::Array(parts)
}
