use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

use super::error_parsing::{format_provider_error, parse_provider_error_body};
use super::{log_sse_event_body, openai_compatible_endpoint_url, LlmError, LlmProvider, LlmStream};
use crate::llm::events::{FinishReason, LlmEvent, ToolDefinition};
use crate::llm::providers::LlmStreamAbortHandle;
use crate::llm::request::{ContentPart, LlmRequest};

pub struct OpenAiCompatibleHttpProvider {
    provider_name: String,
    api_key_env: String,
    api_key: Option<String>,
    api_key_source: &'static str,
    base_url: String,
    supported_models: Vec<String>,
    http_client: reqwest::Client,
}

impl OpenAiCompatibleHttpProvider {
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
        let has_api_key = api_key.as_deref().is_some_and(|key| !key.trim().is_empty());
        if has_api_key {
            tracing::debug!(
                provider = %provider_name,
                base_url = %base_url,
                api_key_env = %api_key_env,
                api_key_source,
                has_api_key,
                "created OpenAI-compatible HTTP provider auth config"
            );
        } else {
            tracing::trace!(
                provider = %provider_name,
                base_url = %base_url,
                api_key_env = %api_key_env,
                api_key_source,
                has_api_key,
                "created OpenAI-compatible HTTP provider auth config"
            );
        }

        Self {
            provider_name: provider_name.to_string(),
            api_key_env: api_key_env.to_string(),
            api_key,
            api_key_source,
            base_url,
            supported_models: supported_models
                .iter()
                .map(|model| model.to_string())
                .collect(),
            http_client: crate::ssrf::provider_client().clone(),
        }
    }

    pub fn with_base_url(mut self, url: &str) -> Self {
        self.base_url = url.to_string();
        self
    }

    pub fn chat_completions_url(&self) -> String {
        openai_compatible_endpoint_url(&self.base_url, "chat/completions")
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
            tracing::debug!(
                provider = %self.provider_name,
                url = %self.chat_completions_url(),
                model,
                api_key_source = self.api_key_source,
                has_api_key = !api_key.trim().is_empty(),
                "attaching bearer auth header for endpoint status request"
            );
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
impl LlmProvider for OpenAiCompatibleHttpProvider {
    fn provider_name(&self) -> &str {
        &self.provider_name
    }

    fn supported_models(&self) -> Vec<String> {
        self.supported_models.clone()
    }

    fn backend_name(&self) -> &str {
        "http"
    }

    fn model_base_url(&self) -> Option<&str> {
        Some(&self.base_url)
    }

    fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    async fn stream(&self, request: LlmRequest) -> Result<LlmStream, LlmError> {
        let api_key = self.api_key.as_ref().ok_or_else(|| self.auth_error())?;
        // Default to the OpenAI-spec structured `role: "tool"`
        // form with `tool_call_id` + `name` on the wire. The
        // renderer is always the structured form; we never
        // synthesize `<system-reminder>` markup.
        let body = build_request_body(&self.provider_name, &request, lower_messages);

        tracing::debug!(
            provider = %self.provider_name,
            url = %self.chat_completions_url(),
            model = %request.model,
            api_key_source = self.api_key_source,
            has_api_key = !api_key.trim().is_empty(),
            tool_count = request.tools.len(),
            "attaching bearer auth header for llm request"
        );

        let (abort_tx, mut abort_rx) = LlmStreamAbortHandle::new();
        let auth_header = format!("Bearer {api_key}");
        let response = self
            .http_client
            .post(self.chat_completions_url())
            .header("Authorization", &auth_header)
            .header("Content-Type", "application/json")
            .header(
                "x-session-affinity",
                request.session_id.as_deref().unwrap_or(""),
            )
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
            // Most OpenAI-compatible providers return
            // `{"error":{"message":"…","type":"…","code":"…","param":"…"}}`
            // on a 4xx/5xx. Pull the structured fields out so the
            // user-visible error says exactly what the provider
            // rejected, instead of a wall-of-JSON. Falls back to
            // the raw body when the response isn't JSON.
            let structured = parse_provider_error_body(&text);
            tracing::error!(
                provider = %self.provider_name,
                url = %self.chat_completions_url(),
                model = %request.model,
                status = status.as_u16(),
                error_kind = %structured.kind,
                error_type = structured.error_type.as_deref().unwrap_or(""),
                error_code = structured.error_code.as_deref().unwrap_or(""),
                error_param = structured.error_param.as_deref().unwrap_or(""),
                error_message = %structured.message,
                response_body_chars = text.chars().count(),
                "llm http error: provider rejected the request"
            );
            let user_message = format_provider_error(&structured, &text);
            return Err(
                LlmError::new(&format!("http_{}", status.as_u16()), &user_message)
                    .provider(&self.provider_name)
                    .model(&request.model),
            );
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
            let mut termination_path = "upstream_eof_without_finish";
            let mut stream_error: Option<String> = None;

            // The HTTP stream is the single source of truth for this
            // provider turn. Tool results are produced by the runner
            // after the stream ends and are included in the next request
            // via persisted conversation history.
            loop {
                let chunk = tokio::select! {
                    changed = abort_rx.changed() => {
                        if changed.is_err() || *abort_rx.borrow() {
                            termination_path = "local_abort_signal";
                            tracing::warn!(
                                provider = %stream_provider,
                                model = %stream_model,
                                termination_path,
                                "llm http stream stopping because local abort signal was received"
                            );
                            break;
                        }
                        continue;
                    }
                    chunk = event_stream.next() => chunk,
                };

                let Some(chunk) = chunk else {
                    break;
                };

                if *abort_rx.borrow() {
                        termination_path = "local_abort_signal";
                        tracing::warn!(
                            provider = %stream_provider,
                            model = %stream_model,
                            termination_path,
                            "llm http stream stopping because local abort signal was received"
                        );
                        break;
                }

                let bytes = match chunk {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        termination_path = "byte_stream_error";
                        stream_error = Some(error.to_string());
                        tracing::warn!(
                            provider = %stream_provider,
                            model = %stream_model,
                            error = stream_error.as_deref().unwrap_or(""),
                            termination_path,
                            "llm http stream stopping because response byte stream returned an error"
                        );
                        break;
                    }
                };
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
                        tracing::info!(
                            provider = %stream_provider,
                            model = %stream_model,
                            sse_done = true,
                            "llm http stream observed SSE [DONE] terminal event"
                        );
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
                            tracing::info!(
                                provider = %stream_provider,
                                model = %stream_model,
                                finish_reason = ?reason,
                                "llm http stream observed provider finish_reason in SSE payload"
                            );
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
                    termination_path = if saw_done && finish_reason_seen.is_some() {
                        "sse_done_and_finish_reason"
                    } else if saw_done {
                        "sse_done"
                    } else {
                        "finish_reason"
                    };
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
                    tracing::info!(
                        provider = %stream_provider,
                        model = %stream_model,
                        termination_path,
                        sse_done = saw_done,
                        finish_reason = ?reason,
                        "llm http stream emitting Finish after provider terminal signal"
                    );
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
                tracing::warn!(
                    provider = %stream_provider,
                    model = %stream_model,
                    termination_path,
                    stream_error = stream_error.as_deref().unwrap_or(""),
                    "llm http stream ended without SSE [DONE] or finish_reason; emitting synthetic Finish(stop)"
                );
                yield LlmEvent::Finish {
                    reason: FinishReason::Stop,
                    usage: None,
                };
            } else {
                tracing::info!(
                    provider = %stream_provider,
                    model = %stream_model,
                    termination_path,
                    "llm http stream closed after provider terminal signal"
                );
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
    provider_name: &str,
    request: &LlmRequest,
    lower_messages: impl Fn(&LlmRequest) -> Vec<serde_json::Value>,
) -> serde_json::Value {
    let messages = lower_messages(request);
    let minimax = is_minimax_provider(provider_name);

    let mut body = serde_json::json!({
        "model": request.model,
        "messages": messages,
        "stream": true,
    });

    if !minimax {
        body["stream_options"] = serde_json::json!({ "include_usage": true });
    }

    if !minimax {
        if let Some(session_id) = &request.session_id {
            // `user` on Chat Completions is the documented cache-routing
            // hint: OpenAI uses it to bucket requests from the same end
            // user, which keeps the implicit prefix cache hot across
            // turns. OpenAI Responses uses `prompt_cache_key` instead;
            // both are passed through by the AI SDK OpenAI provider
            // when the chat-completions path picks them up. The header
            // is for upstream gateways that key on it.
            body["user"] = serde_json::json!(session_id);
        }
    }

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

    // Per-model output-token defaults. opencode's transform.ts
    // sets `reasoning_effort: "medium"` and `textVerbosity: "low"`
    // for GPT-5 family — textVerbosity is the single biggest
    // output-token lever because it forces terse prose. We
    // default to those values for the GPT-5 family and leave
    // other models alone.
    let model_lower = request.model.to_lowercase();
    if model_lower.contains("gpt-5") && !model_lower.contains("gpt-5-chat") {
        if !body
            .as_object()
            .map_or(true, |o| o.contains_key("reasoning_effort"))
        {
            if !model_lower.contains("gpt-5-pro") {
                body["reasoning_effort"] = serde_json::json!("medium");
            }
        }
        if model_lower.contains("gpt-5.")
            && !body
                .as_object()
                .map_or(true, |o| o.contains_key("verbosity"))
        {
            body["verbosity"] = serde_json::json!("low");
        }
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

fn is_minimax_provider(provider_name: &str) -> bool {
    provider_name.trim().eq_ignore_ascii_case("minimax")
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
        let body = build_request_body("openai", &sample_request(), |_req| {
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
        let body = build_request_body("openai", &req, |_| vec![]);
        assert!(
            body.get("tools").is_none(),
            "tools must not be sent when none are defined; body was: {body}"
        );
    }

    #[test]
    fn build_request_body_includes_messages_model_stream() {
        let body = build_request_body("openai", &sample_request(), |_req| {
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
        let body = build_request_body("openai", &req, |_| vec![]);
        assert_eq!(body["temperature"].as_f64().unwrap(), 0.5);
        assert_eq!(body["max_tokens"], 256);
        let top_p = body["top_p"].as_f64().unwrap();
        assert!((top_p - 0.9).abs() < 1e-5, "top_p was {top_p}");
        assert_eq!(body["stop"][0], "STOP");
    }

    #[test]
    fn minimax_request_body_omits_openai_optional_fields() {
        let req = LlmRequest::new("MiniMax-M3", "minimax")
            .with_message(LlmMessage::user("hi"))
            .with_session_id("session-1".to_string());
        let body = build_request_body("minimax", &req, |_req| {
            vec![serde_json::json!({"role": "user", "content": "hi"})]
        });

        assert!(body.get("stream_options").is_none(), "body was: {body}");
        assert!(body.get("user").is_none(), "body was: {body}");
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn assistant_tool_call_message_renders_structured_tool_calls_field() {
        // This is the regression test for "after a tool call,
        // every message returns 400". The previous wire renderer
        // passed tool calls as text content (XML-shaped) which
        // strict providers reject; the new renderer emits the
        // structured `tool_calls` field with canonical JSON
        // `arguments`.
        let assistant = LlmMessage {
            role: "assistant".to_string(),
            content: vec![ContentPart::ToolCall {
                id: "call_abc".to_string(),
                name: "read".to_string(),
                input: serde_json::json!({"path": "src/lib.rs"}),
            }],
        };
        let entry = lower_assistant_message(&assistant.content);
        assert_eq!(entry["role"], "assistant");
        assert!(
            entry.get("tool_calls").is_some(),
            "expected structured tool_calls; got: {entry}"
        );
        let tool_calls = entry["tool_calls"].as_array().expect("array");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_abc");
        assert_eq!(tool_calls[0]["type"], "function");
        assert_eq!(tool_calls[0]["function"]["name"], "read");
        // Canonical JSON: no whitespace, no escape issues, no
        // unquoted keys.
        assert_eq!(
            tool_calls[0]["function"]["arguments"],
            r#"{"path":"src/lib.rs"}"#
        );
    }

    #[test]
    fn tool_message_renders_structured_tool_call_id_and_content() {
        let tool = LlmMessage::tool("call_abc", "read", serde_json::json!({"text": "hello"}));
        let entry = lower_tool_message(&tool.content);
        assert_eq!(entry["role"], "tool");
        assert_eq!(entry["tool_call_id"], "call_abc");
        assert_eq!(entry["name"], "read");
        // Tool result content is the canonical JSON of the
        // result. OpenAI spec allows either a string or an
        // object here; we always emit the string form.
        assert_eq!(entry["content"], r#"{"text":"hello"}"#);
    }

    #[test]
    fn assistant_message_with_text_and_tool_call_keeps_both() {
        // The model can interleave text (e.g. a sentence) and a
        // tool call. The renderer must preserve both, not drop
        // the text in favor of the tool call.
        let assistant = LlmMessage {
            role: "assistant".to_string(),
            content: vec![
                ContentPart::Text {
                    text: "Let me read the file.".to_string(),
                },
                ContentPart::ToolCall {
                    id: "call_1".to_string(),
                    name: "read".to_string(),
                    input: serde_json::json!({"path": "/tmp/x"}),
                },
            ],
        };
        let entry = lower_assistant_message(&assistant.content);
        assert_eq!(entry["role"], "assistant");
        assert_eq!(entry["content"], "Let me read the file.");
        let tool_calls = entry["tool_calls"].as_array().expect("array");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(
            tool_calls[0]["function"]["arguments"],
            r#"{"path":"/tmp/x"}"#
        );
    }

    #[test]
    fn assistant_text_only_message_has_no_tool_calls_field() {
        // A pure-text assistant message must not carry a
        // `tool_calls` field — providers may reject the empty
        // array or the field's presence.
        let assistant = LlmMessage::assistant("All done.");
        let entry = lower_assistant_message(&assistant.content);
        assert_eq!(entry["role"], "assistant");
        assert_eq!(entry["content"], "All done.");
        assert!(
            entry.get("tool_calls").is_none(),
            "tool_calls must be absent on text-only messages; got: {entry}"
        );
    }

    #[test]
    fn assistant_tool_call_with_complex_input_renders_canonical_json() {
        // Inputs containing escapes, nested objects, and
        // unicode must be serialized canonically — the
        // pre-fix `serde_json::Value::Display` path produced
        // a value whose `to_string` was *not* valid JSON in
        // some edge cases.
        let assistant = LlmMessage {
            role: "assistant".to_string(),
            content: vec![ContentPart::ToolCall {
                id: "call_1".to_string(),
                name: "shell".to_string(),
                input: serde_json::json!({
                    "command": "echo \"hello, 世界\"",
                    "count": 3,
                    "nested": {"a": [1, 2, 3]},
                }),
            }],
        };
        let entry = lower_assistant_message(&assistant.content);
        let arguments = entry["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("string");
        // The arguments must parse as valid JSON.
        let parsed: serde_json::Value =
            serde_json::from_str(arguments).expect("arguments must be valid JSON");
        assert_eq!(parsed["command"], "echo \"hello, 世界\"");
        assert_eq!(parsed["count"], 3);
        assert_eq!(parsed["nested"]["a"][2], 3);
    }

    #[test]
    fn tool_message_renders_structured_role_tool() {
        // The default `role: "tool"` rendering must carry the
        // `tool_call_id` and `name` on the wire so providers can
        // correlate the result with the assistant's
        // `tool_calls[*].id`. The body is the canonical JSON
        // string of the result value.
        let tool = LlmMessage::tool("call_abc", "read", serde_json::json!({"text": "hello"}));
        let entry = lower_tool_message(&tool.content);
        assert_eq!(entry["role"], "tool");
        assert_eq!(entry["tool_call_id"], "call_abc");
        assert_eq!(entry["name"], "read");
        let content = entry["content"].as_str().expect("string content");
        assert_eq!(content, r#"{"text":"hello"}"#);
    }

    #[test]
    fn lower_messages_dispatches_per_role_with_structured_tool_results() {
        // Default dispatch: assistant stays structured (tool_calls
        // field), tool results become `role: "tool"` with
        // `tool_call_id` + `name` on the wire, plain user/system
        // messages pass through unchanged. `lower_messages` only
        // iterates `request.messages` — the system prompt is
        // rendered separately as the top-level `system` field
        // by `build_request_body`.
        let request = LlmRequest::new("MiniMax-M3", "minimax")
            .with_system("You are a helpful assistant.")
            .with_message(LlmMessage::user("read /etc/hostname"))
            .with_message(LlmMessage {
                role: "assistant".to_string(),
                content: vec![ContentPart::ToolCall {
                    id: "call_1".to_string(),
                    name: "read".to_string(),
                    input: serde_json::json!({"path": "/etc/hostname"}),
                }],
            })
            .with_message(LlmMessage::tool(
                "call_1",
                "read",
                serde_json::json!({"text": "host1\n"}),
            ));
        let messages = lower_messages(&request);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"], "read /etc/hostname");
        assert_eq!(messages[1]["role"], "assistant");
        assert!(messages[1].get("tool_calls").is_some());
        assert_eq!(messages[2]["role"], "tool");
        assert_eq!(messages[2]["tool_call_id"], "call_1");
        assert_eq!(messages[2]["name"], "read");
        let body = messages[2]["content"].as_str().expect("string");
        assert_eq!(body, r#"{"text":"host1\n"}"#);
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
        .map(|msg| match msg.role.as_str() {
            "tool" => lower_tool_message(&msg.content),
            "assistant" => lower_assistant_message(&msg.content),
            _ => serde_json::json!({
                "role": msg.role,
                "content": lower_text_content(&msg.content),
            }),
        })
        .collect()
}

/// Render an assistant message, preserving the structured
/// `tool_calls` field on the wire. The arguments are serialized
/// canonically via `serde_json::to_string` — never via
/// `serde_json::Value::Display`, which is not guaranteed to be
/// valid JSON and is the root cause of provider 4xx errors on
/// tool-call continuations.
fn lower_assistant_message(content: &[ContentPart]) -> serde_json::Value {
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<serde_json::Value> = Vec::new();
    for part in content {
        match part {
            ContentPart::ToolCall { id, name, input } => {
                let arguments = serde_json::to_string(input).unwrap_or_else(|_| "null".to_string());
                tool_calls.push(serde_json::json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": arguments,
                    },
                }));
            }
            ContentPart::Text { text } => {
                if !text.is_empty() {
                    text_parts.push(text.clone());
                }
            }
            // Reasoning and tool-result parts are not part of an
            // assistant message — they're handled elsewhere.
            _ => {}
        }
    }

    let mut entry = serde_json::Map::new();
    entry.insert(
        "role".to_string(),
        serde_json::Value::String("assistant".to_string()),
    );
    let text = text_parts.join("\n");
    if tool_calls.is_empty() {
        // Plain assistant text message: emit a string content
        // for OpenAI chat-completions compatibility.
        entry.insert("content".to_string(), serde_json::Value::String(text));
    } else {
        if text.is_empty() {
            entry.insert("content".to_string(), serde_json::Value::Null);
        } else {
            entry.insert("content".to_string(), serde_json::Value::String(text));
        }
        entry.insert(
            "tool_calls".to_string(),
            serde_json::Value::Array(tool_calls),
        );
    }
    serde_json::Value::Object(entry)
}

/// Render a `role: "tool"` message with the structured
/// `tool_call_id` field. The result's canonical-JSON string is
/// the `content` field. Both `tool_call_id` and `name` are sent so
/// strict providers (Minimax, OpenAI strict mode) can correlate
/// the result back to the original call.
fn lower_tool_message(content: &[ContentPart]) -> serde_json::Value {
    let mut tool_call_id: Option<String> = None;
    let mut tool_name: Option<String> = None;
    let mut result_text: Option<String> = None;
    for part in content {
        if let ContentPart::ToolResult { id, name, result } = part {
            tool_call_id = Some(id.clone());
            tool_name = Some(name.clone());
            result_text =
                Some(serde_json::to_string(result).unwrap_or_else(|_| "null".to_string()));
            break;
        }
    }
    let mut entry = serde_json::Map::new();
    entry.insert(
        "role".to_string(),
        serde_json::Value::String("tool".to_string()),
    );
    entry.insert(
        "content".to_string(),
        serde_json::Value::String(result_text.unwrap_or_default()),
    );
    if let Some(id) = tool_call_id {
        entry.insert("tool_call_id".to_string(), serde_json::Value::String(id));
    }
    if let Some(name) = tool_name {
        entry.insert("name".to_string(), serde_json::Value::String(name));
    }
    serde_json::Value::Object(entry)
}

/// Render plain text content (used for `user`, `system`, and
/// assistant messages that contain no `ToolCall` parts). The
/// OpenAI chat-completions API accepts a string `content` for
/// plain text; we use the string form to avoid the array form's
/// token overhead.
fn lower_text_content(content: &[ContentPart]) -> serde_json::Value {
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
