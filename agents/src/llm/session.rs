use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_stream::StreamExt;

use crate::domain::ToolCall;
use crate::llm::events::{FinishReason, LlmEvent, ToolResultValue};
use crate::llm::providers::LlmProvider;
use crate::llm::request::{ContentPart, LlmMessage, LlmRequest};
use crate::llm::subagent_registry::SubagentRegistry;
use crate::model_spec::ModelRegistry;
use crate::permission::PermissionService;
use crate::sessions::conversation::{ConversationMessage, ConversationService};
use crate::sessions::service::SessionService;
use crate::tools::{
    bound_tool_output, estimate_tokens, run_tool_async, run_tool_with_context, ToolContext,
    ToolRuntime, MAX_TOOL_OUTPUT_BYTES, MAX_TOOL_OUTPUT_LINES,
};

use crate::sessions::conversation::ConversationFile;

const MAX_STEPS: u32 = 25;

pub type SessionEventSink = Arc<dyn Fn(SessionRunEvent) + Send + Sync>;

#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionRunEvent {
    pub session_id: String,
    pub step: u32,
    pub event: LlmEvent,
}

pub struct SessionRunner {
    session_service: Arc<SessionService>,
    conversation_service: Arc<ConversationService>,
    providers: Arc<ProviderRegistry>,
    subagent_registry: Option<Arc<SubagentRegistry>>,
    max_steps: u32,
    readonly_tools: bool,
    /// Permission mode for this turn. The runner injects it into the
    /// LLM's context as a synthetic user message and uses it when
    /// gating tool calls in the dispatch block.
    mode: crate::permission::PermissionMode,
    event_sink: Option<SessionEventSink>,
    /// Per-session doom loop detector. Tracks the last N tool calls;
    /// if the same call repeats 3 times in a row the runner pauses
    /// to ask the user whether to continue, matching opencode's
    /// behavior.
    doom: Arc<crate::sandbox::DoomLoopDetector>,
    /// Optional permission service used for doom-loop user prompts
    /// (and any other "ask" flows that the v1 mode toggle doesn't
    /// cover). When `None`, the runner falls back to a hard error
    /// on doom loop so the LLM sees a clear failure rather than an
    /// infinite loop.
    permissions: Option<Arc<crate::permission_v2::PermissionService>>,
    /// Model registry used to look up per-model context windows
    /// for request-size pre-checks. Opencode uses the registered
    /// `model.limit.context` similarly to decide when to compact.
    model_registry: Arc<ModelRegistry>,
    /// Optional compactor. When set, the runner will attempt LLM-
    /// driven auto-compaction when the assembled request would
    /// exceed the model context window. When unset, overflow
    /// surfaces as `SessionError::ContextOverflow` so the UI can
    /// trigger manual compaction.
    compactor: Option<Arc<crate::sessions::CompactionService>>,
}

impl SessionRunner {
    pub fn new(
        session_service: Arc<SessionService>,
        conversation_service: Arc<ConversationService>,
        providers: Arc<ProviderRegistry>,
    ) -> Self {
        Self::with_model_registry(
            session_service,
            conversation_service,
            providers,
            Arc::new(ModelRegistry::from_embedded_json()),
        )
    }

    /// Construct a runner with an explicit model registry. Used by
    /// tests and by callers that want to override the default
    /// embedded registry.
    pub fn with_model_registry(
        session_service: Arc<SessionService>,
        conversation_service: Arc<ConversationService>,
        providers: Arc<ProviderRegistry>,
        model_registry: Arc<ModelRegistry>,
    ) -> Self {
        Self {
            session_service,
            conversation_service,
            providers,
            subagent_registry: None,
            max_steps: MAX_STEPS,
            readonly_tools: false,
            mode: crate::permission::PermissionMode::default(),
            event_sink: None,
            doom: Arc::new(crate::sandbox::DoomLoopDetector::default()),
            permissions: None,
            model_registry,
            compactor: None,
        }
    }

    pub fn with_event_sink(mut self, event_sink: SessionEventSink) -> Self {
        self.event_sink = Some(event_sink);
        self
    }

    /// Cap the number of LLM turns a single `run` invocation may take.
    /// Used by the `task` and `ask_peer` tools to bound child sessions.
    pub fn with_max_turns(mut self, max_turns: u32) -> Self {
        self.max_steps = max_turns;
        self
    }

    /// Restrict this runner's tools to the read-only subset used by
    /// `ask_peer` children. The runner drops `write`, `edit`,
    /// `apply_patch`, `shell`, `task`, and `ask_peer` from the tool
    /// definitions it advertises to the LLM.
    pub fn with_readonly_tools(mut self, readonly: bool) -> Self {
        self.readonly_tools = readonly;
        self
    }

    /// Set the permission mode for this turn. The mode is injected into
    /// the LLM context as a synthetic user message and used to gate
    /// tool-call dispatch.
    pub fn with_mode(mut self, mode: crate::permission::PermissionMode) -> Self {
        self.mode = mode;
        self
    }

    /// Wire the SubagentRegistry into the runner so the `task` and
    /// `ask_peer` tools have access. The agent service calls this on
    /// its top-level runner; the `task`/`ask_peer` tools build their
    /// own runners without it.
    pub fn with_subagent_registry(mut self, registry: Arc<SubagentRegistry>) -> Self {
        self.subagent_registry = Some(registry);
        self
    }

    /// Inject a custom doom loop detector. Useful for tests and for
    /// agents that want a different threshold than the default of 3.
    pub fn with_doom_detector(mut self, doom: Arc<crate::sandbox::DoomLoopDetector>) -> Self {
        self.doom = doom;
        self
    }

    /// Inject the v2 permission service. When set, the runner routes
    /// doom-loop and other "ask" flows through it. When unset, doom
    /// loops surface as a hard error to the LLM.
    pub fn with_permissions(
        mut self,
        permissions: Arc<crate::permission_v2::PermissionService>,
    ) -> Self {
        self.permissions = Some(permissions);
        self
    }

    /// Wire a `CompactionService` so the runner auto-compacts when
    /// the assembled request would exceed the model context window.
    /// Mirrors opencode's `runner.runTurn` -> `compaction.compactIfNeeded`
    /// -> `compactAfterOverflow` flow.
    pub fn with_compactor(mut self, compactor: Arc<crate::sessions::CompactionService>) -> Self {
        self.compactor = Some(compactor);
        self
    }

    /// Manual entrypoint for the LLM-driven compactor. Mirrors
    /// `SessionCompaction.process` in opencode. Returns the
    /// structured outcome so the Tauri command can surface a
    /// status to the UI. The caller (Tauri command) supplies the
    /// `CompactionRequest` because it already has the session row
    /// in hand.
    pub async fn run_compaction(
        &self,
        request: crate::sessions::CompactionRequest,
    ) -> Result<crate::sessions::CompactionOutcome, String> {
        let compactor = self
            .compactor
            .as_ref()
            .ok_or_else(|| "compactor not configured".to_string())?;
        Ok(compactor.compact_now(request).await)
    }

    fn emit_event(&self, session_id: &str, step: u32, event: &LlmEvent) {
        if let Some(sink) = &self.event_sink {
            sink(SessionRunEvent {
                session_id: session_id.to_string(),
                step,
                event: event.clone(),
            });
        }
    }

    pub async fn run(&self, session_id: &str) -> Result<RunResult, SessionError> {
        let mut step = 0u32;
        let mut attempted_compaction = false;

        // Opencode-style loop: re-query the persisted conversation
        // history on each iteration. The runner decides whether to
        // continue based on whether the most recent assistant message
        // has any unfulfilled tool calls (i.e. a `ToolCall` content
        // part with no matching `ToolResult` part in the same message).
        // This is more robust than trusting the LLM's `FinishReason`
        // because some providers return "stop" even when the
        // assistant emitted tool calls.
        while step < self.max_steps {
            match self.run_turn(session_id, step).await {
                Ok(continue_loop) => {
                    step += 1;
                    if !continue_loop {
                        break;
                    }
                }
                Err(SessionError::ContextOverflow { .. }) if !attempted_compaction => {
                    // Mirror opencode's `compaction.compactIfNeeded`
                    // + `compactAfterOverflow`: ask the LLM to
                    // summarize the conversation, then retry the
                    // turn. We attempt auto-compaction at most once
                    // per `run` call to avoid loops when compaction
                    // itself cannot shrink the conversation enough.
                    if let Some(compactor) = &self.compactor {
                        let session = self
                            .session_service
                            .get_session(session_id)
                            .map_err(|e| SessionError::Conversation(e))?
                            .ok_or_else(|| SessionError::SessionNotFound(session_id.to_string()))?;
                        attempted_compaction = true;
                        match compactor
                            .compact_now(crate::sessions::CompactionRequest {
                                session_id: session_id.to_string(),
                                provider: session.provider,
                                model: session.model,
                            })
                            .await
                        {
                            crate::sessions::CompactionOutcome::Compacted { .. } => {
                                tracing::info!(
                                    session_id = %session_id,
                                    step = step,
                                    "auto-compaction succeeded; retrying turn"
                                );
                                continue;
                            }
                            outcome => {
                                tracing::warn!(
                                    session_id = %session_id,
                                    step = step,
                                    outcome = ?outcome,
                                    "auto-compaction did not shrink the conversation; returning ContextOverflow"
                                );
                                return Err(SessionError::ContextOverflow {
                                    session_id: session_id.to_string(),
                                    estimated_tokens: 0,
                                    context_window: 0,
                                });
                            }
                        }
                    } else {
                        return Err(SessionError::ContextOverflow {
                            session_id: session_id.to_string(),
                            estimated_tokens: 0,
                            context_window: 0,
                        });
                    }
                }
                Err(other) => return Err(other),
            }
        }

        if self.has_unfulfilled_tool_calls(session_id) {
            return Err(SessionError::StepLimitExceeded {
                session_id: session_id.to_string(),
                limit: self.max_steps,
            });
        }

        Ok(RunResult { steps: step })
    }

    /// Inspect the most recent assistant message in the persisted
    /// conversation. If it contains any `ToolCall` part without a
    /// matching `ToolResult` part (by tool-call id), the LLM declared
    /// intent to use a tool but the runner never completed it. This
    /// typically means the stream ended mid-tool-call (e.g. truncation
    /// or an unexpected close) and the LLM will never see the result.
    /// Returning `true` here means the loop should NOT continue.
    fn has_unfulfilled_tool_calls(&self, session_id: &str) -> bool {
        let messages = match self.conversation_service.get_messages(session_id) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("failed to read history for loop guard: {e}");
                return false;
            }
        };

        let last_assistant = messages.iter().rev().find(|m| m.role == "assistant");
        let Some(last) = last_assistant else {
            return false;
        };

        let parts: Vec<ContentPart> = match serde_json::from_str(&last.content) {
            Ok(p) => p,
            Err(_) => return false,
        };

        let mut called: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut answered: std::collections::HashSet<String> = std::collections::HashSet::new();
        for part in &parts {
            match part {
                ContentPart::ToolCall { id, .. } => {
                    called.insert(id.clone());
                }
                ContentPart::ToolResult { id, .. } => {
                    answered.insert(id.clone());
                }
                _ => {}
            }
        }
        called.difference(&answered).next().is_some()
    }

    async fn run_turn(&self, session_id: &str, step: u32) -> Result<bool, SessionError> {
        let session = self
            .session_service
            .get_session(session_id)
            .map_err(|e| SessionError::Conversation(e))?
            .ok_or_else(|| SessionError::SessionNotFound(session_id.to_string()))?;

        // Drain completed sub-agent results into the conversation so the
        // LLM sees them as ordinary context on this turn.
        if let Some(registry) = &self.subagent_registry {
            let completions = registry.take_completions(session_id);
            for c in completions {
                let prefix = match c.kind {
                    crate::llm::subagent_registry::ChildKind::Task => "task_result",
                    crate::llm::subagent_registry::ChildKind::AskPeer => "peer_answer",
                };
                let block = format!(
                    "<{prefix} id=\"{cid}\" state=\"{st}\">\n{text}\n</{prefix}>",
                    prefix = prefix,
                    cid = c.child_session_id,
                    st = if c.success { "completed" } else { "error" },
                    text = c.text,
                );
                if let Err(e) = self.conversation_service.append_message(
                    session_id,
                    crate::MessageRole::User,
                    block,
                ) {
                    tracing::warn!("failed to append child completion: {e}");
                }
            }
        }

        let history = self
            .conversation_service
            .get_messages(session_id)
            .map_err(|e| SessionError::Conversation(e))?;

        // Read the conversation file directly so we can also surface
        // the persisted compaction summary (if any) as a synthetic
        // system message. `get_messages` returns only the messages
        // array; summary lives at the top level of the file.
        let conversation: ConversationFile = self
            .conversation_service
            .read_ai_conversation(session_id)
            .map_err(SessionError::Conversation)?;

        let mut messages = history_to_llm_messages(&history);

        if let Some(summary) = conversation.summary.as_deref() {
            if !summary.trim().is_empty() {
                messages.push(LlmMessage::system(&format!(
                    "Conversation summary:\n\n{summary}"
                )));
            }
        }

        // Inject the active permission mode as a synthetic user message
        // so the LLM knows what behaviour to follow. The mode is set
        // per-turn from the chat UI's in-memory toggle and is not
        // persisted.
        let mode = self.mode;
        let mode_guidance = match mode {
            crate::permission::PermissionMode::Plan => "\
                only read-only tools (read, glob, grep, webfetch, websearch) are allowed. \
                Mutating tools (write, edit, apply_patch, shell) will be blocked. \
                You may read files, search, and fetch the web to gather context, but you must not call write/edit/apply_patch/shell. \
                If the user asks for a change, describe the change in detail and request the user switch to build mode",
            crate::permission::PermissionMode::Build => "\
                all tools are allowed, including write, edit, apply_patch, and shell. You may make changes to the filesystem",
        };
        messages.push(LlmMessage::system(&format!(
            "Permission mode: {mode}\n\n\
             You are in {mode} mode. In {mode} mode, {mode_guidance}. \
             Tool calls that violate the active mode will be rejected by the \
             runtime; plan accordingly.",
        )));

        let system_prompt = system_prompt_for_session(&session.provider, &[]);

        // Pre-check the assembled request against the model context
        // window. Opencode estimates the same way and refuses to
        // dispatch when input+output would exceed the budget. We
        // surface a typed error so the UI can trigger compaction.
        let spec = self
            .model_registry
            .lookup(&session.provider, &session.model);
        let estimated_tokens: usize = messages
            .iter()
            .map(|m| {
                let chars: usize = m
                    .content
                    .iter()
                    .map(|p| match p {
                        ContentPart::Text { text } => text.len(),
                        ContentPart::Reasoning { text } => text.len(),
                        ContentPart::ToolCall { input, .. } => {
                            serde_json::to_string(input).map(|s| s.len()).unwrap_or(0)
                        }
                        ContentPart::ToolResult { result, .. } => {
                            serde_json::to_string(result).map(|s| s.len()).unwrap_or(0)
                        }
                    })
                    .sum();
                estimate_tokens(&" ".repeat(chars))
            })
            .sum();
        let token_budget = spec.context_window.saturating_sub(spec.max_output);
        if spec.context_window > 0 && estimated_tokens > token_budget {
            tracing::warn!(
                session_id = %session_id,
                provider = %session.provider,
                model = %session.model,
                estimated_tokens,
                context_window = spec.context_window,
                max_output = spec.max_output,
                "estimated request exceeds model context window; refusing to dispatch"
            );
            return Err(SessionError::ContextOverflow {
                session_id: session_id.to_string(),
                estimated_tokens,
                context_window: spec.context_window,
            });
        }

        let tools = if self.readonly_tools {
            readonly_tool_definitions()
        } else {
            default_tool_definitions()
        };

        // The LLM is told about tools via the request body's
        // structured `tools` field. Providers return tool calls as
        // structured events (`delta.tool_calls` for OpenAI-Chat,
        // `content_block` of `type: "tool_use"` for Anthropic), which
        // the provider stream parsers translate into
        // `LlmEvent::ToolInput*` / `LlmEvent::ToolCall` events. We
        // do NOT inject an XML tool description into the system
        // prompt; that would be contradictory.

        // Log the assembled prompt so debug runs can see exactly
        // what we sent the model: the system prompt (with mode
        // prefix), the message history, and the structured tool
        // catalog. Truncated to 2 KiB so the line doesn't blow up
        // the log when the conversation is long.
        let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        tracing::info!(
            session_id = %session_id,
            step = step,
            provider = %session.provider,
            model = %session.model,
            mode = ?self.mode,
            tool_count = tools.len(),
            tools = ?tool_names,
            system_prompt_bytes = system_prompt.len(),
            message_count = messages.len(),
            system_prompt_preview = %system_prompt.chars().take(2048).collect::<String>(),
            "llm request assembled: sending to provider"
        );

        let request = LlmRequest::new(&session.model, &session.provider)
            .with_system(system_prompt.clone())
            .with_messages(messages)
            .with_tools(tools);

        let provider = self
            .providers
            .get(&session.provider)
            .await
            .ok_or(SessionError::NoProviderForSession)?;

        let stream = provider
            .stream(request)
            .await
            .map_err(|e| SessionError::Llm(e))?;

        tracing::info!(
            session_id = %session_id,
            step = step,
            provider = %session.provider,
            model = %session.model,
            history_messages = history.len(),
            "llm stream opened"
        );

        // Stable ID for the assistant message we're about to build.
        // Used to flush parts to disk inline (so a process crash
        // mid-turn leaves a coherent partial message rather than
        // losing the entire turn). The LLM sees tool results
        // through this persisted message on the next turn.
        let assistant_message_id = format!("assistant-{}", uuid::Uuid::new_v4());

        let mut pending_tool_calls: Vec<(String, String, String)> = Vec::new();
        // Buffer for text deltas. Used to detect <think>…</think>
        // markers in models that emit thinking as raw text instead
        // of via the structured `ReasoningDelta` channel (some
        // local/open-weight models). We don't parse tool calls out
        // of this buffer — tool calls arrive as structured
        // `LlmEvent::ToolCall` events from the provider.
        let mut text_buffer = String::new();
        let mut in_think_block = false;
        let mut assistant_parts: Vec<ContentPart> = Vec::new();
        let mut needs_continuation = false;
        let mut finish_reason = FinishReason::Unknown;

        // Helper to flush the current `assistant_parts` snapshot to
        // the conversation file under `assistant_message_id`. Errors
        // are logged but not fatal: a write failure here doesn't
        // stop the runner; the final flush at the end will retry.
        let flush_parts = |parts: &[ContentPart]| -> Result<(), String> {
            let content = serde_json::to_string(parts).unwrap_or_else(|_| "[]".to_string());
            let content_size = content.len();
            let res = self.conversation_service.upsert_message_content(
                session_id,
                &assistant_message_id,
                crate::domain::MessageRole::Assistant,
                &content,
            );
            // Only log the success path at debug level; the failure
            // path is logged as a warning below by the caller so we
            // don't double-log on every event.
            if res.is_ok() {
                tracing::debug!(
                    session_id = %session_id,
                    message_id = %assistant_message_id,
                    parts = parts.len(),
                    bytes = content_size,
                    "inline-persisted assistant parts snapshot to conversation file"
                );
            }
            res
        };

        tokio::pin!(stream);

        while let Some(event) = stream.events.as_mut().next().await {
            self.emit_event(session_id, step, &event);

            let event_kind = match &event {
                LlmEvent::StepStart { .. } => "step_start",
                LlmEvent::StepFinish { .. } => "step_finish",
                LlmEvent::TextStart { .. } => "text_start",
                LlmEvent::TextDelta { .. } => "text_delta",
                LlmEvent::TextEnd { .. } => "text_end",
                LlmEvent::ReasoningStart { .. } => "reasoning_start",
                LlmEvent::ReasoningDelta { .. } => "reasoning_delta",
                LlmEvent::ReasoningEnd { .. } => "reasoning_end",
                LlmEvent::ToolInputStart { .. } => "tool_input_start",
                LlmEvent::ToolInputDelta { .. } => "tool_input_delta",
                LlmEvent::ToolInputEnd { .. } => "tool_input_end",
                LlmEvent::ToolCall { .. } => "tool_call",
                LlmEvent::ToolResult { .. } => "tool_result",
                LlmEvent::ToolError { .. } => "tool_error",
                LlmEvent::Finish { .. } => "finish",
                LlmEvent::ProviderError { .. } => "provider_error",
                LlmEvent::TaskCall { .. } => "task_call",
                LlmEvent::TaskResult { .. } => "task_result",
            };
            tracing::debug!(
                session_id = %session_id,
                step = step,
                event_kind = event_kind,
                event = %serde_json::to_string(&event).unwrap_or_else(|_| "<unserializable>".to_string()),
                "llm stream event"
            );

            match event {
                LlmEvent::TextDelta { text, .. } => {
                    // Buffer text so we can still detect <think>…</think>
                    // markers for models that emit thinking as raw
                    // text. The buffered text is flushed as visible
                    // text or as `Reasoning` parts at TextEnd (or
                    // stream end) — never as tool calls.
                    text_buffer.push_str(&text);
                }
                LlmEvent::TextEnd { .. } => {
                    flush_text_buffer(&mut text_buffer, &mut in_think_block, &mut assistant_parts);
                }
                LlmEvent::ReasoningDelta { text, .. } => {
                    if !text.is_empty() {
                        assistant_parts.push(ContentPart::reasoning(&text));
                    }
                }
                LlmEvent::ToolCall {
                    id,
                    name,
                    input,
                    provider_executed: _,
                } => {
                    needs_continuation = true;
                    let input_str = serde_json::to_string(&input).unwrap_or_default();
                    assistant_parts.push(ContentPart::tool_call(&id, &name, input.clone()));
                    pending_tool_calls.push((id, name, input_str));
                    tracing::info!(
                        session_id = %session_id,
                        step = step,
                        tool = %pending_tool_calls.last().map(|(_, n, _)| n.as_str()).unwrap_or("?"),
                        tool_call_id = %pending_tool_calls.last().map(|(i, _, _)| i.as_str()).unwrap_or("?"),
                        source = "structured",
                        "received structured tool call from provider"
                    );
                }
                LlmEvent::ToolResult {
                    id, name, result, ..
                } => {
                    let result_json = match result {
                        ToolResultValue::Text { value } => serde_json::json!({ "text": value }),
                        ToolResultValue::Error { value } => serde_json::json!({ "error": value }),
                        ToolResultValue::Json { value } => value,
                        ToolResultValue::Content { value } => {
                            serde_json::json!({ "content": value })
                        }
                    };
                    assistant_parts.push(ContentPart::tool_result(&id, &name, result_json));
                }
                LlmEvent::StepFinish { reason, .. } => {
                    finish_reason = reason;
                }
                LlmEvent::Finish { reason, .. } => {
                    finish_reason = reason;
                }
                LlmEvent::ToolError { id, name, message } => {
                    let result = serde_json::json!({ "error": message });
                    assistant_parts.push(ContentPart::tool_result(&id, &name, result));
                }
                LlmEvent::ProviderError { message } => {
                    tracing::error!("provider error during step {}: {}", step, message);
                    // Persist whatever we have before bailing so the
                    // user doesn't lose the partial turn.
                    let _ = flush_parts(&assistant_parts);
                    return Err(SessionError::Provider(message));
                }
                _ => {}
            }

            // Inline-persist the latest finalized parts. Raw text is
            // intentionally finalized at TextEnd so XML tools are parsed
            // from the complete text segment instead of per delta.
            if !assistant_parts.is_empty() {
                if let Err(e) = flush_parts(&assistant_parts) {
                    tracing::warn!(
                        session_id = %session_id,
                        step = step,
                        error = %e,
                        "inline persistence failed; will retry at end of turn"
                    );
                }
            }
        }

        if !text_buffer.is_empty() || in_think_block {
            flush_text_buffer(&mut text_buffer, &mut in_think_block, &mut assistant_parts);
        }

        // After the stream is done, dispatch the pending tool calls.
        // Tool results are appended to `assistant_parts` so the
        // persisted assistant message contains both the call and
        // the result, and the next turn's LLM request sees them
        // through the standard message-history path on the next turn.
        if !pending_tool_calls.is_empty() {
            let permission = PermissionService::new(self.mode);
            let project_root = std::path::PathBuf::from(&session.directory);
            let ctx = ToolContext::new(self.mode).with_project_root(project_root.clone());
            tracing::info!(
                session_id = %session_id,
                step = step,
                count = pending_tool_calls.len(),
                mode = ?self.mode,
                session_directory = %session.directory,
                project_root = %project_root.display(),
                project_root_is_empty = project_root.as_os_str().is_empty(),
                cwd = %std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "<unknown>".to_string()),
                "dispatching tool calls: built ToolContext with project_root from session"
            );

            for (id, name, input_str) in pending_tool_calls {
                let input: serde_json::Value = serde_json::from_str(&input_str).unwrap_or_default();
                let arguments = if let Some(obj) = input.as_object() {
                    obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                } else {
                    std::collections::HashMap::new()
                };
                let tool_call = ToolCall {
                    name: name.clone(),
                    arguments: arguments.clone(),
                };

                self.emit_event(
                    session_id,
                    step,
                    &LlmEvent::ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                        provider_executed: Some(false),
                    },
                );

                tracing::debug!(
                    session_id = %session_id,
                    step = step,
                    tool_call_id = %id,
                    tool = %name,
                    args = ?tool_call.arguments,
                    mode = ?self.mode,
                    doom_history_len = self.doom.history_len(),
                    "dispatching tool call"
                );

                // Doom-loop detection. If the same call has been
                // made 3 times in a row, ask the user (via the v2
                // permission service) whether to continue. The v2
                // default ruleset has `doom_loop: ask`, so this
                // surfaces a real prompt in the Tauri UI.
                if self.doom.record(&name, &input_str) {
                    tracing::warn!(
                        session_id = %session_id,
                        step = step,
                        tool = %name,
                        tool_call_id = %id,
                        "doom loop detected: same tool call repeated 3 times"
                    );

                    let doom_approved = if let Some(permissions) = &self.permissions {
                        // The runner is async; the v2 service exposes
                        // both sync (`check`) and async (`check_async`)
                        // entry points. We're in an async context.
                        use crate::permission_v2::{CheckError, CheckRequest};
                        let request = CheckRequest {
                            permission: "doom_loop".to_string(),
                            pattern: name.clone(),
                            tool: name.clone(),
                            always: vec![name.clone()],
                            request_id: None,
                        };
                        match permissions.check_async(request).await {
                            Ok(_) => true,
                            Err(CheckError::Rejected { .. }) => false,
                            Err(CheckError::Denied { .. }) => false,
                            Err(_) => false,
                        }
                    } else {
                        // No v2 service wired up: hard-stop. This is
                        // the safe default — better to surface a
                        // clear error than to silently burn tokens in
                        // an infinite loop.
                        false
                    };

                    if doom_approved {
                        tracing::info!(
                            session_id = %session_id,
                            step = step,
                            tool = %name,
                            "user approved doom-loop continuation; resetting detector"
                        );
                        self.doom.reset();
                        // Fall through and execute the call.
                    } else {
                        // User (or default policy) rejected: surface
                        // a `ToolError` so the LLM gets feedback and
                        // can try a different approach.
                        let message = format!(
                            "doom loop: the same `{name}` call has been made 3 times in a row. \
                             The user did not approve continuing. Try a different approach."
                        );
                        let event = LlmEvent::ToolError {
                            id: id.clone(),
                            name: name.clone(),
                            message: message.clone(),
                        };
                        self.emit_event(session_id, step, &event);
                        assistant_parts.push(ContentPart::tool_result(
                            &id,
                            &name,
                            serde_json::json!({ "error": message }),
                        ));
                        tracing::info!(
                            session_id = %session_id,
                            step = step,
                            tool = %name,
                            tool_call_id = %id,
                            "appended doom-loop ToolError to assistant parts"
                        );
                        let _ = flush_parts(&assistant_parts);
                        // Reset the detector so the next call from
                        // the LLM gets a fresh window.
                        self.doom.reset();
                        continue;
                    }
                }

                if let Err(err) = permission.assert_tool_call(&tool_call) {
                    let event = LlmEvent::ToolError {
                        id: id.clone(),
                        name: name.clone(),
                        message: err.to_string(),
                    };
                    self.emit_event(session_id, step, &event);
                    assistant_parts.push(ContentPart::tool_result(
                        &id,
                        &name,
                        serde_json::json!({ "error": err.to_string() }),
                    ));
                    tracing::warn!(
                        session_id = %session_id,
                        step = step,
                        tool = %name,
                        tool_call_id = %id,
                        error = %err,
                        "tool call denied by v1 permission service; appending ToolError"
                    );
                    let _ = flush_parts(&assistant_parts);
                    continue;
                }

                // Dispatch through the async path so the `task` and
                // `ask_peer` tools can drive child sessions. For
                // read-only runs (e.g. an `ask_peer` child), the runner
                // has no ToolRuntime; it falls back to the sync path
                // inside `run_tool_async` for non-task tools, but task
                // / ask_peer are unreachable because the tool set is
                // already filtered.
                let result = if let Some(registry) = &self.subagent_registry {
                    let runtime = ToolRuntime {
                        caller_session_id: session_id.to_string(),
                        session_service: Arc::clone(&self.session_service),
                        conversation_service: Arc::clone(&self.conversation_service),
                        providers: Arc::clone(&self.providers),
                        subagent_registry: Arc::clone(registry),
                        project_root: project_root.clone(),
                    };
                    tracing::debug!(
                        session_id = %session_id,
                        step = step,
                        tool = %name,
                        project_root = %project_root.display(),
                        "dispatching via async path (subagent registry present)"
                    );
                    run_tool_async(&tool_call, &runtime).await
                } else {
                    tracing::debug!(
                        session_id = %session_id,
                        step = step,
                        tool = %name,
                        project_root = %project_root.display(),
                        "dispatching via sync path"
                    );
                    run_tool_with_context(&tool_call, &ctx)
                };

                let (result_value, output) = if result.success {
                    let output = result.output.clone();
                    (serde_json::json!({ "text": output }), Some(result.output))
                } else {
                    let error = result.error.unwrap_or_default();
                    (serde_json::json!({ "error": error }), None)
                };

                // Bound the persisted tool result so a single tool
                // call cannot blow the model context window. The
                // bounded string is what we store on the assistant
                // message and what the LLM sees on subsequent
                // turns; the live event keeps the full output so
                // the UI can still display the untruncated result.
                let bounded = if result.success {
                    let out = bound_tool_output(output.as_deref().unwrap_or(""));
                    if out.truncated {
                        crate::tools::output_bounds::note_truncation();
                    }
                    out
                } else {
                    crate::tools::BoundedOutput {
                        text: output.as_deref().unwrap_or("").to_string(),
                        truncated: false,
                    }
                };
                let persisted_output = if bounded.truncated {
                    Some(bounded.text.clone())
                } else {
                    output.clone()
                };
                let result_value = if bounded.truncated {
                    serde_json::json!({
                        "text": bounded.text,
                        "truncated": true,
                        "max_lines": MAX_TOOL_OUTPUT_LINES,
                        "max_bytes": MAX_TOOL_OUTPUT_BYTES,
                    })
                } else {
                    result_value
                };

                let event = LlmEvent::ToolResult {
                    id: id.clone(),
                    name: name.clone(),
                    result: ToolResultValue::Json {
                        value: result_value.clone(),
                    },
                    output: persisted_output,
                };
                self.emit_event(session_id, step, &event);
                assistant_parts.push(ContentPart::tool_result(&id, &name, result_value.clone()));
                let tool_result_size = assistant_parts.len();
                tracing::debug!(
                    session_id = %session_id,
                    step = step,
                    tool = %name,
                    tool_call_id = %id,
                    success = result.success,
                    parts_after = tool_result_size,
                    "tool result appended to assistant parts"
                );
                let _ = flush_parts(&assistant_parts);
            }
        }

        tracing::info!(
            session_id = %session_id,
            step = step,
            parts = assistant_parts.len(),
            finish_reason = ?finish_reason,
            needs_continuation = needs_continuation,
            "llm turn finished"
        );

        if assistant_parts.is_empty() {
            tracing::info!(
                session_id = %session_id,
                step = step,
                finish_reason = ?finish_reason,
                needs_continuation = needs_continuation,
                "llm turn produced no assistant output; skipping assistant message persist"
            );
            return Ok(needs_continuation);
        }

        // Final flush (also covers the case where inline flushes
        // failed earlier). This is the only persistence path the
        // pre-refactor code had; we keep it for belt-and-suspenders
        // safety even though the inline flushes above should have
        // already written the latest snapshot.
        let final_content =
            serde_json::to_string(&assistant_parts).unwrap_or_else(|_| "[]".to_string());
        let final_size = final_content.len();
        match self.conversation_service.upsert_message_content(
            session_id,
            &assistant_message_id,
            crate::domain::MessageRole::Assistant,
            &final_content,
        ) {
            Ok(()) => tracing::info!(
                session_id = %session_id,
                message_id = %assistant_message_id,
                step = step,
                parts = assistant_parts.len(),
                bytes = final_size,
                "final-flush of assistant message succeeded"
            ),
            Err(ref e) => {
                tracing::error!(
                    session_id = %session_id,
                    message_id = %assistant_message_id,
                    step = step,
                    error = %e,
                    "failed to persist assistant message"
                );
                return Err(SessionError::Conversation(e.clone()));
            }
        }

        // Continue after any tool call or tool-parse error so the next
        // LLM turn can see the persisted call/result transcript.
        let continue_loop = needs_continuation;

        Ok(continue_loop)
    }
}

fn history_to_llm_messages(history: &[ConversationMessage]) -> Vec<LlmMessage> {
    history
        .iter()
        .filter(|msg| !msg.content.trim().is_empty())
        .map(|msg| match msg.role.as_str() {
            "user" => LlmMessage::user(&msg.content),
            "assistant" => {
                if let Ok(parts) = serde_json::from_str::<Vec<ContentPart>>(&msg.content) {
                    LlmMessage {
                        role: "assistant".to_string(),
                        content: parts,
                    }
                } else {
                    LlmMessage::assistant(&msg.content)
                }
            }
            "system" => LlmMessage::system(&msg.content),
            _ => LlmMessage::user(&msg.content),
        })
        .collect()
}

fn system_prompt_for_session(provider: &str, _extra: &[String]) -> String {
    let agent_name = match provider {
        "anthropic" => "Claude",
        "openai" => "GPT",
        "minimax" => "MiniMax",
        _ => "AI Assistant",
    };

    // Mirrors the opencode / claude-code style: short, direct,
    // CLI-oriented. The list of available tools is sent on the
    // request as a structured `tools` field; the model is expected
    // to use the provider's native tool-calling API.
    format!(
        "You are {name}, the Arachne coding agent — an interactive CLI tool that \
         helps users with software engineering tasks. Use the instructions below and the \
         tools available to you to assist the user.\n\n\
         \
         IMPORTANT: You must NEVER generate or guess URLs for the user unless you are \
         confident that the URLs are for helping the user with programming. You may use \
         URLs provided by the user in their messages or local files.\n\n\
         \
         # Tone and style\n\
         - Only use emojis if the user explicitly requests it.\n\
         - Be concise, direct, and to the point. The user is on a CLI.\n\
         - Use GitHub-flavored markdown for formatting.\n\
         - Output text to communicate; use tools to act. Never use tools or code comments \
         to talk to the user.\n\
         - NEVER create files unless they're absolutely necessary for achieving your goal. \
         ALWAYS prefer editing an existing file to creating a new one.\n\n\
         \
         # Doing tasks\n\
         - When asked to do work, plan it before acting. Inspect the codebase first.\n\
         - Tool results and user messages may include system reminder sections. These are \
         reminders from the system; they are not part of the user input.\n\
         - Prefer small, correct changes over large speculative rewrites.\n\n\
         \
         # Code references\n\
         - When referencing specific functions or pieces of code, include the pattern \
         `file_path:line_number` so the user can navigate directly to the source.\n\n\
         \
         # Tool invocation\n\
         - The list of available tools and their JSON-Schema argument \
         definitions is sent on the request, not in this prompt. \
         To call a tool, use the provider's native tool-calling API. \
         Do not write tool calls as XML, markdown, code blocks, or plain text. \
         Do not call a tool that isn't listed. Do not invent argument names.",
        name = agent_name,
    )
}

/// Flush the text buffer into `assistant_parts`, splitting out any
/// `<think>...</think>` blocks as `Reasoning` parts. This is a
/// fallback for models that emit thinking as raw text instead of via
/// the structured `ReasoningDelta` channel. We never extract tool
/// calls from this buffer — tool calls arrive as structured
/// `LlmEvent::ToolCall` events from the provider.
///
/// If a `<think>` is open at flush time, the buffered content is
/// yielded as `Reasoning` (so the UI can render the partial
/// thinking).
fn flush_text_buffer(
    buffer: &mut String,
    in_think_block: &mut bool,
    assistant_parts: &mut Vec<ContentPart>,
) {
    if buffer.is_empty() {
        return;
    }

    let mut rest = std::mem::take(buffer);
    *in_think_block = false;

    while let Some(open_idx) = rest.find("<think>") {
        let before = &rest[..open_idx];
        if !before.is_empty() {
            assistant_parts.push(ContentPart::text(before));
        }

        let after_open = &rest[open_idx + "<think>".len()..];
        if let Some(close_idx) = after_open.find("</think>") {
            let think = &after_open[..close_idx];
            if !think.is_empty() {
                assistant_parts.push(ContentPart::reasoning(think));
            }
            rest = after_open[close_idx + "</think>".len()..].to_string();
        } else {
            if !after_open.is_empty() {
                assistant_parts.push(ContentPart::reasoning(after_open));
            }
            *in_think_block = true;
            return;
        }
    }

    if !rest.is_empty() {
        assistant_parts.push(ContentPart::text(rest));
    }
}

fn default_tool_definitions() -> Vec<crate::llm::events::ToolDefinition> {
    vec![
        crate::llm::events::ToolDefinition::new(
            "read",
            "Read a file from disk",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to read" },
                    "offset": { "type": "integer", "description": "Line offset to start reading from", "minimum": 1 },
                    "limit": { "type": "integer", "description": "Maximum number of lines to read" }
                },
                "required": ["path"]
            }),
        ),
        crate::llm::events::ToolDefinition::new(
            "write",
            "Write content to a file",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to write" },
                    "content": { "type": "string", "description": "Content to write" }
                },
                "required": ["path", "content"]
            }),
        ),
        crate::llm::events::ToolDefinition::new(
            "edit",
            "Replace text in an existing file",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to edit" },
                    "old_string": { "type": "string", "description": "Text to find and replace" },
                    "new_string": { "type": "string", "description": "Replacement text" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        ),
        crate::llm::events::ToolDefinition::new(
            "glob",
            "Find files by glob pattern",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Root directory to search from" },
                    "pattern": { "type": "string", "description": "Glob pattern to match files against" }
                },
                "required": ["path"]
            }),
        ),
        crate::llm::events::ToolDefinition::new(
            "grep",
            "Search file contents",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Root directory to search from" },
                    "pattern": { "type": "string", "description": "Text pattern to search for" },
                    "include": { "type": "string", "description": "File name pattern to filter by" }
                },
                "required": ["path", "pattern"]
            }),
        ),
        crate::llm::events::ToolDefinition::new(
            "shell",
            "Run a shell command",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command to execute" },
                    "workdir": { "type": "string", "description": "Working directory for the command" }
                },
                "required": ["command"]
            }),
        ),
        crate::llm::events::ToolDefinition::new(
            "apply_patch",
            "Apply a file-oriented patch",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "patchText": { "type": "string", "description": "Full patch text describing file operations" }
                },
                "required": ["patchText"]
            }),
        ),
        crate::llm::events::ToolDefinition::new(
            "todo",
            "Update the session todo list",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "Todo content to set" }
                },
                "required": ["content"]
            }),
        ),
        crate::llm::events::ToolDefinition::new(
            "ask_peer",
            "Ask a question of a peer session in a different project. The peer runs \
             read-only (read/glob/grep/webfetch), so this is for consulting another \
             session's analysis without risking writes. Cross-repo only — the caller \
             and peer must live in different directories.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "peer_session_id": {
                        "type": "string",
                        "description": "ID of the peer session to consult"
                    },
                    "question": {
                        "type": "string",
                        "description": "The question to ask the peer"
                    },
                    "max_turns": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 5,
                        "description": "Maximum number of model turns the peer is allowed to take (1-5, default 3)"
                    }
                },
                "required": ["peer_session_id", "question"]
            }),
        ),
    ]
}

/// Read-only tool subset used by `ask_peer` child sessions. Excludes
/// `write`, `edit`, `apply_patch`, `shell`, `task`, and `ask_peer`
/// itself. The peer can read files, search, and fetch the web.
pub fn readonly_tool_definitions() -> Vec<crate::llm::events::ToolDefinition> {
    vec![
        crate::llm::events::ToolDefinition::new(
            "read",
            "Read a file from disk",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to a file to read" },
                    "offset": { "type": "integer", "minimum": 1 },
                    "limit": { "type": "integer", "minimum": 1 }
                },
                "required": ["path"]
            }),
        ),
        crate::llm::events::ToolDefinition::new(
            "glob",
            "Find files by glob pattern",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Root directory" },
                    "pattern": { "type": "string", "description": "Glob pattern" }
                },
                "required": ["path"]
            }),
        ),
        crate::llm::events::ToolDefinition::new(
            "grep",
            "Search file contents",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "pattern": { "type": "string" },
                    "include": { "type": "string" }
                },
                "required": ["path", "pattern"]
            }),
        ),
        crate::llm::events::ToolDefinition::new(
            "webfetch",
            "Fetch a web URL",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" }
                },
                "required": ["url"]
            }),
        ),
        crate::llm::events::ToolDefinition::new(
            "websearch",
            "Search the web",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" }
                },
                "required": ["query"]
            }),
        ),
    ]
}

#[derive(Debug)]
pub struct RunResult {
    pub steps: u32,
}

#[derive(Debug)]
pub enum SessionError {
    SessionNotFound(String),
    NoProviderForSession,
    Conversation(String),
    Llm(crate::llm::request::LlmError),
    Provider(String),
    StepLimitExceeded {
        session_id: String,
        limit: u32,
    },
    /// Estimated request size exceeds the model context window. The
    /// UI is expected to surface this and trigger a compaction
    /// (see `ConversationService::compact_conversation`) before
    /// retrying.
    ContextOverflow {
        session_id: String,
        estimated_tokens: usize,
        context_window: usize,
    },
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::SessionNotFound(id) => write!(f, "session not found: {id}"),
            SessionError::NoProviderForSession => {
                write!(f, "no provider configured for session")
            }
            SessionError::Conversation(msg) => write!(f, "conversation error: {msg}"),
            SessionError::Llm(err) => write!(f, "LLM error: {err}"),
            SessionError::Provider(msg) => write!(f, "provider error: {msg}"),
            SessionError::StepLimitExceeded { session_id, limit } => {
                write!(f, "session {session_id} exceeded step limit {limit}")
            }
            SessionError::ContextOverflow {
                session_id,
                estimated_tokens,
                context_window,
            } => write!(
                f,
                "session {session_id} request estimated at {estimated_tokens} tokens exceeds the {context_window}-token context window; compact the conversation before retrying"
            ),
        }
    }
}

impl std::error::Error for SessionError {}

pub struct ProviderRegistry {
    providers: RwLock<HashMap<String, Arc<dyn LlmProvider>>>,
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: RwLock::new(HashMap::new()),
        }
    }

    pub async fn register(&self, provider: Arc<dyn LlmProvider>) {
        let name = provider.provider_name().to_string();
        self.providers.write().await.insert(name, provider);
    }

    pub async fn get(&self, name: &str) -> Option<Arc<dyn LlmProvider>> {
        self.providers.read().await.get(name).cloned()
    }

    pub fn register_defaults_sync(&self) {
        let openai = Arc::new(crate::llm::providers::OpenAiProvider::new(None, None))
            as Arc<dyn LlmProvider>;
        let anthropic = Arc::new(crate::llm::providers::AnthropicProvider::new(None, None))
            as Arc<dyn LlmProvider>;
        let minimax = Arc::new(crate::llm::providers::MiniMaxTokenPlanProvider::new(
            None, None,
        )) as Arc<dyn LlmProvider>;

        // Note: using blocking insert since we're in a sync context
        // In async context use register() which does .write().await
        let mut guard = self.providers.blocking_write();
        guard.insert("openai".to_string(), openai);
        guard.insert("anthropic".to_string(), anthropic);
        guard.insert("minimax".to_string(), minimax);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_to_llm_messages_empty() {
        let history = vec![];
        let messages = history_to_llm_messages(&history);
        assert!(messages.is_empty());
    }

    #[test]
    fn history_to_llm_messages_user() {
        let history = vec![ConversationMessage {
            id: "1".to_string(),
            role: "user".to_string(),
            content: "hello".to_string(),
            timestamp: "2024-01-01T00:00:00Z".to_string(),
        }];
        let messages = history_to_llm_messages(&history);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
    }

    #[test]
    fn system_prompt_for_session_names() {
        assert!(system_prompt_for_session("anthropic", &[]).contains("Claude"));
        assert!(system_prompt_for_session("openai", &[]).contains("GPT"));
        assert!(system_prompt_for_session("minimax", &[]).contains("MiniMax"));
    }

    fn part_text(p: &ContentPart) -> Option<&str> {
        match p {
            ContentPart::Text { text } => Some(text.as_str()),
            ContentPart::Reasoning { text } => Some(text.as_str()),
            _ => None,
        }
    }

    fn part_is_reasoning(p: &ContentPart) -> bool {
        matches!(p, ContentPart::Reasoning { .. })
    }

    fn part_is_text(p: &ContentPart) -> bool {
        matches!(p, ContentPart::Text { .. })
    }

    fn flush_for_test(buffer: &str) -> Vec<ContentPart> {
        let mut parts: Vec<ContentPart> = Vec::new();
        let mut buf = buffer.to_string();
        let mut in_think = false;
        flush_text_buffer(&mut buf, &mut in_think, &mut parts);
        if !buf.is_empty() || in_think {
            // If the buffer still has content (open think block
            // at end), the production path yields it as Reasoning.
            // The test helper does the same.
            if in_think && !buf.is_empty() {
                parts.push(ContentPart::reasoning(&buf));
            } else if !buf.is_empty() {
                parts.push(ContentPart::text(&buf));
            }
        }
        parts
    }

    #[test]
    fn flush_text_buffer_passthrough_when_no_think_block() {
        let parts = flush_for_test("hello world");
        assert_eq!(parts.len(), 1);
        assert_eq!(part_text(&parts[0]), Some("hello world"));
        assert!(part_is_text(&parts[0]));
    }

    #[test]
    fn flush_text_buffer_extracts_complete_think_block() {
        let parts = flush_for_test("<think>plan</think>answer");
        assert_eq!(parts.len(), 2);
        assert!(part_is_reasoning(&parts[0]));
        assert_eq!(part_text(&parts[0]), Some("plan"));
        assert!(part_is_text(&parts[1]));
        assert_eq!(part_text(&parts[1]), Some("answer"));
    }

    #[test]
    fn flush_text_buffer_handles_text_before_and_after() {
        let parts = flush_for_test("hi<think>reason</think>bye");
        assert_eq!(parts.len(), 3);
        assert_eq!(part_text(&parts[0]), Some("hi"));
        assert_eq!(part_text(&parts[1]), Some("reason"));
        assert!(part_is_reasoning(&parts[1]));
        assert_eq!(part_text(&parts[2]), Some("bye"));
    }

    #[test]
    fn flush_text_buffer_handles_multiple_think_blocks() {
        let parts = flush_for_test("<think>a</think>X<think>b</think>Y");
        assert_eq!(parts.len(), 4);
        assert!(part_is_reasoning(&parts[0]));
        assert_eq!(part_text(&parts[0]), Some("a"));
        assert_eq!(part_text(&parts[1]), Some("X"));
        assert!(part_is_reasoning(&parts[2]));
        assert_eq!(part_text(&parts[2]), Some("b"));
        assert_eq!(part_text(&parts[3]), Some("Y"));
    }

    #[test]
    fn flush_text_buffer_unterminated_think_yields_reasoning() {
        // An unterminated `<think>` should still surface its
        // content as a partial Reasoning part so the UI can
        // render the live thinking text. `in_think` is set so a
        // subsequent call (e.g. when the close tag finally
        // arrives) knows we're mid-thought.
        let mut parts: Vec<ContentPart> = Vec::new();
        let mut buf = "<think>still thinking...".to_string();
        let mut in_think = false;
        flush_text_buffer(&mut buf, &mut in_think, &mut parts);
        assert_eq!(
            parts.len(),
            1,
            "unterminated think should yield a partial Reasoning, got: {parts:?}"
        );
        assert!(part_is_reasoning(&parts[0]));
        assert_eq!(part_text(&parts[0]), Some("still thinking..."));
        assert!(
            in_think,
            "in_think must be set so the runner knows we're mid-thought"
        );
    }

    #[test]
    fn flush_text_buffer_xml_tool_call_is_not_parsed_as_tool() {
        // Regression test: after removing the XML-tool parser, an
        // XML tool call embedded in text MUST NOT become a
        // `ContentPart::ToolCall`. It is preserved as visible
        // text. The runner no longer extracts tool calls from
        // text — they arrive as structured `LlmEvent::ToolCall`
        // events from the provider.
        let parts = flush_for_test("I will read it.\n<read>\n<path>src/lib.rs</path>\n</read>");
        assert!(
            !parts
                .iter()
                .any(|p| matches!(p, ContentPart::ToolCall { .. })),
            "xml tool call must NOT be parsed as ContentPart::ToolCall, got: {parts:?}"
        );
        assert!(parts
            .iter()
            .any(|p| part_is_text(p) && part_text(p).unwrap().contains("<read>")));
    }

    #[test]
    fn flush_text_buffer_tools_inside_think_become_reasoning() {
        // The text-fallback path doesn't try to distinguish
        // "tool call inside a think block" from "literal text
        // inside a think block" — both are captured as Reasoning
        // for the transcript. That's a fine fallback for models
        // that emit think blocks as raw text. The primary
        // structured path doesn't go through this buffer.
        let parts =
            flush_for_test("<think>\n<read>\n<path>src/lib.rs</path>\n</read>\n</think>answer");
        assert_eq!(parts.len(), 2);
        assert!(part_is_reasoning(&parts[0]));
        assert!(part_text(&parts[0]).unwrap().contains("<read>"));
        assert_eq!(part_text(&parts[1]), Some("answer"));
    }

    // ---------- has_unfulfilled_tool_cases ----------

    use crate::database::connection::Database;
    use crate::database::repositories::ProjectRepository;
    use crate::llm::providers::LlmStream;
    use crate::llm::request::LlmError;
    use crate::sessions::service::SessionService;
    use std::sync::Once;
    use tempfile::TempDir;

    static TRACING_INIT: Once = Once::new();
    fn init_tracing() {
        TRACING_INIT.call_once(|| {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .with_test_writer()
                .try_init();
        });
    }

    fn build_runner_with_db() -> (SessionRunner, TempDir, String) {
        let tmp = TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("test.sqlite");

        // Bootstrap: open the DB, init the schema, insert a project.
        // We do this in a tight scope so the `db` connection is
        // dropped before we open the same path via SessionService.
        let project_id = {
            let db = Database::new(db_path.clone()).expect("db open");
            db.init().expect("db init");
            let project = crate::domain::Project {
                id: "p1".to_string(),
                path: "/tmp".to_string(),
                name: "arachne".to_string(),
                tech_stack: Vec::new(),
                created_at: chrono::Utc::now(),
            };
            ProjectRepository::insert(&db, &project).expect("insert project");
            project.id
        };
        assert_eq!(project_id, "p1");

        let session_service = SessionService::new(db_path);
        let conv_service = ConversationService::new(tmp.path().join("conversations"));
        let providers: Arc<ProviderRegistry> = Arc::new(ProviderRegistry::new());
        let runner = SessionRunner::new(session_service, conv_service, providers);

        let session_id = "test-session-1".to_string();
        runner
            .session_service
            .create_session(
                "p1".to_string(),
                "/tmp".to_string(),
                "anthropic".to_string(),
                "claude-3-5-sonnet-20241022".to_string(),
            )
            .expect("create_session");
        runner
            .conversation_service
            .create_conversation(&session_id)
            .expect("create conversation");
        (runner, tmp, session_id)
    }

    // ---------- end-to-end stream with mock provider ----------

    /// Scripted LLM provider. Each `stream()` call pops the next
    /// pre-canned sequence of events from the queue. The first time
    /// it's called it produces a `<think>...</think>` block followed
    /// by a tool-call XML block (the same shape the user observed in
    /// the wild); the second time it returns a `Finish` with no text
    /// so the loop terminates cleanly.
    struct ScriptedProvider {
        provider_name: String,
        scripts: std::sync::Mutex<Vec<Vec<LlmEvent>>>,
    }

    impl ScriptedProvider {
        fn new(provider_name: &str, scripts: Vec<Vec<LlmEvent>>) -> Self {
            Self {
                provider_name: provider_name.to_string(),
                scripts: std::sync::Mutex::new(scripts),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for ScriptedProvider {
        fn provider_name(&self) -> &str {
            &self.provider_name
        }

        fn supported_models(&self) -> Vec<String> {
            vec!["scripted-model".to_string()]
        }

        async fn stream(&self, _request: LlmRequest) -> Result<LlmStream, LlmError> {
            let script = self.scripts.lock().unwrap().remove(0);
            let events: std::pin::Pin<Box<dyn tokio_stream::Stream<Item = LlmEvent> + Send>> = {
                let stream = async_stream::stream! {
                    for ev in script {
                        yield ev;
                    }
                };
                Box::pin(stream)
            };
            Ok(LlmStream {
                events,
                abort_tx: None,
            })
        }

        fn model_base_url(&self) -> Option<&str> {
            None
        }

        fn api_key(&self) -> Option<&str> {
            None
        }
    }

    /// The exact text the user observed in the debug log. The model
    /// emitted it token-by-token through `text_delta` events (no
    /// `text_start` / `text_end` in this stream), then a single
    /// `Finish` event.
    fn user_logged_buffer() -> String {
        "\n</think>\n\n<think>The user is asking me to try tool calls. \
         Let me try using some of the available tools to see if they work, \
         even though the LLM itself seems to have authentication issues.\n\n\
         Let me try a simple read or glob operation to see if the tool \
         infrastructure is working.\n</think>\n\n\n\nLet me try some tools:\n\n\
         <read>\n<path>/tmp/note.txt</path>\n</read>\n\n\
         <shell>\n<command>echo done</command>\n</shell>"
            .to_string()
    }

    /// Scripted LLM that emits a structured `LlmEvent::ToolCall` for
    /// `glob`, matching the opencode-style flow. The first script
    /// emits a reasoning block + text + the structured tool call;
    /// the second emits a `Finish` to close the loop.
    fn run_with_structured_tool_call(
        _session_directory: &str,
        tool_call: LlmEvent,
    ) -> Vec<Vec<LlmEvent>> {
        // First turn: emit a brief reasoning preamble (live), a
        // visible-text preamble, the structured tool call, and a
        // Finish.
        let turn1 = vec![
            LlmEvent::ReasoningDelta {
                id: "reasoning-0".to_string(),
                text: "Let me find the marker file.".to_string(),
            },
            LlmEvent::TextDelta {
                id: "text-0".to_string(),
                text: "I will use glob to look for the marker.\n".to_string(),
            },
            tool_call,
            LlmEvent::Finish {
                reason: FinishReason::ToolCalls,
                usage: None,
            },
        ];
        // Second turn (after the tool result is in): a brief reply
        // and a Stop.
        let turn2 = vec![
            LlmEvent::TextDelta {
                id: "text-0".to_string(),
                text: "Found the marker.".to_string(),
            },
            LlmEvent::Finish {
                reason: FinishReason::Stop,
                usage: None,
            },
        ];
        vec![turn1, turn2]
    }

    fn text_delta_chunks(full: &str, chunk_size: usize) -> Vec<LlmEvent> {
        let mut events: Vec<LlmEvent> = Vec::new();
        for slice in full.as_bytes().chunks(chunk_size) {
            let text = std::str::from_utf8(slice).unwrap().to_string();
            events.push(LlmEvent::TextDelta {
                id: "text".to_string(),
                text,
            });
        }
        events.push(LlmEvent::Finish {
            reason: FinishReason::Stop,
            usage: None,
        });
        events
    }

    async fn run_with_scripted(
        provider_name: &str,
        scripts: Vec<Vec<LlmEvent>>,
        session_directory: &str,
    ) -> (SessionRunner, TempDir, String) {
        let tmp = TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("test.sqlite");
        let project_id = {
            let db = Database::new(db_path.clone()).expect("db open");
            db.init().expect("db init");
            let project = crate::domain::Project {
                id: "p1".to_string(),
                path: session_directory.to_string(),
                name: "arachne".to_string(),
                tech_stack: Vec::new(),
                created_at: chrono::Utc::now(),
            };
            ProjectRepository::insert(&db, &project).expect("insert project");
            project.id
        };
        assert_eq!(project_id, "p1");

        let session_service = SessionService::new(db_path);
        let conv_service = ConversationService::new(tmp.path().join("conversations"));
        let providers: Arc<ProviderRegistry> = Arc::new(ProviderRegistry::new());
        let scripted = Arc::new(ScriptedProvider::new(provider_name, scripts));
        providers.register(scripted).await;
        let runner = SessionRunner::new(session_service, conv_service, providers);

        let session_id = runner
            .session_service
            .create_session(
                "p1".to_string(),
                session_directory.to_string(),
                provider_name.to_string(),
                "scripted-model".to_string(),
            )
            .expect("create_session");
        runner
            .conversation_service
            .create_conversation(&session_id)
            .expect("create conversation");
        runner
            .conversation_service
            .append_message(
                &session_id,
                crate::MessageRole::User,
                "please use some tools".to_string(),
            )
            .expect("append user");
        (runner, tmp, session_id)
    }

    #[tokio::test]
    async fn finish_only_turn_does_not_persist_empty_assistant_message() {
        init_tracing();
        let (runner, _tmp, session_id) = run_with_scripted(
            "scripted",
            vec![vec![LlmEvent::Finish {
                reason: FinishReason::Stop,
                usage: None,
            }]],
            "/tmp",
        )
        .await;

        let result = runner.run(&session_id).await;
        assert!(result.is_ok(), "run failed: {:?}", result.err());

        let msgs = runner
            .conversation_service
            .get_messages(&session_id)
            .expect("get_messages");
        assert!(
            msgs.iter().all(|m| m.role != "assistant"),
            "finish-only turn should not persist an empty assistant message: {msgs:#?}"
        );
    }

    #[tokio::test]
    async fn e2e_stream_accumulates_text_and_extracts_single_reasoning_part() {
        init_tracing();
        // The model streams a text buffer that contains a
        // `<think>…</think>` block followed by visible text (and,
        // historically, XML tool calls). After the opencode-style
        // refactor, tool calls must arrive as structured
        // `LlmEvent::ToolCall` events — the runner does NOT
        // extract them from text any more. The XML in this
        // historical buffer is preserved as visible text.
        //
        // The runner should:
        //   1. Produce ONE `Reasoning` part (not two) from the
        //      `<think>…</think>` block.
        //   2. Produce ZERO `ToolCall` parts (the XML is text).
        //   3. NOT have the think text anywhere in the visible
        //      Text part.
        let full = user_logged_buffer();
        let chunks = text_delta_chunks(&full, 6);
        let (runner, _tmp, session_id) = run_with_scripted(
            "scripted",
            vec![
                chunks,
                vec![LlmEvent::Finish {
                    reason: FinishReason::Stop,
                    usage: None,
                }],
            ],
            "/tmp",
        )
        .await;

        let result = runner.run(&session_id).await;
        assert!(result.is_ok(), "run failed: {:?}", result.err());

        let msgs = runner
            .conversation_service
            .get_messages(&session_id)
            .expect("get_messages");

        let assistant = msgs
            .iter()
            .rev()
            .filter(|m| m.role == "assistant")
            .find(|m| !m.content.is_empty() && m.content != "[]")
            .expect("an assistant message with non-empty content");
        tracing::info!(test = "e2e_stream_accumulates", persisted = %assistant.content, "persisted assistant content");

        let parts: Vec<ContentPart> = serde_json::from_str(&assistant.content)
            .expect("assistant content should be a parts JSON array");

        let reasoning: Vec<String> = parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Reasoning { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            reasoning.len(),
            1,
            "expected exactly ONE reasoning part, got {}: {:#?}",
            reasoning.len(),
            parts
        );
        let reasoning_text = &reasoning[0];
        assert!(
            reasoning_text.contains("The user is asking me to try tool calls"),
            "reasoning should capture the think block, got: {reasoning_text:?}"
        );

        // Tool calls must NOT be parsed from text any more.
        let tool_calls: Vec<(String, String)> = parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::ToolCall { id, name, .. } => Some((id.clone(), name.clone())),
                _ => None,
            })
            .collect();
        assert!(
            tool_calls.is_empty(),
            "xml tool calls in text must NOT be parsed as ToolCall, got: {tool_calls:?}"
        );

        let text_visible: String = parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !text_visible.contains("The user is asking me to try tool calls"),
            "the think text must NOT leak into the visible Text part, got: {text_visible:?}"
        );
        // The XML tool calls (which used to become ToolCall
        // parts) now remain as visible text. This is a deliberate
        // behavior change of the opencode-style refactor.
        assert!(
            text_visible.contains("<read>") && text_visible.contains("<shell>"),
            "xml tool calls should remain as visible text, got: {text_visible:?}"
        );
    }

    #[tokio::test]
    async fn e2e_stream_does_not_duplicate_reasoning_across_multiple_text_ends() {
        init_tracing();
        // Hypothetical provider that interleaves multiple
        // TextStart/TextEnd cycles around a single think block.
        // The runner must still produce exactly one Reasoning
        // part per `<think>…</think>` block.
        let full = user_logged_buffer();
        let mid = full.len() / 2;
        let (a, b) = full.split_at(mid);
        let events = vec![
            LlmEvent::TextStart {
                id: "t".to_string(),
            },
            LlmEvent::TextDelta {
                id: "t".to_string(),
                text: a.to_string(),
            },
            LlmEvent::TextEnd {
                id: "t".to_string(),
            },
            LlmEvent::TextStart {
                id: "t".to_string(),
            },
            LlmEvent::TextDelta {
                id: "t".to_string(),
                text: b.to_string(),
            },
            LlmEvent::TextEnd {
                id: "t".to_string(),
            },
            LlmEvent::Finish {
                reason: FinishReason::Stop,
                usage: None,
            },
        ];
        let (runner, _tmp, session_id) = run_with_scripted(
            "scripted",
            vec![
                events,
                vec![LlmEvent::Finish {
                    reason: FinishReason::Stop,
                    usage: None,
                }],
            ],
            "/tmp",
        )
        .await;

        runner.run(&session_id).await.expect("run ok");

        let msgs = runner
            .conversation_service
            .get_messages(&session_id)
            .expect("get_messages");
        let assistant = msgs
            .iter()
            .rev()
            .filter(|m| m.role == "assistant")
            .find(|m| !m.content.is_empty() && m.content != "[]")
            .expect("an assistant message with non-empty content");
        let parts: Vec<ContentPart> = serde_json::from_str(&assistant.content).expect("parts");
        let reasoning: Vec<String> = parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Reasoning { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            reasoning.len(),
            1,
            "two text-ends must not produce two reasoning parts, got: {:#?}",
            parts
        );
        let reasoning_text = &reasoning[0];
        assert!(
            reasoning_text.contains("The user is asking me to try tool calls"),
            "reasoning should capture the think block, got: {reasoning_text:?}"
        );
        // The XML tool calls are preserved as visible text.
        let text_visible: String = parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text_visible.contains("<read>") && text_visible.contains("<shell>"),
            "xml tool calls should remain as visible text, got: {text_visible:?}"
        );

        let text_visible: String = parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !text_visible.contains("The user is asking me to try tool calls"),
            "the think text must NOT leak into the visible Text part, got: {text_visible:?}"
        );
    }

    /// Regression test for the bug where `glob` ignored the
    /// session's project directory and fell back to the process
    /// CWD. Now exercised via the **structured** `LlmEvent::ToolCall`
    /// path (matching opencode): the provider emits a structured
    /// tool call with an explicit `path` (or empty path), the
    /// runner dispatches it with a `ToolContext` whose
    /// `project_root` comes from `session.directory`, and `glob`
    /// walks that directory.
    ///
    /// Flow under test (the *real* production path, end-to-end):
    ///   1. `SessionService::create_session` inserts an
    ///      `AgentSession` whose `directory` points at a unique
    ///      `TempDir` (so it is guaranteed not to be the process
    ///      CWD and not to contain `should-never-match-*` files).
    ///   2. The scripted LLM emits a structured
    ///      `LlmEvent::ToolCall` for `glob` with NO `path` field.
    ///      The provider lowerer surfaces the call's input as
    ///      `Value::Null` for the missing field; in production,
    ///      `glob` falls back to the `ToolContext.project_root`.
    ///   3. The runner builds a `ToolContext` from
    ///      `session.directory` and dispatches through
    ///      `run_tool_with_context`.
    ///   4. `glob::run_with_context` walks the session root and
    ///      finds the marker.
    #[tokio::test]
    async fn e2e_glob_uses_session_directory_via_structured_event() {
        init_tracing();

        // (1) Build a session rooted at a unique TempDir. Plant two
        //     files: one our pattern should match, one it must not
        //     (sanity check that glob isn't just "*"ing everything).
        let session_root = TempDir::new().expect("tempdir");
        let session_root_path = session_root.path().to_path_buf();
        let marker_name = "marker-arachne-glob-sessiondir.txt";
        let marker = session_root_path.join(marker_name);
        std::fs::write(&marker, "found it").expect("write marker");
        let decoy = session_root_path.join("should-never-match-decoy.bin");
        std::fs::write(&decoy, vec![0u8; 4]).expect("write decoy");

        let session_root_str = session_root_path.to_str().unwrap().to_string();
        tracing::info!(
            test = "e2e_glob_uses_session_directory_via_structured_event",
            session_root = %session_root_str,
            marker = %marker.display(),
            process_cwd = %std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "<unknown>".to_string()),
            "test setup: session dir is a unique TempDir, distinct from process CWD"
        );

        // (2) The scripted LLM emits a structured `glob` tool call
        //     with no `path` field. Production glob defaults the
        //     search root to the `ToolContext.project_root`, which
        //     is the session's directory.
        let tool_call = LlmEvent::ToolCall {
            id: "call_glob_1".to_string(),
            name: "glob".to_string(),
            input: serde_json::json!({ "pattern": "marker-arachne-glob-*.txt" }),
            provider_executed: Some(false),
        };
        let scripts = run_with_structured_tool_call(&session_root_str, tool_call);

        let (runner, _tmp, session_id) =
            run_with_scripted("scripted", scripts, &session_root_str).await;

        // (3)+(4) End-to-end: the runner loads the session, builds
        //         the ToolContext from session.directory, and
        //         dispatches the glob.
        let result = runner.run(&session_id).await;
        assert!(result.is_ok(), "run failed: {:?}", result.err());

        // Pull the assistant message that contains the tool call.
        // There may be multiple assistant messages in the
        // conversation (one per turn); find the one that has a
        // `ToolCall` part.
        let msgs = runner
            .conversation_service
            .get_messages(&session_id)
            .expect("get_messages");
        let assistant_with_call = msgs
            .iter()
            .rev()
            .filter(|m| m.role == "assistant")
            .find(|m| {
                !m.content.is_empty()
                    && m.content != "[]"
                    && serde_json::from_str::<Vec<ContentPart>>(&m.content)
                        .map(|parts| {
                            parts
                                .iter()
                                .any(|p| matches!(p, ContentPart::ToolCall { .. }))
                        })
                        .unwrap_or(false)
            })
            .expect("non-empty assistant message containing a ToolCall");
        let parts: Vec<ContentPart> =
            serde_json::from_str(&assistant_with_call.content).expect("parts");

        // The structured tool call should produce a `ToolCall`
        // part *and* a `ToolResult` part with the glob output.
        let tool_calls: Vec<&ContentPart> = parts
            .iter()
            .filter(|p| matches!(p, ContentPart::ToolCall { .. }))
            .collect();
        assert_eq!(
            tool_calls.len(),
            1,
            "expected exactly one ToolCall, got: {parts:#?}"
        );
        let results: Vec<&ContentPart> = parts
            .iter()
            .filter(|p| matches!(p, ContentPart::ToolResult { .. }))
            .collect();
        assert_eq!(
            results.len(),
            1,
            "expected exactly one ToolResult (the glob), got: {parts:#?}"
        );
        let result_text = match &results[0] {
            ContentPart::ToolResult { result, .. } => result.to_string(),
            _ => unreachable!(),
        };
        tracing::info!(
            test = "e2e_glob_uses_session_directory_via_structured_event",
            glob_result = %result_text,
            "glob tool_result"
        );

        // --- The actual assertions ---
        // 1. glob MUST NOT fail with "No files found" — that
        //    indicates it walked the wrong directory.
        assert!(
            !result_text.contains("No files found"),
            "glob searched the wrong directory; got: {result_text}"
        );
        // 2. The marker file path (under the session root) MUST
        //    appear in the result.
        assert!(
            result_text.contains(marker_name),
            "glob did NOT find the marker under the session dir \
             ({session_root_str}); the runner is not threading \
             session.directory into glob's project_root. got: \
             {result_text}"
        );
        // 3. The decoy file MUST NOT match the pattern.
        assert!(
            !result_text.contains("should-never-match-decoy.bin"),
            "glob returned an unrelated file; pattern filtering \
             is broken. got: {result_text}"
        );

        // 4. The Reasoning part from the structured `ReasoningDelta`
        //    should be persisted.
        assert!(
            parts.iter().any(
                |p| matches!(p, ContentPart::Reasoning { text } if text.contains("Let me find"))
            ),
            "structured ReasoningDelta should produce a Reasoning part, got: {parts:#?}"
        );
    }

    /// Regression test for the opencode-style refactor: an LLM that
    /// emits an XML-style tool call embedded in its text stream
    /// (the OLD format) is now treated as plain text. The runner
    /// does NOT extract tool calls from text — it relies on
    /// structured events from the provider.
    #[tokio::test]
    async fn e2e_xml_text_tool_call_is_not_extracted() {
        init_tracing();
        let session_root = TempDir::new().expect("tempdir");
        let session_root_path = session_root.path().to_path_buf();
        let session_root_str = session_root_path.to_str().unwrap().to_string();

        // The scripted "LLM" emits a text block that contains the
        // OLD XML tool format. The runner must treat the entire
        // thing as visible text and NOT produce a `ContentPart::ToolCall`.
        let model_text = "<read>\n<path>/etc/hostname</path>\n</read>".to_string();
        let chunks = text_delta_chunks(&model_text, 6);
        let (runner, _tmp, session_id) = run_with_scripted(
            "scripted",
            vec![
                chunks,
                vec![LlmEvent::Finish {
                    reason: FinishReason::Stop,
                    usage: None,
                }],
            ],
            &session_root_str,
        )
        .await;

        let result = runner.run(&session_id).await;
        assert!(result.is_ok(), "run failed: {:?}", result.err());

        let msgs = runner
            .conversation_service
            .get_messages(&session_id)
            .expect("get_messages");
        let assistant = msgs
            .iter()
            .rev()
            .filter(|m| m.role == "assistant")
            .find(|m| !m.content.is_empty() && m.content != "[]")
            .expect("non-empty assistant message");
        let parts: Vec<ContentPart> = serde_json::from_str(&assistant.content).expect("parts");

        assert!(
            !parts
                .iter()
                .any(|p| matches!(p, ContentPart::ToolCall { .. })),
            "xml-style tool call in text must NOT be parsed as a ToolCall, got: {parts:#?}"
        );
        assert!(
            !parts
                .iter()
                .any(|p| matches!(p, ContentPart::ToolResult { .. })),
            "no tool result should be produced when no tool was called, got: {parts:#?}"
        );
        // The XML is preserved as visible text.
        let combined_text: String = parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(
            combined_text.contains("<read>"),
            "xml text must remain visible to the user, got: {combined_text:?}"
        );
    }

    #[test]
    fn has_unfulfilled_tool_calls_returns_false_for_empty_history() {
        let (runner, _tmp, session_id) = build_runner_with_db();
        assert!(!runner.has_unfulfilled_tool_calls(&session_id));
    }

    #[test]
    fn has_unfulfilled_tool_calls_returns_false_when_all_calls_have_results() {
        let (runner, _tmp, session_id) = build_runner_with_db();
        let parts = serde_json::json!([
            { "type": "tool_call", "id": "t1", "name": "read", "input": {} },
            { "type": "tool_result", "id": "t1", "name": "read", "result": { "text": "ok" } }
        ])
        .to_string();
        runner
            .conversation_service
            .upsert_message_content(
                &session_id,
                "m1",
                crate::domain::MessageRole::Assistant,
                &parts,
            )
            .expect("upsert");
        assert!(!runner.has_unfulfilled_tool_calls(&session_id));
    }

    #[test]
    fn has_unfulfilled_tool_calls_returns_true_when_call_has_no_result() {
        let (runner, _tmp, session_id) = build_runner_with_db();
        let parts = serde_json::json!([
            { "type": "tool_call", "id": "t1", "name": "read", "input": {} }
        ])
        .to_string();
        runner
            .conversation_service
            .upsert_message_content(
                &session_id,
                "m1",
                crate::domain::MessageRole::Assistant,
                &parts,
            )
            .expect("upsert");
        assert!(runner.has_unfulfilled_tool_calls(&session_id));
    }

    #[test]
    fn has_unfulfilled_tool_calls_handles_only_text_no_tool_calls() {
        let (runner, _tmp, session_id) = build_runner_with_db();
        let parts = serde_json::json!([
            { "type": "text", "text": "no tools here" }
        ])
        .to_string();
        runner
            .conversation_service
            .upsert_message_content(
                &session_id,
                "m1",
                crate::domain::MessageRole::Assistant,
                &parts,
            )
            .expect("upsert");
        assert!(!runner.has_unfulfilled_tool_calls(&session_id));
    }

    #[test]
    fn has_unfulfilled_tool_calls_ignores_non_assistant_messages() {
        // The user message has a stray "tool_call" in it (which would
        // be unusual but we should be robust). The runner only looks
        // at the LAST assistant message.
        let (runner, _tmp, session_id) = build_runner_with_db();
        runner
            .conversation_service
            .append_message(
                &session_id,
                crate::domain::MessageRole::User,
                "hello".to_string(),
            )
            .expect("append user");
        let assistant_parts = serde_json::json!([
            { "type": "text", "text": "hi back" }
        ])
        .to_string();
        runner
            .conversation_service
            .upsert_message_content(
                &session_id,
                "m1",
                crate::domain::MessageRole::Assistant,
                &assistant_parts,
            )
            .expect("upsert");
        assert!(!runner.has_unfulfilled_tool_calls(&session_id));
    }

    // ---------- inline persistence ----------

    #[test]
    fn upsert_message_content_creates_then_updates_in_place() {
        let (runner, _tmp, session_id) = build_runner_with_db();
        let mid = serde_json::json!([
            { "type": "tool_call", "id": "t1", "name": "read", "input": {} }
        ])
        .to_string();
        runner
            .conversation_service
            .upsert_message_content(
                &session_id,
                "m1",
                crate::domain::MessageRole::Assistant,
                &mid,
            )
            .expect("first upsert");

        // Re-read the file: the assistant message should be there.
        let msgs = runner
            .conversation_service
            .get_messages(&session_id)
            .expect("read");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].id, "m1");
        assert!(msgs[0].content.contains("tool_call"));

        // Inline-update with a final-flush value.
        let final_form = serde_json::json!([
            { "type": "tool_call", "id": "t1", "name": "read", "input": {} },
            { "type": "tool_result", "id": "t1", "name": "read", "result": { "text": "hello" } }
        ])
        .to_string();
        runner
            .conversation_service
            .upsert_message_content(
                &session_id,
                "m1",
                crate::domain::MessageRole::Assistant,
                &final_form,
            )
            .expect("second upsert");
        let msgs = runner
            .conversation_service
            .get_messages(&session_id)
            .expect("read 2");
        assert_eq!(msgs.len(), 1, "upsert should not create a new message");
        assert_eq!(msgs[0].id, "m1");
        assert!(msgs[0].content.contains("tool_result"));
    }

    #[test]
    fn upsert_message_content_is_crash_resilient() {
        // The whole point of inline persistence: a process crash
        // mid-turn leaves a coherent partial message in the file.
        // We simulate the "first event arrives, then crash before
        // the final flush" scenario and verify the partial is
        // visible to the next turn's `get_messages`.
        let (runner, _tmp, session_id) = build_runner_with_db();

        // 1. The LLM streams a text part. We persist it inline.
        let partial = serde_json::json!([
            { "type": "text", "text": "I am about to read the file." }
        ])
        .to_string();
        runner
            .conversation_service
            .upsert_message_content(
                &session_id,
                "m1",
                crate::domain::MessageRole::Assistant,
                &partial,
            )
            .expect("partial upsert");

        // 2. CRASH. The runner never reached its final flush. The
        //    next turn's read path must still see the partial text.
        let msgs = runner
            .conversation_service
            .get_messages(&session_id)
            .expect("read after crash");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "assistant");
        assert!(msgs[0].content.contains("I am about to read"));

        // 3. The next turn begins, the LLM makes a tool call, the
        //    upsert REPLACES the partial with a new shape (this is
        //    correct: the next turn's assistant message is a fresh
        //    message with a different id).
        runner
            .conversation_service
            .upsert_message_content(
                &session_id,
                "m2",
                crate::domain::MessageRole::Assistant,
                &serde_json::json!([{ "type": "text", "text": "second turn" }]).to_string(),
            )
            .expect("m2 upsert");
        let msgs = runner
            .conversation_service
            .get_messages(&session_id)
            .expect("read 2");
        assert_eq!(msgs.len(), 2, "m1 + m2 should both be persisted");
        assert_eq!(msgs[0].id, "m1");
        assert_eq!(msgs[1].id, "m2");
    }

    // ---------- doom loop wiring ----------

    use crate::permission_v2::ruleset::PermissionRuleset;

    fn build_runner_with_default_permissions() -> (SessionRunner, TempDir, String) {
        let (mut runner, tmp, session_id) = build_runner_with_db();
        let ruleset = PermissionRuleset::default();
        let (service, _receiver) =
            crate::permission_v2::PermissionService::new(session_id.clone(), ruleset);
        runner = runner.with_permissions(service);
        (runner, tmp, session_id)
    }

    #[test]
    fn doom_detector_default_threshold_is_three() {
        // The runner ships with the opencode default of 3. Verify
        // it's wired up at construction time.
        let (runner, _tmp, _session_id) = build_runner_with_db();
        assert_eq!(runner.doom.history_len(), 0);
        // Two repeats don't trigger.
        assert!(!runner.doom.record("read", "foo.rs"));
        assert!(!runner.doom.record("read", "foo.rs"));
        // The third triggers.
        assert!(runner.doom.record("read", "foo.rs"));
    }

    #[test]
    fn runner_accepts_injected_doom_detector() {
        // A custom threshold (e.g. 2 for tests) can be supplied.
        let (mut runner, _tmp, _session_id) = build_runner_with_db();
        let detector = Arc::new(crate::sandbox::DoomLoopDetector::new(2));
        runner = runner.with_doom_detector(detector.clone());
        assert!(!detector.record("read", "x"));
        assert!(detector.record("read", "x"));
    }

    #[test]
    fn doom_loop_reset_clears_history_after_injected_run() {
        let (runner, _tmp, _session_id) = build_runner_with_db();
        // Three same calls trigger doom; resetting clears history.
        assert!(!runner.doom.record("read", "x"));
        assert!(!runner.doom.record("read", "x"));
        assert!(runner.doom.record("read", "x"));
        runner.doom.reset();
        assert_eq!(runner.doom.history_len(), 0);
        // After reset, a single call doesn't trigger.
        assert!(!runner.doom.record("read", "x"));
    }

    #[test]
    fn doom_loop_different_args_dont_trigger() {
        let (runner, _tmp, _session_id) = build_runner_with_db();
        assert!(!runner.doom.record("read", "a.rs"));
        assert!(!runner.doom.record("read", "b.rs"));
        assert!(!runner.doom.record("read", "c.rs"));
    }

    #[test]
    fn permissions_builder_wires_v2_service() {
        // The permissions field is private; verify it doesn't break
        // construction. The real end-to-end test is the async doom
        // loop test below.
        let (runner, _tmp, _session_id) = build_runner_with_default_permissions();
        // Drop succeeds -> wiring didn't panic.
        drop(runner);
    }
}
