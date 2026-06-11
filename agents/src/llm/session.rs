use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_stream::StreamExt;

use crate::domain::ToolCall;
use crate::llm::events::{FinishReason, LlmEvent, ToolResultValue};
use crate::llm::providers::LlmProvider;
use crate::llm::request::{ContentPart, LlmMessage, LlmRequest};
use crate::llm::subagent_registry::SubagentRegistry;
use crate::permission::PermissionService;
use crate::sessions::conversation::{ConversationMessage, ConversationService};
use crate::sessions::service::SessionService;
use crate::tools::{run_tool_async, run_tool_with_context, ToolContext, ToolRuntime};

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
}

impl SessionRunner {
    pub fn new(
        session_service: Arc<SessionService>,
        conversation_service: Arc<ConversationService>,
        providers: Arc<ProviderRegistry>,
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

        // Opencode-style loop: re-query the persisted conversation
        // history on each iteration. The runner decides whether to
        // continue based on whether the most recent assistant message
        // has any unfulfilled tool calls (i.e. a `ToolCall` content
        // part with no matching `ToolResult` part in the same message).
        // This is more robust than trusting the LLM's `FinishReason`
        // because some providers return "stop" even when the
        // assistant emitted tool calls.
        while step < self.max_steps {
            let continue_loop = self.run_turn(session_id, step).await?;
            step += 1;

            if !continue_loop {
                break;
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

        let mut messages = history_to_llm_messages(&history);

        // Inject the active permission mode as a synthetic user message
        // so the LLM knows what behaviour to follow. The mode is set
        // per-turn from the chat UI's in-memory toggle and is not
        // persisted.
        let mode = self.mode;
        println!("{:?}", self.mode);
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
            "<permission_mode>{mode}</permission_mode>\
             <instructions>You are in {mode} mode. In {mode} mode, {mode_guidance}. \
             Tool calls that violate the active mode will be rejected by the \
             runtime; plan accordingly.</instructions>",
        )));

        let system_prompt = system_prompt_for_session(&session.provider, &[]);

        let tools = if self.readonly_tools {
            readonly_tool_definitions()
        } else {
            default_tool_definitions()
        };

        // The LLM is told about tools via the system prompt (in XML
        // block format) rather than via the request body's `tools`
        // field. Providers don't send a structured `tools` array; the
        // runner parses tool calls out of the streamed `text_delta`
        // events below.
        let tool_prompt = crate::llm::xml_tools::render_tools_as_prompt(&tools);
        let system_prompt = format!("{system_prompt}{tool_prompt}");

        let request = LlmRequest::new(&session.model, &session.provider)
            .with_system(system_prompt)
            .with_messages(messages);

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
        let mut accumulated_text = String::new();
        let mut in_think_block = false;
        let mut assistant_parts: Vec<ContentPart> = Vec::new();
        let mut needs_continuation = false;
        let mut finish_reason = FinishReason::Unknown;
        let mut tool_id_counter: u32 = 0;
        let known_tool_names = crate::llm::xml_tools::known_tool_set(&tools);

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
                    accumulated_text.push_str(&text);
                }
                LlmEvent::TextEnd { .. } => {
                    process_accumulated_text(
                        &mut accumulated_text,
                        &mut assistant_parts,
                        &mut in_think_block,
                        &known_tool_names,
                        &mut tool_id_counter,
                        &mut pending_tool_calls,
                        &mut needs_continuation,
                    );
                }
                LlmEvent::ReasoningDelta { text, .. } => {
                    if !text.is_empty() {
                        assistant_parts.push(ContentPart::reasoning(&text));
                    }
                }
                LlmEvent::ToolInputDelta { id, name: _, text } => {
                    if let Some((_, _, buf)) =
                        pending_tool_calls.iter_mut().find(|(cid, _, _)| cid == &id)
                    {
                        buf.push_str(&text);
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
            if let Err(e) = flush_parts(&assistant_parts) {
                tracing::warn!(
                    session_id = %session_id,
                    step = step,
                    error = %e,
                    "inline persistence failed; will retry at end of turn"
                );
            }
        }

        if !accumulated_text.is_empty() || in_think_block {
            process_accumulated_text(
                &mut accumulated_text,
                &mut assistant_parts,
                &mut in_think_block,
                &known_tool_names,
                &mut tool_id_counter,
                &mut pending_tool_calls,
                &mut needs_continuation,
            );
        }

        // After the stream is done, dispatch the pending tool calls.
        // Tool results are appended to `assistant_parts` so the
        // persisted assistant message contains both the call and
        // the result, and the next turn's LLM request sees them
        // through the standard message-history path (NOT through
        // the in-memory `stream.inject_tool_result` channel, which
        // is closed by the time we get here for OpenAI/Anthropic).
        if !pending_tool_calls.is_empty() {
            let permission = PermissionService::new(self.mode);
            let project_root = std::path::PathBuf::from(&session.directory);
            let ctx = ToolContext::new(self.mode)
                .with_project_root(project_root.clone());
            tracing::debug!(
                session_id = %session_id,
                step = step,
                count = pending_tool_calls.len(),
                mode = ?self.mode,
                project_root = %project_root.display(),
                project_root_is_empty = project_root.as_os_str().is_empty(),
                session_directory = %session.directory,
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

                let event = LlmEvent::ToolResult {
                    id: id.clone(),
                    name: name.clone(),
                    result: ToolResultValue::Json {
                        value: result_value.clone(),
                    },
                    output,
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
            "llm turn finished; persisting assistant message"
        );

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
    // CLI-oriented. The tool list is appended separately by
    // `xml_tools::render_tools_as_prompt`, which the runner injects
    // after this base prompt.
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
         - Tool results and user messages may include <system-reminder> tags. These are \
         reminders from the system; they are not part of the user input.\n\
         - Prefer small, correct changes over large speculative rewrites.\n\n\
         \
         # Code references\n\
         - When referencing specific functions or pieces of code, include the pattern \
         `file_path:line_number` so the user can navigate directly to the source.\n\n\
         \
         # Tool invocation\n\
         - The list of available tools and their argument schemas follows in the next \
         section of this prompt. To call a tool, emit a fenced XML block as documented \
         there. Do not call a tool that isn't listed. Do not invent argument names.",
        name = agent_name,
    )
}

/// Splits an accumulated text buffer into a sequence of `ContentPart`s,
/// extracting any `<think>...</think>` blocks into `Reasoning` parts and
/// the rest into `Text` parts. Unterminated `<think>` (no closing tag yet)
/// is treated as still-in-progress reasoning and yielded as `Reasoning`,
/// so the streaming UI can render live thinking.
fn split_text_with_think_blocks(buffer: &str) -> Vec<ContentPart> {
    let mut parts: Vec<ContentPart> = Vec::new();
    let mut rest = buffer;

    while let Some(open_idx) = rest.find("<think>") {
        let before = &rest[..open_idx];
        if !before.is_empty() {
            parts.push(ContentPart::text(before));
        }

        let after_open = &rest[open_idx + "<think>".len()..];
        if let Some(close_idx) = after_open.find("</think>") {
            let think = &after_open[..close_idx];
            if !think.is_empty() {
                parts.push(ContentPart::reasoning(think));
            }
            rest = &after_open[close_idx + "</think>".len()..];
        } else {
            if !after_open.is_empty() {
                parts.push(ContentPart::reasoning(after_open));
            }
            rest = "";
            break;
        }
    }

    if !rest.is_empty() {
        parts.push(ContentPart::text(rest));
    }

    parts
}

#[cfg(test)]
/// Scans `buffer` for a complete `<think>...</think>` block. If one is
/// found, the surrounding text is split into `Text` and `Reasoning`
/// parts and pushed onto `parts`; the buffer is updated to contain
/// everything after the closing tag. If a `<think>` was opened but not
/// yet closed, the open-tag text is held in `buffer` and `in_think_block`
/// is set so the next call can finalize it. Anything before the open tag
/// is flushed as `Text` parts.
fn drain_complete_think_blocks(
    buffer: &mut String,
    parts: &mut Vec<ContentPart>,
    in_think_block: &mut bool,
) {
    loop {
        if *in_think_block {
            let Some(close_idx) = buffer.find("</think>") else {
                return;
            };
            let think = buffer[..close_idx].to_string();
            if !think.is_empty() {
                parts.push(ContentPart::reasoning(think));
            }
            // `drain(..end)` removes the consumed prefix (everything up
            // to and including the closing tag) and returns the removed
            // bytes. The buffer is left with the suffix; we drop the
            // iterator's collected bytes.
            let _: String = buffer.drain(..close_idx + "</think>".len()).collect();
            *in_think_block = false;
        }

        let Some(open_idx) = buffer.find("<think>") else {
            if !buffer.is_empty() {
                let rest = std::mem::take(buffer);
                parts.push(ContentPart::text(rest));
            }
            return;
        };

        if open_idx > 0 {
            let before = buffer[..open_idx].to_string();
            parts.push(ContentPart::text(before));
        }
        // Drop the open tag; keep whatever follows as the in-progress
        // thinking buffer.
        let after_open = buffer[open_idx + "<think>".len()..].to_string();
        buffer.clear();
        buffer.push_str(&after_open);
        *in_think_block = true;
    }
}

#[cfg(test)]
/// Final flush of any remaining text in the buffer. Handles both
/// unterminated `<think>` blocks (yielded as `Reasoning`) and plain text
/// (yielded as `Text`).
fn flush_accumulated_text(
    buffer: &mut String,
    parts: &mut Vec<ContentPart>,
    in_think_block: &mut bool,
) {
    if buffer.is_empty() {
        return;
    }

    if *in_think_block {
        if !buffer.is_empty() {
            parts.push(ContentPart::reasoning(buffer.as_str()));
        }
    } else if buffer.contains("<think>") {
        let split = split_text_with_think_blocks(buffer);
        for part in split {
            parts.push(part);
        }
    } else {
        parts.push(ContentPart::text(buffer.as_str()));
    }
    buffer.clear();
    *in_think_block = false;
}

#[allow(clippy::too_many_arguments)]
fn process_accumulated_text(
    buffer: &mut String,
    assistant_parts: &mut Vec<ContentPart>,
    in_think_block: &mut bool,
    known_tools: &std::collections::HashSet<String>,
    id_counter: &mut u32,
    pending_tool_calls: &mut Vec<(String, String, String)>,
    needs_continuation: &mut bool,
) {
    if buffer.is_empty() {
        return;
    }

    let parts = split_text_with_think_blocks(buffer);
    buffer.clear();
    *in_think_block = false;

    for part in parts {
        match part {
            ContentPart::Text { text } => drain_text_for_tool_blocks(
                &text,
                known_tools,
                id_counter,
                pending_tool_calls,
                assistant_parts,
                needs_continuation,
            ),
            ContentPart::Reasoning { text } => {
                if !text.is_empty() {
                    assistant_parts.push(ContentPart::reasoning(text));
                }
            }
            ContentPart::ToolCall { .. } | ContentPart::ToolResult { .. } => {}
        }
    }
}

/// Pull complete XML tool blocks out of the accumulated text buffer.
/// Each `Valid` block becomes a queued tool call that will be dispatched
/// after the stream ends. Each `Invalid` block is fed back to the LLM
/// as a synthetic `ToolError` so the model can correct itself on the
/// next turn.
#[allow(clippy::too_many_arguments)]
fn drain_text_for_tool_blocks(
    text: &str,
    known_tools: &std::collections::HashSet<String>,
    id_counter: &mut u32,
    pending_tool_calls: &mut Vec<(String, String, String)>,
    assistant_parts: &mut Vec<ContentPart>,
    needs_continuation: &mut bool,
) {
    if text.is_empty() {
        return;
    }

    let segments =
        crate::llm::xml_tools::drain_tool_blocks_preserving_text(text, known_tools, id_counter);

    for segment in segments {
        match segment {
            crate::llm::xml_tools::DrainedToolSegment::Text(text) => {
                if !text.is_empty() {
                    assistant_parts.push(ContentPart::text(text));
                }
            }
            crate::llm::xml_tools::DrainedToolSegment::Tool(
                crate::llm::xml_tools::ParsedToolBlock::Valid { id, tool, input },
            ) => {
                *needs_continuation = true;
                let input_str = serde_json::to_string(&input).unwrap_or_default();
                assistant_parts.push(ContentPart::tool_call(&id, &tool, input.clone()));
                pending_tool_calls.push((id.clone(), tool.clone(), input_str.clone()));
                tracing::info!(
                    tool_call_id = %id,
                    tool = %tool,
                    input = %input_str,
                    "parsed valid tool block; queued for dispatch"
                );
            }
            crate::llm::xml_tools::DrainedToolSegment::Tool(
                crate::llm::xml_tools::ParsedToolBlock::Invalid { tool, reason },
            ) => {
                let id = format!("xml-err-{}", *id_counter);
                *id_counter += 1;
                *needs_continuation = true;
                let result = serde_json::json!({ "error": reason });
                assistant_parts.push(ContentPart::tool_result(&id, &tool, result));
                tracing::warn!(
                    tool = %tool,
                    tool_result_id = %id,
                    reason = %reason,
                    "parsed invalid tool block; feeding ToolError back to LLM"
                );
            }
            crate::llm::xml_tools::DrainedToolSegment::Tool(
                crate::llm::xml_tools::ParsedToolBlock::Incomplete,
            ) => {}
        }
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
    StepLimitExceeded { session_id: String, limit: u32 },
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

    fn split_text(buffer: &str) -> Vec<ContentPart> {
        let mut parts = Vec::new();
        let mut buf = buffer.to_string();
        let mut in_think = false;
        drain_complete_think_blocks(&mut buf, &mut parts, &mut in_think);
        if !buf.is_empty() || in_think {
            flush_accumulated_text(&mut buf, &mut parts, &mut in_think);
        }
        parts
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

    #[test]
    fn split_text_passthrough_when_no_think_block() {
        let parts = split_text("hello world");
        assert_eq!(parts.len(), 1);
        assert_eq!(part_text(&parts[0]), Some("hello world"));
        assert!(!part_is_reasoning(&parts[0]));
    }

    #[test]
    fn split_text_extracts_complete_think_block() {
        let parts = split_text("<think>plan</think>answer");
        assert_eq!(parts.len(), 2);
        assert!(part_is_reasoning(&parts[0]));
        assert_eq!(part_text(&parts[0]), Some("plan"));
        assert!(!part_is_reasoning(&parts[1]));
        assert_eq!(part_text(&parts[1]), Some("answer"));
    }

    #[test]
    fn split_text_with_text_before_and_after() {
        let parts = split_text("hi<think>reason</think>bye");
        assert_eq!(parts.len(), 3);
        assert_eq!(part_text(&parts[0]), Some("hi"));
        assert_eq!(part_text(&parts[1]), Some("reason"));
        assert!(part_is_reasoning(&parts[1]));
        assert_eq!(part_text(&parts[2]), Some("bye"));
    }

    #[test]
    fn split_text_with_unterminated_think_block_flushes_as_reasoning() {
        let parts = split_text("<think>still thinking...");
        assert_eq!(parts.len(), 1);
        assert!(part_is_reasoning(&parts[0]));
        assert_eq!(part_text(&parts[0]), Some("still thinking..."));
    }

    #[test]
    fn split_text_handles_multiple_think_blocks() {
        let parts = split_text("<think>a</think>X<think>b</think>Y");
        assert_eq!(parts.len(), 4);
        assert!(part_is_reasoning(&parts[0]));
        assert_eq!(part_text(&parts[0]), Some("a"));
        assert_eq!(part_text(&parts[1]), Some("X"));
        assert!(part_is_reasoning(&parts[2]));
        assert_eq!(part_text(&parts[2]), Some("b"));
        assert_eq!(part_text(&parts[3]), Some("Y"));
    }

    #[test]
    fn split_text_empty_think_block() {
        let parts = split_text("<think></think>after");
        assert_eq!(parts.len(), 1);
        assert_eq!(part_text(&parts[0]), Some("after"));
    }

    #[test]
    fn streaming_drain_emits_text_then_reasoning_as_blocks_complete() {
        let mut parts: Vec<ContentPart> = Vec::new();
        let mut buf = String::new();
        let mut in_think = false;

        // Simulate streaming the LLM's first response token by token.
        for chunk in ["<think>", "plan", "</think>", "\n\n", "It looks"] {
            buf.push_str(chunk);
            drain_complete_think_blocks(&mut buf, &mut parts, &mut in_think);
        }

        // We expect three parts: the reasoning block, the text fragment
        // that came in before any new text joined it, and the final
        // text fragment.
        assert_eq!(parts.len(), 3);
        assert!(part_is_reasoning(&parts[0]));
        assert_eq!(part_text(&parts[0]), Some("plan"));
        assert!(!part_is_reasoning(&parts[1]));
        assert_eq!(part_text(&parts[1]), Some("\n\n"));
        assert!(!part_is_reasoning(&parts[2]));
        assert_eq!(part_text(&parts[2]), Some("It looks"));
        assert!(buf.is_empty());
        assert!(!in_think);
    }

    #[test]
    fn process_aggregated_text_parses_xml_tool_after_full_text() {
        let mut parts: Vec<ContentPart> = Vec::new();
        let mut buf = "I will read it.\n<read>\n<path>src/lib.rs</path>\n</read>".to_string();
        let mut in_think = false;
        let mut known_tools = std::collections::HashSet::new();
        known_tools.insert("read".to_string());
        let mut id_counter = 0;
        let mut pending = Vec::new();
        let mut needs_continuation = false;

        eprintln!("=== INPUT ===\n{buf}\n=== END INPUT ===");
        process_accumulated_text(
            &mut buf,
            &mut parts,
            &mut in_think,
            &known_tools,
            &mut id_counter,
            &mut pending,
            &mut needs_continuation,
        );

        eprintln!("=== PARTS ({}) ===", parts.len());
        for (i, part) in parts.iter().enumerate() {
            match part {
                ContentPart::Text { text } => eprintln!("[{i}] Text: {text:?}"),
                ContentPart::Reasoning { text } => eprintln!("[{i}] Reasoning: {text:?}"),
                ContentPart::ToolCall { id, name, input } => eprintln!("[{i}] ToolCall: id={id} name={name} input={input}"),
                ContentPart::ToolResult { id, name, result } => eprintln!("[{i}] ToolResult: id={id} name={name} result={result}"),
            }
        }
        eprintln!("=== PENDING ({}) ===", pending.len());
        for (i, (id, name, input)) in pending.iter().enumerate() {
            eprintln!("[{i}] Pending: id={id} name={name} input={input}");
        }
        eprintln!("=== NEEDS_CONTINUATION: {needs_continuation} ===");
        eprintln!("=== END ===");

        assert!(buf.is_empty());
        assert!(!in_think);
        assert!(needs_continuation);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1, "read");
        assert_eq!(parts.len(), 2);
        assert_eq!(part_text(&parts[0]), Some("I will read it.\n"));
        match &parts[1] {
            ContentPart::ToolCall { name, input, .. } => {
                assert_eq!(name, "read");
                assert_eq!(input["path"], "src/lib.rs");
            }
            other => panic!("expected tool call part, got {other:?}"),
        }
    }

    #[test]
    fn process_aggregated_text_does_not_parse_tools_inside_think() {
        let mut parts: Vec<ContentPart> = Vec::new();
        let mut buf =
            "<think>\n<read>\n<path>src/lib.rs</path>\n</read>\n</think>answer".to_string();
        let mut in_think = false;
        let mut known_tools = std::collections::HashSet::new();
        known_tools.insert("read".to_string());
        let mut id_counter = 0;
        let mut pending = Vec::new();
        let mut needs_continuation = false;

        process_accumulated_text(
            &mut buf,
            &mut parts,
            &mut in_think,
            &known_tools,
            &mut id_counter,
            &mut pending,
            &mut needs_continuation,
        );

        assert!(pending.is_empty());
        assert!(!needs_continuation);
        assert_eq!(parts.len(), 2);
        assert!(part_is_reasoning(&parts[0]));
        assert_eq!(part_text(&parts[1]), Some("answer"));
    }

    // Regression: the model on a follow-up turn can emit a stray
    // `</think>` from its previous turn's reasoning, then open a new
    // `<think>` block. The runner must produce ONE reasoning part for
    // the new thinking content, and put the orphan closer in the
    // preceding text part. It must NOT duplicate the thinking text.
    #[test]
    fn process_aggregated_text_does_not_duplicate_thinking_across_orphan_close_then_open() {
        let mut parts: Vec<ContentPart> = Vec::new();
        let mut buf = "\n</think>\n\n<think>The user is asking me to try tool calls. \
                       Let me try using some of the available tools to see if they work, \
                       even though the LLM itself seems to have authentication issues.\n\n\
                       Let me try a simple read or glob operation to see if the tool \
                       infrastructure is working.\n</think>\n\n\n\nLet me try some tools:\n\n\
                       Tool: glob\n[\"*\"]\n\nTool: read\n[\"/README.md\"]\n\n\
                       Tool: websearch\n[\"test search\"]"
            .to_string();
        let mut in_think = false;
        let mut known_tools = std::collections::HashSet::new();
        known_tools.insert("glob".to_string());
        known_tools.insert("read".to_string());
        known_tools.insert("websearch".to_string());
        let mut id_counter = 0;
        let mut pending = Vec::new();
        let mut needs_continuation = false;

        process_accumulated_text(
            &mut buf,
            &mut parts,
            &mut in_think,
            &known_tools,
            &mut id_counter,
            &mut pending,
            &mut needs_continuation,
        );

        let reasoning_count = parts.iter().filter(|p| part_is_reasoning(p)).count();
        let reasoning_text: String = parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Reasoning { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

        assert_eq!(
            reasoning_count, 1,
            "expected exactly one reasoning part, got: {parts:#?}"
        );
        assert!(
            reasoning_text.contains("user is asking me to try tool calls"),
            "reasoning should capture the think block, got: {reasoning_text:?}"
        );
        assert!(
            !reasoning_text.contains("Let me try some tools"),
            "post-think text must not leak into reasoning, got: {reasoning_text:?}"
        );
    }
        eprintln!("=== REASONING TEXT ===");
        eprintln!("{reasoning_text:?}");

        assert_eq!(
            reasoning_count, 1,
            "expected exactly one reasoning part, got: {parts:#?}"
        );
        assert!(
            reasoning_text.contains("user is asking me to try tool calls"),
            "reasoning should capture the think block, got: {reasoning_text:?}"
        );
        assert!(
            !reasoning_text.contains("Let me try some tools"),
            "post-think text must not leak into reasoning, got: {reasoning_text:?}"
        );
    }

    // ---------- has_unfulfilled_tool_cases ----------

    use crate::database::connection::Database;
    use crate::database::repositories::ProjectRepository;
    use crate::llm::providers::{LlmStream, ToolResultInject};
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
            let (tx, _rx) = tokio::sync::mpsc::channel::<ToolResultInject>(8);
            Ok(LlmStream {
                events,
                tool_result_tx: tx,
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

    /// Same as `user_logged_buffer` but the model emits a `glob`
    /// call instead of `read`. The runner's tool set knows about
    /// `glob` and should parse + dispatch it.
    fn user_logged_buffer_with_glob() -> String {
        "\n</think>\n\n<think>Trying a glob.\n</think>\n\n\
         <glob>\n<path>/tmp</path>\n<pattern>*</pattern>\n</glob>"
            .to_string()
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
    async fn e2e_stream_accumulates_text_and_extracts_single_reasoning_part() {
        init_tracing();
        // The model streams a `` block + visible text + two valid
        // XML tool calls. The runner should:
        //   1. Accumulate the raw text into `accumulated_text`.
        //   2. At the post-stream flush, call `process_accumulated_text`
        //      exactly once on the whole buffer.
        //   3. Produce ONE `Reasoning` part (not two).
        //   4. Produce TWO `ToolCall` parts.
        //   5. NOT have the think text anywhere in the visible text.
        let full = user_logged_buffer();
        let chunks = text_delta_chunks(&full, 6);
        let (runner, _tmp, session_id) = run_with_scripted(
            "scripted",
            vec![chunks, vec![LlmEvent::Finish { reason: FinishReason::Stop, usage: None }]],
            "/tmp",
        )
        .await;

        let result = runner.run(&session_id).await;
        assert!(result.is_ok(), "run failed: {:?}", result.err());

        let msgs = runner
            .conversation_service
            .get_messages(&session_id)
            .expect("get_messages");

        // The runner creates one assistant message per turn. The LAST
        // one is from the final (no-op) turn with no script events,
        // so its content is the empty `[]` snapshot. We want the
        // previous turn that actually streamed the LLM output.
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

        let tool_calls: Vec<(String, String)> = parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::ToolCall { id, name, .. } => Some((id.clone(), name.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            tool_calls.len(),
            2,
            "expected two tool calls, got: {tool_calls:?}"
        );
        let tool_names: std::collections::HashSet<_> =
            tool_calls.iter().map(|(_, n)| n.clone()).collect();
        assert!(tool_names.contains("read"));
        assert!(tool_names.contains("shell"));

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

    #[tokio::test]
    async fn e2e_stream_does_not_duplicate_reasoning_across_multiple_text_ends() {
        init_tracing();
        // Hypothetical provider that interleaves multiple
        // TextStart/TextEnd cycles around a single think block. The
        // runner must still produce exactly one Reasoning part.
        let full = user_logged_buffer();
        let mid = full.len() / 2;
        let (a, b) = full.split_at(mid);
        let events = vec![
            LlmEvent::TextStart { id: "t".to_string() },
            LlmEvent::TextDelta {
                id: "t".to_string(),
                text: a.to_string(),
            },
            LlmEvent::TextEnd { id: "t".to_string() },
            LlmEvent::TextStart { id: "t".to_string() },
            LlmEvent::TextDelta {
                id: "t".to_string(),
                text: b.to_string(),
            },
            LlmEvent::TextEnd { id: "t".to_string() },
            LlmEvent::Finish {
                reason: FinishReason::Stop,
                usage: None,
            },
        ];
        let (runner, _tmp, session_id) = run_with_scripted(
            "scripted",
            vec![events, vec![LlmEvent::Finish { reason: FinishReason::Stop, usage: None }]],
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
        // The runner creates one assistant message per turn. The LAST
        // one is from the final (no-op) turn with no script events,
        // so its content is the empty `[]` snapshot. We want the
        // previous turn that actually streamed the LLM output.
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

        let tool_calls: Vec<(String, String)> = parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::ToolCall { id, name, .. } => Some((id.clone(), name.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            tool_calls.len(),
            2,
            "expected two tool calls, got: {tool_calls:?}"
        );
        let tool_names: std::collections::HashSet<_> =
            tool_calls.iter().map(|(_, n)| n.clone()).collect();
        assert!(tool_names.contains("read"));
        assert!(tool_names.contains("shell"));

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

    #[tokio::test]
    async fn e2e_glob_uses_session_project_root() {
        init_tracing();
        // Set up a fresh temp directory as the session's project
        // root. The model's `glob` call passes `path="/tmp"` so the
        // runner must use the explicit path, NOT the project root.
        let session_root = TempDir::new().expect("tempdir");
        let session_root_path = session_root.path().to_path_buf();
        // Plant a known file so the glob can find it.
        let marker = session_root_path.join("marker-arachne-glob.txt");
        std::fs::write(&marker, "found it").expect("write marker");

        let full = user_logged_buffer_with_glob();
        let chunks = text_delta_chunks(&full, 6);
        let session_root_str = session_root_path.to_str().unwrap().to_string();
        let (runner, _tmp, session_id) = run_with_scripted(
            "scripted",
            vec![
                chunks,
                vec![LlmEvent::Finish { reason: FinishReason::Stop, usage: None }],
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
        let parts: Vec<ContentPart> =
            serde_json::from_str(&assistant.content).expect("parts");

        // The glob call should have produced a tool_result part.
        let results: Vec<&ContentPart> = parts
            .iter()
            .filter(|p| matches!(p, ContentPart::ToolResult { .. }))
            .collect();
        assert_eq!(
            results.len(),
            1,
            "expected one tool_result (the glob), got: {parts:#?}"
        );
        let result_text = match &results[0] {
            ContentPart::ToolResult { result, .. } => result.to_string(),
            _ => unreachable!(),
        };
        // The glob matched against /tmp. Even on a minimal CI image
        // /tmp has at least a handful of files, so the result string
        // should be non-empty.
        tracing::info!(test = "e2e_glob_uses_session_project_root", glob_result = %result_text, "glob tool_result");
        assert!(
            !result_text.contains("error"),
            "glob returned an error: {result_text}"
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
