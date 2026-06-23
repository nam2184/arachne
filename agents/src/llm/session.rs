use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::runtime::Handle;
use tokio::sync::RwLock;
use tokio_stream::StreamExt;

use crate::domain::ToolCall;
use crate::llm::events::{FinishReason, LlmEvent, ToolResultValue};
use crate::llm::providers::{LlmProvider, ToolDispatcherFn};
use crate::llm::request::{ContentPart, LlmMessage, LlmRequest};
use crate::llm::subagent_registry::SubagentRegistry;
use crate::llm::xml_tool_call::{InvalidToolCallHit, XmlToolCallScanner};
use crate::model_spec::ModelRegistry;
use crate::permission::PermissionService;
use crate::sessions::conversation::{ConversationMessage, ConversationService};
use crate::sessions::service::SessionService;
use crate::tools::{
    bound_tool_output, run_tool_async, run_tool_async_sandboxed, run_tool_sandboxed,
    run_tool_with_context, SandboxedContext, ToolContext, ToolRuntime, MAX_TOOL_OUTPUT_BYTES,
    MAX_TOOL_OUTPUT_LINES,
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
    /// When set, the runner writes the *full* (untruncated)
    /// output of any tool that gets bounded to the file system at
    /// `<dir>/<tool>-<seq>.txt` and includes the path in the
    /// truncation marker. Mirrors opencode's `Truncate.write`
    /// behavior. `None` (default) disables spillover — bounded
    /// outputs are just clipped and the marker says "suppressed".
    spill_dir: Option<std::path::PathBuf>,
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
    /// Sandboxed tool dispatch context. When set, every tool
    /// call goes through `run_tool_sandboxed`, which enforces
    /// path containment (the `path` argument must resolve
    /// inside the session's `project_root` or an allowlisted
    /// `external_roots`), env-scrubbed shell, SSRF-guarded
    /// network, and the v2 permission service. When unset,
    /// the runner falls back to the v1 `run_tool_with_context`
    /// path (no containment, no v2 service). Always set in
    /// production via `AgentService::build_runner`; tests that
    /// want the v1 behavior build a runner without this field.
    sandboxed_ctx: Option<Arc<crate::tools::SandboxedContext>>,
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
            spill_dir: None,
            mode: crate::permission::PermissionMode::default(),
            event_sink: None,
            doom: Arc::new(crate::sandbox::DoomLoopDetector::default()),
            permissions: None,
            model_registry,
            compactor: None,
            sandboxed_ctx: None,
        }
    }

    pub fn with_event_sink(mut self, event_sink: SessionEventSink) -> Self {
        self.event_sink = Some(event_sink);
        self
    }

    /// Cap the number of LLM turns a single `run` invocation may take.
    /// Used by the `task` tool to bound child sessions.
    pub fn with_max_turns(mut self, max_turns: u32) -> Self {
        self.max_steps = max_turns;
        self
    }

    /// Restrict this runner's tools to the read-only subset. The runner drops
    /// `write`, `edit`, `apply_patch`, `shell`, and `task` from the tool
    /// definitions it advertises to the LLM.
    pub fn with_readonly_tools(mut self, readonly: bool) -> Self {
        self.readonly_tools = readonly;
        self
    }

    /// Set the permission mode for this turn. The mode is injected into
    /// the LLM context as a synthetic user message and used to gate
    /// tool-call dispatch. Setting `Plan` mode also implies
    /// `readonly_tools = true` so the model never sees the write /
    /// edit / apply_patch / shell tool schemas on the wire — the
    /// tool list matches the system prompt's "you may not call
    /// these" reminder, instead of advertising them and then
    /// rejecting the call. `Build` mode (the default) advertises
    /// the full tool list.
    pub fn with_mode(mut self, mode: crate::permission::PermissionMode) -> Self {
        self.mode = mode;
        if matches!(mode, crate::permission::PermissionMode::Plan) {
            self.readonly_tools = true;
        }
        self
    }

    /// Wire the SubagentRegistry into the runner so the `task` tool and
    /// peer-targeted plan-mode tool calls have access.
    pub fn with_subagent_registry(mut self, registry: Arc<SubagentRegistry>) -> Self {
        self.subagent_registry = Some(registry);
        self
    }

    /// Return a no-op subagent registry. Used by the runner when
    /// dispatching async tools that don't need sub-sessions
    /// (`webfetch`); the registry is never consulted in that
    /// path. Avoids forcing every top-level session to wire up a
    /// real DB-backed registry just so a fetch can be async.
    pub fn subagent_registry_noop(&self) -> Arc<SubagentRegistry> {
        SubagentRegistry::new_noop()
    }

    /// Wire the directory where bounded-but-truncated tool
    /// outputs get spilled to disk (opencode `TruncationDir`
    /// pattern). Pass `None` (or skip this call) to disable
    /// spillover — bounded outputs are just clipped and the
    /// marker says "suppressed". The Tauri runtime wires the
    /// app-data-dir-relative `tool-output/` path; tests use
    /// `tempdir()`.
    pub fn with_spill_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.spill_dir = Some(dir.into());
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

    /// Route tool dispatch through the v2 sandboxed execution path.
    pub fn with_sandboxed_context(mut self, ctx: Arc<SandboxedContext>) -> Self {
        self.sandboxed_ctx = Some(ctx);
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

    /// Decide whether a `SessionError` from `run_turn` should trigger
    /// an auto-compaction attempt. The runner decides purely from the
    /// typed `SessionError::ContextOverflow` that the pre-dispatch
    /// check returns when the assembled request would exceed the
    /// model's `context_window` minus the configured output budget.
    ///
    /// Provider-side 4xx/5xx responses (auth, rate limit, bad
    /// gateway, etc.) are not treated as overflow triggers. The
    /// runner only auto-compacts when it can prove the request is
    /// over the model's hard limit by looking at the body it is
    /// about to send.
    fn should_attempt_compaction(&self, error: &SessionError) -> bool {
        if self.compactor.is_none() {
            return false;
        }
        matches!(error, SessionError::ContextOverflow { .. })
    }

    fn compaction_trigger_label(&self, error: &SessionError) -> &'static str {
        match error {
            SessionError::ContextOverflow { .. } => "pre_dispatch_estimate",
            _ => "unknown",
        }
    }

    pub async fn run(&self, session_id: &str) -> Result<RunResult, SessionError> {
        let mut step = 0u32;
        // Auto-compaction can be tried up to this many times per
        // `run()` call. The first attempt covers the proactive
        // estimate; a second attempt covers a provider-side
        // 4xx/5xx that looks like a context overflow. Two is
        // enough: if the conversation still does not fit, the
        // error is surfaced so the user can compact manually.
        const MAX_COMPACTION_ATTEMPTS: u32 = 2;
        let mut compaction_attempts: u32 = 0;

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
                Err(error)
                    if self.should_attempt_compaction(&error)
                        && compaction_attempts < MAX_COMPACTION_ATTEMPTS =>
                {
                    // Mirror opencode's `compaction.compactIfNeeded`
                    // + `compactAfterOverflow`: ask the LLM to
                    // summarize the conversation, then retry the
                    // turn. We attempt auto-compaction up to
                    // `MAX_COMPACTION_ATTEMPTS` times per `run` call
                    // to recover from both the proactive estimate
                    // and provider-side 4xx/5xx responses that look
                    // like context overflow.
                    if let Some(compactor) = &self.compactor {
                        let session = self
                            .session_service
                            .get_session(session_id)
                            .map_err(|e| SessionError::Conversation(e))?
                            .ok_or_else(|| SessionError::SessionNotFound(session_id.to_string()))?;
                        compaction_attempts += 1;
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
                                    attempt = compaction_attempts,
                                    max_attempts = MAX_COMPACTION_ATTEMPTS,
                                    triggered_by = self.compaction_trigger_label(&error),
                                    "auto-compaction succeeded; retrying turn"
                                );
                                continue;
                            }
                            outcome => {
                                tracing::warn!(
                                    session_id = %session_id,
                                    step = step,
                                    attempt = compaction_attempts,
                                    max_attempts = MAX_COMPACTION_ATTEMPTS,
                                    outcome = ?outcome,
                                    "auto-compaction did not shrink the conversation; surfacing error"
                                );
                                return Err(error);
                            }
                        }
                    } else {
                        return Err(error);
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

    /// Build the harness-side tool dispatcher handed to the
    /// SDK provider. Every tool call the model issues goes
    /// through this closure, which runs the same v2 permission
    /// service check, doom-loop detector, and sandboxed
    /// `run_tool_*` dispatch the manual dispatcher used to
    /// apply on the HTTP path. The SDK's `handle_tool_call`
    /// loop simply hands us the resolved `ToolCallInfo`, gets
    /// the result string back, and feeds it to the model as
    /// `Message::Tool`.
    ///
    /// The closure is `Fn` (sync) because the AI SDK's
    /// `ToolExecute::new` accepts `Box<dyn Fn(Value) -> Result<String, String>>`.
    /// Our `run_tool_*` paths are async; we drive them via
    /// `Handle::current().block_on(...)` since the SDK already
    /// invokes the closure from inside its own tokio runtime.
    fn build_harness_tool_dispatcher(
        &self,
        session_id: &str,
        step: u32,
        mode: crate::permission::PermissionMode,
        session_directory: String,
        project_root: PathBuf,
    ) -> Arc<ToolDispatcherFn> {
        let session_id_owned = session_id.to_string();
        let sandboxed_ctx = self.sandboxed_ctx.clone();
        let subagent_registry = self.subagent_registry.clone();
        let providers = Arc::clone(&self.providers);
        let session_service = Arc::clone(&self.session_service);
        let conversation_service = Arc::clone(&self.conversation_service);
        let doom = Arc::clone(&self.doom);
        let spill_dir = self.spill_dir.clone();
        let noop_registry = self.subagent_registry_noop();

        Arc::new(move |tool_name: &str, input: serde_json::Value| {
            // The executor runs synchronously inside the SDK's
            // tokio task. We `block_on` a small async block that
            // does the v2 permission check, doom-loop detector,
            // and sandbox dispatch — i.e. every guard the
            // hand-rolled dispatcher applied to HTTP-path tool
            // calls also applies to SDK-path tool calls.
            let handle = match Handle::try_current() {
                Ok(handle) => handle,
                Err(_) => {
                    return Err(
                        "tool dispatcher invoked outside a tokio runtime".to_string(),
                    );
                }
            };

            let tool_name_owned = tool_name.to_string();
            let arguments = if let Some(obj) = input.as_object() {
                obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
            } else {
                std::collections::HashMap::new()
            };
            let tool_call = crate::domain::ToolCall {
                name: tool_name_owned.clone(),
                arguments: arguments.clone(),
            };

            // Doom-loop guard. Same threshold the manual
            // dispatcher used: three identical calls in a row
            // trips an `Err` that the model sees as a tool
            // error and self-corrects on the next turn.
            let args_fingerprint = serde_json::to_string(&arguments)
                .unwrap_or_else(|_| "{}".to_string());
            if doom.record(&tool_name_owned, &args_fingerprint) {
                let message = format!(
                    "doom loop: the same `{tool_name_owned}` call has been issued 3 times in a row. \
                     Vary your inputs or take a different approach."
                );
                doom.reset();
                return Err(message);
            }

            // Sandbox dispatch — same path the manual dispatcher
            // took. We branch on async-vs-sync dispatch the same
            // way `run_turn` did so `task`, peer-targeted plan-mode
            // tools, and the network tools still get a
            // `ToolRuntime`.
            let ctx = ToolContext::new(mode).with_project_root(project_root.clone());
            let sandboxed = sandboxed_ctx.as_deref();
            let has_peer_session_id = arguments
                .get("peer_session_id")
                .and_then(|value| value.as_str())
                .is_some_and(|value| !value.trim().is_empty());
            let needs_async_runtime =
                tool_name_owned == "task" || has_peer_session_id;
            let needs_async_dispatch = needs_async_runtime
                || tool_name_owned == "webfetch"
                || tool_name_owned == "websearch";

            let result = handle.block_on(async {
                if let Some(registry) = &subagent_registry {
                    let runtime = ToolRuntime {
                        caller_session_id: session_id_owned.clone(),
                        session_service: Arc::clone(&session_service),
                        conversation_service: Arc::clone(&conversation_service),
                        providers: Arc::clone(&providers),
                        subagent_registry: Arc::clone(registry),
                        mode,
                        turn_id: step as u64,
                        project_root: project_root.clone(),
                    };
                    if needs_async_dispatch {
                        if let Some(sandboxed) = sandboxed {
                            run_tool_async_sandboxed(&tool_call, &runtime, sandboxed).await
                        } else {
                            run_tool_async(&tool_call, &runtime).await
                        }
                    } else if let Some(sandboxed) = sandboxed {
                        run_tool_sandboxed(&tool_call, sandboxed).await
                    } else {
                        run_tool_with_context(&tool_call, &ctx)
                    }
                } else if needs_async_dispatch
                    && (tool_name_owned == "webfetch" || tool_name_owned == "websearch")
                {
                    let runtime = ToolRuntime {
                        caller_session_id: session_id_owned.clone(),
                        session_service: Arc::clone(&session_service),
                        conversation_service: Arc::clone(&conversation_service),
                        providers: Arc::clone(&providers),
                        subagent_registry: Arc::clone(&noop_registry),
                        mode,
                        turn_id: step as u64,
                        project_root: project_root.clone(),
                    };
                    if let Some(sandboxed) = sandboxed {
                        run_tool_async_sandboxed(&tool_call, &runtime, sandboxed).await
                    } else {
                        run_tool_async(&tool_call, &runtime).await
                    }
                } else if let Some(sandboxed) = sandboxed {
                    run_tool_sandboxed(&tool_call, sandboxed).await
                } else {
                    run_tool_with_context(&tool_call, &ctx)
                }
            });

            // Bound the persisted tool result so a single call
            // can't blow the model context window — same logic
            // the manual dispatcher used. Emit the
            // `LlmEvent::ToolResult` so the runner-side
            // persistence block picks it up.
            let raw_output = result.output.clone();
            let bounded = if result.success {
                let raw = raw_output.as_str();
                let spill = spill_dir.as_deref();
                let out = if tool_name_owned == "shell" {
                    crate::tools::output_bounds::tail_bound_output(raw, spill, &tool_name_owned)
                } else {
                    bound_tool_output(raw, spill, &tool_name_owned)
                };
                if out.truncated {
                    crate::tools::output_bounds::note_truncation();
                }
                out
            } else {
                crate::tools::BoundedOutput {
                    text: raw_output.clone(),
                    truncated: false,
                    dropped_lines: 0,
                    spill_path: None,
                }
            };
            let result_value = if !result.success {
                serde_json::json!({ "error": result.error.unwrap_or_default() })
            } else if bounded.truncated {
                let mut value = serde_json::json!({
                    "text": bounded.text,
                    "truncated": true,
                    "max_lines": MAX_TOOL_OUTPUT_LINES,
                    "max_bytes": MAX_TOOL_OUTPUT_BYTES,
                });
                if let Some(path) = &bounded.spill_path {
                    value["spill_path"] = serde_json::Value::String(path.clone());
                }
                value
            } else {
                serde_json::json!({ "text": bounded.text })
            };

            // The SDK wraps whatever string we return into
            // `Message::Tool`. We return the canonical JSON the
            // manual dispatcher would have stored on the
            // assistant message, so the model sees the same
            // tool result on the next turn.
            serde_json::to_string(&result_value).map_err(|err| err.to_string())
        })
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

        // Inject the prior compaction summary (if any) as a
        // synthetic user message and then replay the structured
        // recent-tail messages verbatim. This mirrors opencode's
        // message-v2 → model-messages converter: a `<compaction>`
        // part is rendered as a user message carrying the prior
        // summary, then the kept tail is replayed as real
        // conversation messages between that summary and the
        // post-tail user input. The runner never re-emits a
        // prior `<recent-context>` system block as model input —
        // that was the previous behavior, and it produced a JSON
        // blob the model could not reason over.
        if let Some(summary) = conversation.summary.as_deref() {
            if !summary.trim().is_empty() {
                messages.insert(
                    0,
                    LlmMessage::user(&format!(
                        "<conversation-summary>\n{summary}\n</conversation-summary>"
                    )),
                );
            }
        }
        if !conversation.recent_messages.is_empty() {
            // Place the recent-tail messages between the synthetic
            // summary (if any) and the live history. `get_messages`
            // returned only the post-compaction messages, so the
            // tail is the bridge from summary → current turn.
            let tail = history_to_llm_messages(&conversation.recent_messages);
            let insert_at = if !conversation
                .summary
                .as_deref()
                .map(str::trim)
                .map(str::is_empty)
                .unwrap_or(true)
            {
                1
            } else {
                0
            };
            let tail_len = tail.len();
            messages.splice(insert_at..insert_at, tail);
            // `insert_at` and `tail_len` are kept to make the splice
            // self-documenting; the `_ =` suppresses any unused-warning
            // if a future refactor moves the computation.
            let _ = tail_len;
        } else if let Some(legacy) = history
            .iter()
            .find(|msg| msg.role == "system" && msg.content.starts_with("<recent-context>"))
        {
            // Back-compat: a session compacted by the old code path
            // stored a single `<recent-context>` system message in
            // `messages`. The runner surfaces that text as a user
            // message so the model at least sees the recent turn
            // content. Subsequent compactions will rewrite the file
            // into the new structured shape.
            let body = legacy
                .content
                .trim_start_matches("<recent-context>")
                .trim_end_matches("</recent-context>")
                .trim();
            if !body.is_empty() {
                messages.insert(
                    0,
                    LlmMessage::user(&format!("<recent-context>\n{body}\n</recent-context>")),
                );
            }
        }

        let peer_context = if let Ok(block) =
            crate::routing::resolver::build_context_block(session_id, &self.session_service)
        {
            block.render()
        } else {
            None
        };

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

        let mut system_prompt = system_prompt_for_session(&session.provider, &session.directory, &[]);
        if let Some(peer_context) = peer_context {
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&peer_context);
        }

        // Pre-check the assembled request body against the model's
        // hard context window. We inspect the *outgoing* request
        // (system + history + tools), not the provider response, so
        // this is the only place that decides "the request is too
        // big" — provider-side 4xx/5xx (auth, rate limit, bad
        // gateway, etc.) never trigger compaction.
        //
        // The compactor is the single source of truth for the
        // estimate: when present, both `should_compact` (which adds
        // the configured `buffer_tokens` for the compactor's
        // summary headroom) and `request_fits` (the bare
        // `context_window - max_output` check used when the
        // compactor is configured but `auto` is off, or when the
        // runner is built without an auto-compactor at all) live on
        // the compactor and inspect the same assembled request the
        // provider is about to receive.
        let tools = tools_for_model(&session.model, self.readonly_tools);
        let precheck_request = LlmRequest::new(&session.model, &session.provider)
            .with_system(system_prompt.clone())
            .with_messages(messages.clone())
            .with_tools(tools.clone())
            .with_session_id(session_id.to_string());
        let spec = self
            .model_registry
            .lookup(&session.provider, &session.model);
        let overflow = match &self.compactor {
            Some(compactor) => compactor
                .should_compact(&precheck_request)
                .map(|estimated| (estimated, "compactor_should_compact")),
            None => {
                let estimator = crate::sessions::compaction::RequestFitsEstimator::new(Arc::clone(
                    &self.model_registry,
                ));
                if !estimator.request_fits(&precheck_request) {
                    let estimated = estimator.estimate_request_tokens(&precheck_request);
                    Some((estimated, "request_fits"))
                } else {
                    None
                }
            }
        };
        if let Some((estimated_tokens, source)) = overflow {
            tracing::warn!(
                session_id = %session_id,
                provider = %session.provider,
                model = %session.model,
                estimated_tokens,
                context_window = spec.context_window,
                max_output = spec.max_output,
                source = source,
                "assembled request body exceeds model context window; refusing to dispatch"
            );
            return Err(SessionError::ContextOverflow {
                session_id: session_id.to_string(),
                estimated_tokens,
                context_window: spec.context_window,
            });
        }

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
            has_compaction_summary = conversation.summary.is_some(),
            recent_tail_messages = conversation.recent_messages.len(),
            system_prompt_preview = %system_prompt.chars().take(2048).collect::<String>(),
            "llm request assembled: sending to provider"
        );

        let request = LlmRequest::new(&session.model, &session.provider)
            .with_system(system_prompt.clone())
            .with_messages(messages)
            .with_tools(tools)
            .with_session_id(session_id.to_string());

        let provider = self
            .providers
            .get(&session.provider)
            .await
            .ok_or(SessionError::NoProviderForSession)?;

        // The harness dispatcher needs the resolved project
        // root up front so it can pass the same `ToolContext`
        // into `run_tool_*` that the manual dispatcher uses.
        // Same construction as the post-stream block below.
        let dispatcher_project_root = std::path::PathBuf::from(&session.directory);

        // Wire the harness-side tool dispatcher for SDK-backed
        // providers so the AI SDK's `handle_tool_call` loop
        // routes through the same v2 permission service,
        // doom-loop detector, and sandboxed `run_tool_*` paths
        // the manual dispatcher used. The HTTP backend ignores
        // this — it does its own tool dispatch from the
        // streamed `LlmEvent::ToolCall` events. We always wire
        // it (the HTTP backend no-ops) so the dispatcher is
        // uniformly available across backends.
        let dispatcher = self.build_harness_tool_dispatcher(
            session_id,
            step,
            self.mode,
            session.directory.clone(),
            dispatcher_project_root,
        );
        provider.set_tool_dispatcher(dispatcher);

        let stream = provider
            .stream(request)
            .await
            .map_err(|e| SessionError::Llm(e))?;

        tracing::info!(
            session_id = %session_id,
            step = step,
            provider = %session.provider,
            provider_backend = %provider.backend_name(),
            provider_base_url = ?provider.model_base_url(),
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
        // XML tool call detector. Some local / instruct-tuned models
        // emit tool calls inside the text stream as XML blocks
        // (`<tool_call>…</tool_call>`, `<tool name="x">…</tool>`,
        // `<invoke …>`, `<function_calls>…</function_calls>`,
        // `<antml:function_calls>…</antml:function_calls>`,
        // self-closing variants). The scanner strips these from the
        // visible text and emits `LlmEvent::InvalidToolCall` events
        // so the runner can append a synthetic `tool_result` to the
        // assistant message and the model self-corrects on the next
        // turn.
        let mut xml_scanner = XmlToolCallScanner::new();
        let mut invalid_tool_call_hits: Vec<InvalidToolCallHit> = Vec::new();
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
                LlmEvent::InvalidToolCall { .. } => "invalid_tool_call",
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
                    // Scan the delta for XML tool call blocks. The
                    // scanner returns the visible text (with any
                    // detected blocks stripped) and a list of
                    // complete hits. We feed the visible text into
                    // the existing buffer so <think>…</think>
                    // detection still works, and we surface each
                    // hit as a typed `LlmEvent::InvalidToolCall`
                    // event so the UI can render a "the model
                    // emitted a tool call as XML" indicator.
                    let outcome = xml_scanner.feed(&text);
                    if !outcome.text.is_empty() {
                        text_buffer.push_str(&outcome.text);
                    }
                    for hit in outcome.hits {
                        let id = crate::llm::xml_tool_call::next_invalid_tool_call_id();
                        let (name, raw) = (hit.name.clone(), hit.raw.clone());
                        let event = LlmEvent::InvalidToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            raw: raw.clone(),
                        };
                        self.emit_event(session_id, step, &event);
                        invalid_tool_call_hits.push(hit);
                        let raw_preview: String = raw.chars().take(120).collect();
                        tracing::warn!(
                            session_id = %session_id,
                            step = step,
                            tool = %id,
                            name = %name,
                            raw_preview = %raw_preview,
                            "detected XML tool call block in streamed text; stripping and feeding back as tool_result"
                        );
                    }
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
                    provider_executed,
                } => {
                    needs_continuation = true;
                    let input_str = serde_json::to_string(&input).unwrap_or_default();
                    assistant_parts.push(ContentPart::tool_call(&id, &name, input.clone()));
                    let provider_executed = provider_executed.unwrap_or(false);
                    if !provider_executed {
                        pending_tool_calls.push((id.clone(), name.clone(), input_str));
                    }
                    tracing::info!(
                        session_id = %session_id,
                        step = step,
                        tool = %name,
                        tool_call_id = %id,
                        source = "structured",
                        provider_executed,
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

        // Flush the XML tool call scanner. Any in-flight block that
        // never closed (model emitted a partial tag) is reported
        // here as a synthetic hit so the model still gets
        // `tool_result` feedback on the next turn.
        let tail = xml_scanner.flush();
        if !tail.text.is_empty() {
            text_buffer.push_str(&tail.text);
            flush_text_buffer(&mut text_buffer, &mut in_think_block, &mut assistant_parts);
        }
        for hit in tail.hits {
            let id = crate::llm::xml_tool_call::next_invalid_tool_call_id();
            let (name, _) = (hit.name.clone(), hit.raw.clone());
            let event = LlmEvent::InvalidToolCall {
                id: id.clone(),
                name: hit.name.clone(),
                raw: hit.raw.clone(),
            };
            self.emit_event(session_id, step, &event);
            invalid_tool_call_hits.push(hit);
            tracing::warn!(
                session_id = %session_id,
                step = step,
                tool = %id,
                name = %name,
                "stream ended inside an XML tool call block; emitting synthetic hit"
            );
        }

        // Synthesize `ContentPart::tool_result` entries for every
        // detected XML tool call block. These ride along with the
        // persisted assistant message and reach the model on the
        // next turn through the standard tool-result channel so it
        // can self-correct.
        for hit in &invalid_tool_call_hits {
            let id = crate::llm::xml_tool_call::next_invalid_tool_call_id();
            let preview: String = hit.raw.chars().take(120).collect();
            let message = format!(
                "invalid tool call: do not emit tool calls as XML, markdown, code blocks, or plain text. \
                 The invalid XML tool call block you emitted for `{}` was discarded. \
                 Use the provider's native tool-calling API (the structured `tools` field on the request). \
                 Detected body: {}",
                hit.name,
                preview
            );
            assistant_parts.push(ContentPart::tool_result(
                &id,
                &hit.name,
                serde_json::json!({ "error": message }),
            ));
        }
        if !invalid_tool_call_hits.is_empty() {
            if pending_tool_calls.is_empty() {
                needs_continuation = true;
            }
            tracing::info!(
                session_id = %session_id,
                step = step,
                count = invalid_tool_call_hits.len(),
                needs_continuation = needs_continuation,
                pending_tool_calls = pending_tool_calls.len(),
                "appended invalid XML-tool-call feedback tool_results to assistant parts"
            );
            let _ = flush_parts(&assistant_parts);
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
                process_cwd = %std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "<unknown>".to_string()),
                "dispatching tool calls: built ToolContext with project_root from session; \
                 per-tool path resolution is logged by tools/mod.rs::resolve_session_path"
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
                if self.sandboxed_ctx.is_none() && self.doom.record(&name, &input_str) {
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

                // Dispatch through the async path so `task` can drive child
                // sessions, peer-targeted plan-mode tools can create/reuse
                // subsessions, and network tools can await HTTP without blocking
                // the executor.
                let has_peer_session_id = tool_call
                    .arguments
                    .get("peer_session_id")
                    .and_then(|value| value.as_str())
                    .is_some_and(|value| !value.trim().is_empty());
                let needs_async_runtime = name == "task" || has_peer_session_id;
                let needs_async_dispatch =
                    needs_async_runtime || name == "webfetch" || name == "websearch";
                let sandboxed = self.sandboxed_ctx.as_deref();
                let result = if let Some(registry) = &self.subagent_registry {
                    let runtime = ToolRuntime {
                        caller_session_id: session_id.to_string(),
                        session_service: Arc::clone(&self.session_service),
                        conversation_service: Arc::clone(&self.conversation_service),
                        providers: Arc::clone(&self.providers),
                        subagent_registry: Arc::clone(registry),
                        mode: self.mode,
                        turn_id: step as u64,
                        project_root: project_root.clone(),
                    };
                    if needs_async_dispatch {
                        tracing::debug!(
                            session_id = %session_id,
                            step = step,
                            tool = %name,
                            project_root = %project_root.display(),
                            "dispatching via async path (subagent registry present)"
                        );
                        if let Some(sandboxed) = sandboxed {
                            run_tool_async_sandboxed(&tool_call, &runtime, sandboxed).await
                        } else {
                            run_tool_async(&tool_call, &runtime).await
                        }
                    } else {
                        tracing::debug!(
                            session_id = %session_id,
                            step = step,
                            tool = %name,
                            project_root = %project_root.display(),
                            "dispatching via sync path (subagent registry present, tool doesn't need it)"
                        );
                        if let Some(sandboxed) = sandboxed {
                            run_tool_sandboxed(&tool_call, sandboxed).await
                        } else {
                            run_tool_with_context(&tool_call, &ctx)
                        }
                    }
                } else if needs_async_dispatch && (name == "webfetch" || name == "websearch") {
                    tracing::debug!(
                        session_id = %session_id,
                        step = step,
                        tool = %name,
                        project_root = %project_root.display(),
                        "dispatching via async path (no subagent registry; tool needs async I/O)"
                    );
                    // For the no-registry case the async network tools do not
                    // need a runtime themselves. Build a minimal `ToolRuntime`
                    // whose `subagent_registry` is never consulted.
                    let runtime = ToolRuntime {
                        caller_session_id: session_id.to_string(),
                        session_service: Arc::clone(&self.session_service),
                        conversation_service: Arc::clone(&self.conversation_service),
                        providers: Arc::clone(&self.providers),
                        subagent_registry: self.subagent_registry_noop(),
                        mode: self.mode,
                        turn_id: step as u64,
                        project_root: project_root.clone(),
                    };
                    if let Some(sandboxed) = sandboxed {
                        run_tool_async_sandboxed(&tool_call, &runtime, sandboxed).await
                    } else {
                        run_tool_async(&tool_call, &runtime).await
                    }
                } else {
                    tracing::debug!(
                        session_id = %session_id,
                        step = step,
                        tool = %name,
                        project_root = %project_root.display(),
                        "dispatching via sync path"
                    );
                    if let Some(sandboxed) = sandboxed {
                        run_tool_sandboxed(&tool_call, sandboxed).await
                    } else {
                        run_tool_with_context(&tool_call, &ctx)
                    }
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
                //
                // Tool-specific policy: `shell` uses
                // `tail_bound_output` (opencode's `tail()` function)
                // because the relevant signal — exit code, error
                // trace, final state — is at the end of the output,
                // not the beginning. Every other tool uses the
                // head-bounded `bound_tool_output`. When
                // `self.spill_dir` is set, the *full* untruncated
                // text is written to disk and the path surfaces in
                // the marker so the model can re-read it.
                let bounded = if result.success {
                    let raw = output.as_deref().unwrap_or("");
                    let spill = self.spill_dir.as_deref();
                    let out = if name == "shell" {
                        crate::tools::output_bounds::tail_bound_output(raw, spill, &name)
                    } else {
                        bound_tool_output(raw, spill, &name)
                    };
                    if out.truncated {
                        crate::tools::output_bounds::note_truncation();
                    }
                    out
                } else {
                    crate::tools::BoundedOutput {
                        text: output.as_deref().unwrap_or("").to_string(),
                        truncated: false,
                        dropped_lines: 0,
                        spill_path: None,
                    }
                };
                let persisted_output = if bounded.truncated {
                    Some(bounded.text.clone())
                } else {
                    output.clone()
                };
                let result_value = if bounded.truncated {
                    let mut value = serde_json::json!({
                        "text": bounded.text,
                        "truncated": true,
                        "max_lines": MAX_TOOL_OUTPUT_LINES,
                        "max_bytes": MAX_TOOL_OUTPUT_BYTES,
                    });
                    if let Some(path) = &bounded.spill_path {
                        value["spill_path"] = serde_json::Value::String(path.clone());
                    }
                    value
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
            if let Some(registry) = &self.subagent_registry {
                registry.complete_peer_subsessions(session_id, step as u64);
            }
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

        if let Some(registry) = &self.subagent_registry {
            registry.complete_peer_subsessions(session_id, step as u64);
        }

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
            "tool" => {
                // Persisted tool results are stored as
                // `Vec<ContentPart>` with a single `ToolResult`
                // part. Re-emit them with the proper `role: "tool"`
                // shape so both the HTTP backend and the SDK path
                // can render the structured `tool_call_id` and
                // `name` on the next turn.
                if let Ok(parts) = serde_json::from_str::<Vec<ContentPart>>(&msg.content) {
                    if let Some(ContentPart::ToolResult { id, name, result }) = parts
                        .into_iter()
                        .find(|p| matches!(p, ContentPart::ToolResult { .. }))
                    {
                        LlmMessage::tool(&id, &name, result)
                    } else {
                        LlmMessage::user(&msg.content)
                    }
                } else {
                    LlmMessage::user(&msg.content)
                }
            }
            _ => LlmMessage::user(&msg.content),
        })
        .collect()
}

fn system_prompt_for_session(provider: &str, session_directory: &str, _extra: &[String]) -> String {
    let agent_name = match provider {
        "anthropic" => "Claude",
        "openai" => "GPT",
        "minimax" => "MiniMax",
        _ => "AI Assistant",
    };
    let project_name = project_name_from_directory(session_directory);

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
         # Session project\n\
         - Main project: {project_name}\n\
         - Project path: {project_path}\n\
         - Treat this path as the primary working tree for this session unless the user explicitly points elsewhere.\n\n\
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
         Do not call a tool that isn't listed. Do not invent argument names. \
         The runtime will strip and reject any XML tool call block it sees \
         in your text and feed the rejection back as a `tool_result` so you \
         can self-correct on the next turn.",
        name = agent_name,
        project_name = project_name,
        project_path = session_directory,
    )
}

fn project_name_from_directory(directory: &str) -> &str {
    let trimmed = directory.trim_end_matches(|c| c == '/' || c == '\\');
    trimmed
        .rsplit(|c| c == '/' || c == '\\')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(directory)
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
    let mut tools = vec![
        crate::llm::events::ToolDefinition::new(
            "apply_patch",
            "Apply a file-oriented patch",
            object_schema(
                serde_json::json!({
                    "patchText": { "type": "string", "description": "Full patch text describing file operations" }
                }),
                &["patchText"],
            ),
        ),
        crate::llm::events::ToolDefinition::new(
            "edit",
            "Replace text in an existing file",
            object_schema(
                serde_json::json!({
                    "path": { "type": "string", "description": "Path to the file to edit" },
                    "old_string": { "type": "string", "description": "Text to find and replace" },
                    "new_string": { "type": "string", "description": "Replacement text" }
                }),
                &["path", "old_string", "new_string"],
            ),
        ),
        crate::llm::events::ToolDefinition::new(
            "glob",
            "Find files by glob pattern",
            object_schema(
                serde_json::json!({
                    "path": { "type": "string", "description": "Root directory to search from" },
                    "pattern": { "type": "string", "description": "Glob pattern to match files against" }
                }),
                &["path"],
            ),
        ),
        crate::llm::events::ToolDefinition::new(
            "grep",
            "Search file contents",
            object_schema(
                serde_json::json!({
                    "path": { "type": "string", "description": "Root directory to search from" },
                    "pattern": { "type": "string", "description": "Text pattern to search for" },
                    "include": { "type": "string", "description": "File name pattern to filter by" }
                }),
                &["path", "pattern"],
            ),
        ),
        crate::llm::events::ToolDefinition::new(
            "read",
            "Read a file from disk",
            object_schema(
                serde_json::json!({
                    "path": { "type": "string", "description": "Path to the file to read" },
                    "offset": { "type": "integer", "description": "Line offset to start reading from", "minimum": 1 },
                    "limit": { "type": "integer", "description": "Maximum number of lines to read" }
                }),
                &["path"],
            ),
        ),
        crate::llm::events::ToolDefinition::new(
            "shell",
            "Run a shell command",
            object_schema(
                serde_json::json!({
                    "command": { "type": "string", "description": "Shell command to execute" },
                    "workdir": { "type": "string", "description": "Working directory for the command" }
                }),
                &["command"],
            ),
        ),
        crate::llm::events::ToolDefinition::new(
            "todo",
            "Update the session todo list",
            object_schema(
                serde_json::json!({
                    "content": { "type": "string", "description": "Todo content to set" }
                }),
                &["content"],
            ),
        ),
        crate::llm::events::ToolDefinition::new(
            "write",
            "Write content to a file",
            object_schema(
                serde_json::json!({
                    "path": { "type": "string", "description": "Path to the file to write" },
                    "content": { "type": "string", "description": "Content to write" }
                }),
                &["path", "content"],
            ),
        ),
    ];
    sort_tools_by_name(&mut tools);
    tools
}

/// Read-only tool subset used by Plan mode. Excludes `write`, `edit`,
/// `apply_patch`, `shell`, and `task`. Read/glob/grep can target connected
/// peers by passing `peer_session_id` from the `<peers>` system context.
pub fn readonly_tool_definitions() -> Vec<crate::llm::events::ToolDefinition> {
    let mut tools = vec![
        crate::llm::events::ToolDefinition::new(
            "glob",
            "Find files by glob pattern",
            object_schema(
                serde_json::json!({
                    "path": { "type": "string", "description": "Root directory" },
                    "pattern": { "type": "string", "description": "Glob pattern" },
                    "peer_session_id": { "type": "string", "description": "Optional. Use ONLY to search a different connected session listed in <peers>. Omit for local/current-repo work." }
                }),
                &["path"],
            ),
        ),
        crate::llm::events::ToolDefinition::new(
            "grep",
            "Search file contents",
            object_schema(
                serde_json::json!({
                    "path": { "type": "string" },
                    "pattern": { "type": "string" },
                    "include": { "type": "string" },
                    "peer_session_id": { "type": "string", "description": "Optional. Use ONLY to search a different connected session listed in <peers>. Omit for local/current-repo work." }
                }),
                &["path", "pattern"],
            ),
        ),
        crate::llm::events::ToolDefinition::new(
            "read",
            "Read a file from disk",
            object_schema(
                serde_json::json!({
                    "path": { "type": "string", "description": "Path to a file to read" },
                    "offset": { "type": "integer", "minimum": 1 },
                    "limit": { "type": "integer", "minimum": 1 },
                    "peer_session_id": { "type": "string", "description": "Optional. Use ONLY to read a file from a different connected session listed in <peers>. Omit for local/current-repo work." }
                }),
                &["path"],
            ),
        ),
        crate::llm::events::ToolDefinition::new(
            "webfetch",
            "Fetch a web URL",
            object_schema(
                serde_json::json!({
                    "url": { "type": "string" }
                }),
                &["url"],
            ),
        ),
        crate::llm::events::ToolDefinition::new(
            "websearch",
            "Search the web",
            object_schema(
                serde_json::json!({
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 20 }
                }),
                &["query"],
            ),
        ),
    ];
    sort_tools_by_name(&mut tools);
    tools
}

/// Wrap a property map in the canonical object schema envelope:
/// `type: object`, `properties`, `required`, and
/// `additionalProperties: false`. The latter is mandatory for
/// OpenAI strict-mode tools and a useful hint everywhere else: it
/// stops the model from inventing field names that aren't in the
/// schema, which would otherwise round-trip as parse errors.
fn object_schema(properties: serde_json::Value, required: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

/// Names of the file-mutation tools that we surface to the model.
/// The GPT family gets `apply_patch` instead of `edit`/`write`
/// (matching opencode's `ToolRegistry.tools` filter) because GPT
/// is trained on the diff-style `*** Begin Patch` format. Other
/// models are better at find-and-replace.
const EDIT_TOOLS: &[&str] = &["edit", "write"];
const APPLY_PATCH_TOOL: &str = "apply_patch";

/// True when the model is in the GPT family that supports the
/// `*** Begin Patch` diff format. Mirrors opencode's
/// `usePatch = modelID.includes("gpt-") && !modelID.includes("oss")
/// && !modelID.includes("gpt-4")` — the `-chat` suffix is excluded
/// because gpt-5-chat is a non-tool-calling chat model, and `gpt-4`
/// is excluded because the original gpt-4 was not trained on
/// `apply_patch` either. The `-oss` suffix excludes the open-weight
/// gpt-oss family, which we don't currently support but the rule
/// keeps room for it.
fn model_uses_apply_patch(model_id: &str) -> bool {
    let id = model_id.to_lowercase();
    id.contains("gpt-")
        && !id.contains("gpt-4")
        && !id.contains("oss")
        && !id.contains("gpt-5-chat")
}

/// Return the tool list to advertise to the model for this turn.
///
/// The filter is layered:
/// 1. If `readonly` is set (Plan mode),
///    return the read-only subset regardless of model. Plan mode
///    is a safety gate and must win over the GPT/preferred-edit-tool
///    heuristic — a Plan-mode GPT-5 still can't write.
/// 2. Otherwise, start with the full `default_tool_definitions()`.
///    For GPT-family models that speak `apply_patch`, drop `edit`
///    and `write`. For everything else, drop `apply_patch`.
/// 3. The output is sorted by name for prompt-cache stability —
///    `apply_patch` ↔ `edit`/`write` filtering must not change the
///    order of the other tools on the wire.
pub fn tools_for_model(model_id: &str, readonly: bool) -> Vec<crate::llm::events::ToolDefinition> {
    if readonly {
        let mut tools = readonly_tool_definitions();
        sort_tools_by_name(&mut tools);
        return tools;
    }

    let mut tools = default_tool_definitions();
    if model_uses_apply_patch(model_id) {
        tools.retain(|tool| !EDIT_TOOLS.contains(&tool.name.as_str()));
    } else {
        tools.retain(|tool| tool.name != APPLY_PATCH_TOOL);
    }
    sort_tools_by_name(&mut tools);
    tools
}

/// Sort tool definitions in-place by name. Tool order on the wire
/// is significant for prompt caching (Anthropic and OpenAI both
/// hash the literal request bytes; any reordering of the `tools`
/// array busts the cache prefix). Sorting by name makes the order
/// stable across turns and across optional-tool toggles (e.g.
/// `apply_patch` ↔ `edit`/`write` filtering by model).
fn sort_tools_by_name(tools: &mut [crate::llm::events::ToolDefinition]) {
    tools.sort_by(|a, b| a.name.cmp(&b.name));
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
        let openai_config = crate::ProviderConfig::new(
            "openai".to_string(),
            "gpt-4o".to_string(),
            crate::ProviderProtocol::OpenAI,
        );
        let anthropic_config = crate::ProviderConfig::new(
            "anthropic".to_string(),
            "claude-sonnet-4-20250514".to_string(),
            crate::ProviderProtocol::Anthropic,
        );
        let openai = crate::llm::providers::aisdk_provider_from_config(&openai_config)
            .expect("default openai provider must be supported by AISDK");
        let anthropic = crate::llm::providers::aisdk_provider_from_config(&anthropic_config)
            .expect("default anthropic provider must be supported by AISDK");
        let minimax = Arc::new(crate::llm::providers::MiniMaxTokenPlanProvider::new(
            None, None, false,
        )) as Arc<dyn LlmProvider>;

        // Note: using blocking insert since we're in a sync context
        // In async context use register() which does .write().await
        let mut guard = self.providers.blocking_write();
        guard.insert("openai".to_string(), openai);
        guard.insert("anthropic".to_string(), anthropic);
        guard.insert("minimax".to_string(), minimax);

        for provider_name in crate::llm::providers::aisdk_supported_provider_names() {
            if guard.contains_key(*provider_name) {
                continue;
            }

            let mut config = crate::ProviderConfig::new(
                provider_name.to_string(),
                default_aisdk_registry_model(provider_name),
                aisdk_registry_protocol(provider_name),
            );
            config.base_url = std::env::var(crate::llm::providers::aisdk_provider_base_url_env(
                provider_name,
            ))
            .ok();

            if let Some(provider) = crate::llm::providers::aisdk_provider_from_config(&config) {
                guard.insert(provider_name.to_string(), provider);
            }
        }
    }
}

fn default_aisdk_registry_model(provider_name: &str) -> String {
    std::env::var(crate::llm::providers::aisdk_provider_model_env(
        provider_name,
    ))
    .ok()
    .filter(|model| !model.trim().is_empty())
    .unwrap_or_else(|| "default".to_string())
}

fn aisdk_registry_protocol(provider_name: &str) -> crate::ProviderProtocol {
    if provider_name == "anthropic" {
        crate::ProviderProtocol::Anthropic
    } else {
        crate::ProviderProtocol::OpenAI
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
        assert!(system_prompt_for_session("anthropic", "/work/openman", &[]).contains("Claude"));
        assert!(system_prompt_for_session("openai", "/work/openman", &[]).contains("GPT"));
        assert!(system_prompt_for_session("minimax", "/work/openman", &[]).contains("MiniMax"));
    }

    #[test]
    fn system_prompt_for_session_includes_project_context() {
        let prompt =
            system_prompt_for_session("openai", "C:\\Users\\mrowe\\Documents\\openman", &[]);

        assert!(prompt.contains("Main project: openman"));
        assert!(prompt.contains("Project path: C:\\Users\\mrowe\\Documents\\openman"));
    }

    fn tool_names(tools: &[crate::llm::events::ToolDefinition]) -> Vec<&str> {
        let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        names.sort();
        names
    }

    fn is_sorted_by_name(tools: &[crate::llm::events::ToolDefinition]) -> bool {
        tools.windows(2).all(|pair| pair[0].name <= pair[1].name)
    }

    #[test]
    fn model_uses_apply_patch_recognises_gpt5_family() {
        // Pure function, no I/O. The exact match set is the GPT-5
        // family that was trained on the `*** Begin Patch` format.
        assert!(model_uses_apply_patch("gpt-5"));
        assert!(model_uses_apply_patch("gpt-5-turbo"));
        assert!(model_uses_apply_patch("gpt-5-mini"));
        assert!(model_uses_apply_patch("openai/gpt-5"));
        assert!(model_uses_apply_patch("GPT-5"));
    }

    #[test]
    fn model_uses_apply_patch_rejects_non_gpt5_models() {
        // Claude, Gemini, llama, gpt-4, gpt-oss, gpt-5-chat,
        // minimax. None of these are trained on `apply_patch`.
        for id in [
            "claude-3-5-sonnet-latest",
            "claude-3-5-haiku-latest",
            "claude-sonnet-4-20250514",
            "gemini-1.5-pro",
            "gemini-2.5-pro",
            "llama-3.1-70b",
            "MiniMax-M3",
            "MiniMax-M2.7",
            "gpt-4",
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-4-turbo",
            "gpt-oss-120b",
            "gpt-5-chat-latest",
        ] {
            assert!(
                !model_uses_apply_patch(id),
                "{id} should NOT use apply_patch"
            );
        }
    }

    #[test]
    fn tools_for_model_gpt5_gets_apply_patch_and_no_edit_or_write() {
        let tools = tools_for_model("gpt-5", false);
        let names = tool_names(&tools);
        assert!(
            names.contains(&"apply_patch"),
            "apply_patch missing: {names:?}"
        );
        assert!(
            !names.contains(&"edit"),
            "edit should be dropped: {names:?}"
        );
        assert!(
            !names.contains(&"write"),
            "write should be dropped: {names:?}"
        );
        // All other tools still present.
        for name in ["glob", "grep", "read", "shell", "todo"] {
            assert!(names.contains(&name), "{name} missing: {names:?}");
        }
        assert!(
            is_sorted_by_name(&tools),
            "tools must be sorted by name: {tools:#?}"
        );
    }

    #[test]
    fn tools_for_model_claude_gets_edit_and_write_and_no_apply_patch() {
        let tools = tools_for_model("claude-sonnet-4-20250514", false);
        let names = tool_names(&tools);
        assert!(names.contains(&"edit"), "edit missing: {names:?}");
        assert!(names.contains(&"write"), "write missing: {names:?}");
        assert!(
            !names.contains(&"apply_patch"),
            "apply_patch should be dropped for Claude: {names:?}"
        );
        assert!(is_sorted_by_name(&tools));
    }

    #[test]
    fn tools_for_model_gpt4_falls_back_to_edit_and_write() {
        // gpt-4 was not trained on apply_patch.
        let tools = tools_for_model("gpt-4o", false);
        let names = tool_names(&tools);
        assert!(names.contains(&"edit"));
        assert!(names.contains(&"write"));
        assert!(
            !names.contains(&"apply_patch"),
            "gpt-4 should not get apply_patch: {names:?}"
        );
    }

    #[test]
    fn tools_for_model_minimax_and_gemini_get_edit_and_write() {
        // The non-GPT-5 providers all use the find-and-replace
        // (`edit`/`write`) tool pair.
        for id in ["MiniMax-M3", "gemini-1.5-pro", "llama-3.1-70b"] {
            let tools = tools_for_model(id, false);
            let names = tool_names(&tools);
            assert!(names.contains(&"edit"), "{id} missing edit: {names:?}");
            assert!(names.contains(&"write"), "{id} missing write: {names:?}");
            assert!(
                !names.contains(&"apply_patch"),
                "{id} should not get apply_patch: {names:?}"
            );
        }
    }

    #[test]
    fn tools_for_model_readonly_overrides_gpt_apply_patch() {
        // Plan mode is read-only regardless of model. Even gpt-5 must not see
        // write tools when the permission mode is Plan.
        let tools = tools_for_model("gpt-5", true);
        let names = tool_names(&tools);
        assert_eq!(
            names,
            vec!["glob", "grep", "read", "webfetch", "websearch"],
            "read-only mode must return exactly the read-only subset"
        );
        // Crucially: apply_patch is *not* exposed in read-only mode
        // even for gpt-5. Plan mode is a safety gate, not a
        // capability toggle.
        assert!(!names.contains(&"apply_patch"));
    }

    #[test]
    fn tools_for_model_readonly_for_claude() {
        let tools = tools_for_model("claude-sonnet-4-20250514", true);
        let names = tool_names(&tools);
        assert_eq!(names, vec!["glob", "grep", "read", "webfetch", "websearch"]);
    }

    #[test]
    fn tools_for_model_output_is_stable_across_models() {
        // The tool list must be alphabetically sorted so the wire
        // order is identical across all turns (cache stability).
        // Verify that both the GPT and Claude branches sort the
        // output the same way.
        let gpt = tools_for_model("gpt-5", false);
        let claude = tools_for_model("claude-3-5-sonnet-latest", false);
        assert!(is_sorted_by_name(&gpt));
        assert!(is_sorted_by_name(&claude));
    }

    #[test]
    fn tools_for_model_filters_are_disjoint() {
        // The two file-mutation toolsets are mutually exclusive —
        // no model ever sees both `apply_patch` and (`edit` or
        // `write`). The GPT branch keeps `apply_patch`; the
        // non-GPT branch keeps `edit`/`write`. Tuple shape is
        // (model_id, has_apply_patch, has_edit, has_write).
        let cases = [
            ("gpt-5", true, false, false), // GPT branch: apply_patch only
            ("gpt-4o", false, true, true), // non-GPT: edit + write
            ("claude", false, true, true),
            ("MiniMax-M3", false, true, true),
        ];
        for (id, has_patch, has_edit, has_write) in cases {
            let tools = tools_for_model(id, false);
            let names = tool_names(&tools);
            assert_eq!(
                names.contains(&"apply_patch"),
                has_patch,
                "apply_patch in {id}: {names:?}"
            );
            assert_eq!(names.contains(&"edit"), has_edit, "edit in {id}: {names:?}");
            assert_eq!(
                names.contains(&"write"),
                has_write,
                "write in {id}: {names:?}"
            );
        }
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

    #[test]
    fn flush_text_buffer_preserves_system_reminder_as_text() {
        let parts = flush_for_test("before <system-reminder>secret</system-reminder> after");
        let visible: String = parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(visible.contains("<system-reminder>secret</system-reminder>"));
        assert!(parts
            .iter()
            .all(|p| !matches!(p, ContentPart::Reasoning { .. })));
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
        /// When set, every `LlmRequest` the runner dispatches is
        /// cloned into this vec. Tests use it to assert on the
        /// exact tool list (and other request fields) the runner
        /// produced for a given model.
        captured_requests: Option<std::sync::Arc<std::sync::Mutex<Vec<LlmRequest>>>>,
    }

    impl ScriptedProvider {
        fn new(provider_name: &str, scripts: Vec<Vec<LlmEvent>>) -> Self {
            Self {
                provider_name: provider_name.to_string(),
                scripts: std::sync::Mutex::new(scripts),
                captured_requests: None,
            }
        }

        fn with_capture(
            provider_name: &str,
            scripts: Vec<Vec<LlmEvent>>,
        ) -> (Self, std::sync::Arc<std::sync::Mutex<Vec<LlmRequest>>>) {
            let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink_for_provider = std::sync::Arc::clone(&sink);
            (
                Self {
                    provider_name: provider_name.to_string(),
                    scripts: std::sync::Mutex::new(scripts),
                    captured_requests: Some(sink_for_provider),
                },
                sink,
            )
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

        async fn stream(&self, request: LlmRequest) -> Result<LlmStream, LlmError> {
            if let Some(sink) = &self.captured_requests {
                sink.lock().unwrap().push(request);
            }
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
        run_with_scripted_model(provider_name, scripts, session_directory, "scripted-model").await
    }

    async fn run_with_scripted_model(
        provider_name: &str,
        scripts: Vec<Vec<LlmEvent>>,
        session_directory: &str,
        model_id: &str,
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
                model_id.to_string(),
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

    async fn run_with_scripted_capture(
        provider_name: &str,
        scripts: Vec<Vec<LlmEvent>>,
        session_directory: &str,
        model_id: &str,
    ) -> (
        SessionRunner,
        TempDir,
        String,
        std::sync::Arc<std::sync::Mutex<Vec<LlmRequest>>>,
    ) {
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
        let (scripted, capture_sink) = ScriptedProvider::with_capture(provider_name, scripts);
        providers.register(Arc::new(scripted)).await;
        let runner = SessionRunner::new(session_service, conv_service, providers);

        let session_id = runner
            .session_service
            .create_session(
                "p1".to_string(),
                session_directory.to_string(),
                provider_name.to_string(),
                model_id.to_string(),
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
        (runner, tmp, session_id, capture_sink)
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

        let result = runner.run(&session_id).await.expect("run failed");
        assert_eq!(
            result.steps, 1,
            "finish-only turn should stop after the single provider call"
        );

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

    #[tokio::test]
    async fn provider_executed_tool_call_is_not_dispatched_again() {
        init_tracing();
        let session_root = TempDir::new().expect("tempdir");
        let session_root_str = session_root.path().to_str().unwrap().to_string();
        let call_id = "call_provider_executed_glob";

        let turn1 = vec![
            LlmEvent::ToolCall {
                id: call_id.to_string(),
                name: "glob".to_string(),
                input: serde_json::json!({ "path": session_root_str, "pattern": "*.rs" }),
                provider_executed: Some(true),
            },
            LlmEvent::ToolResult {
                id: call_id.to_string(),
                name: "glob".to_string(),
                result: ToolResultValue::Text {
                    value: "provider result".to_string(),
                },
                output: None,
            },
            LlmEvent::Finish {
                reason: FinishReason::ToolCalls,
                usage: None,
            },
        ];
        let turn2 = vec![LlmEvent::Finish {
            reason: FinishReason::Stop,
            usage: None,
        }];
        let (runner, _tmp, session_id) =
            run_with_scripted("scripted", vec![turn1, turn2], &session_root_str).await;

        let result = runner.run(&session_id).await;
        assert!(result.is_ok(), "run failed: {:?}", result.err());

        let msgs = runner
            .conversation_service
            .get_messages(&session_id)
            .expect("get_messages");
        let assistant = msgs
            .iter()
            .find(|m| m.role == "assistant" && m.content.contains(call_id))
            .expect("assistant message with provider-executed tool call");
        let parts: Vec<ContentPart> = serde_json::from_str(&assistant.content).unwrap();
        let results: Vec<&ContentPart> = parts
            .iter()
            .filter(|p| matches!(p, ContentPart::ToolResult { .. }))
            .collect();
        assert_eq!(
            results.len(),
            1,
            "provider-executed tool calls must not be manually dispatched again: {parts:#?}"
        );
        match results[0] {
            ContentPart::ToolResult { result, .. } => assert!(
                result.to_string().contains("provider result"),
                "expected the provider-supplied result, got: {result}"
            ),
            _ => unreachable!(),
        }
    }

    /// The XML tool call detector: when the model emits a tool
    /// call as a `<tool name="x">…</tool>` (or `<invoke …>`,
    /// `<function_calls>`, `<antml:function_calls>`, etc.) inside
    /// the text stream, the runner strips the block from the
    /// visible text, emits a typed `LlmEvent::InvalidToolCall`
    /// on the event sink, and appends a synthetic
    /// `ContentPart::tool_result` describing the violation to the
    /// assistant message so the model sees feedback on the next
    /// turn. This test drives a scripted provider that emits an
    /// XML block and asserts all three.
    #[tokio::test]
    async fn e2e_xml_tool_call_in_text_is_stripped_and_reported() {
        init_tracing();
        let session_root = TempDir::new().expect("tempdir");
        let session_root_str = session_root.path().to_str().unwrap().to_string();

        let model_text =
            "before <tool name=\"read\">{\"path\":\"/etc/hostname\"}</tool> after".to_string();
        let chunks = text_delta_chunks(&model_text, 7);
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

        let captured: Arc<std::sync::Mutex<Vec<LlmEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_for_sink = Arc::clone(&captured);
        let runner = runner.with_event_sink(Arc::new(move |ev: SessionRunEvent| {
            captured_for_sink.lock().unwrap().push(ev.event);
        }));

        let result = runner.run(&session_id).await;
        assert!(result.is_ok(), "run failed: {:?}", result.err());

        let emitted: Vec<LlmEvent> = captured.lock().unwrap().clone();
        let invalid: Vec<&LlmEvent> = emitted
            .iter()
            .filter(|e| matches!(e, LlmEvent::InvalidToolCall { .. }))
            .collect();
        assert_eq!(
            invalid.len(),
            1,
            "expected exactly one InvalidToolCall event, got: {emitted:#?}"
        );
        match invalid[0] {
            LlmEvent::InvalidToolCall { name, raw, .. } => {
                assert_eq!(
                    name, "read",
                    "invalid_tool_call.name should be the inner tool name"
                );
                assert!(
                    raw.contains("/etc/hostname"),
                    "raw body should contain the inner JSON, got: {raw:?}"
                );
            }
            _ => unreachable!(),
        }

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

        // The XML block must NOT be a real ToolCall part — it was
        // never an actual tool call.
        assert!(
            !parts
                .iter()
                .any(|p| matches!(p, ContentPart::ToolCall { .. })),
            "xml block must NOT be parsed as a real ToolCall, got: {parts:#?}"
        );

        // The runner must have appended a synthetic ToolResult
        // describing the violation so the model sees feedback on
        // the next turn.
        let feedback: Vec<&ContentPart> = parts
            .iter()
            .filter(|p| matches!(p, ContentPart::ToolResult { .. }))
            .collect();
        assert_eq!(
            feedback.len(),
            1,
            "expected one synthetic ToolResult with the violation, got: {parts:#?}"
        );
        match feedback[0] {
            ContentPart::ToolResult { name, result, .. } => {
                assert_eq!(name, "read", "feedback name should be the inner tool name");
                let serialized = serde_json::to_string(result).unwrap_or_default();
                assert!(
                    serialized.contains("invalid tool call")
                        && serialized.contains("do not emit tool calls as XML"),
                    "feedback message should tell the model to use the native tool-calling API, got: {serialized}"
                );
            }
            _ => unreachable!(),
        }

        // The visible text must NOT contain the XML block.
        let combined_text: String = parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        assert!(
            !combined_text.contains("<tool"),
            "xml block must be stripped from visible text, got: {combined_text:?}"
        );
        assert!(
            !combined_text.contains("</tool>"),
            "xml block close must be stripped from visible text, got: {combined_text:?}"
        );
        assert!(
            combined_text.contains("before") && combined_text.contains("after"),
            "surrounding text must remain visible, got: {combined_text:?}"
        );
    }

    /// End-to-end: a GPT-5 model session must receive `apply_patch`
    /// in the request body's tool list and must NOT receive `edit`
    /// or `write`. Mirrors opencode's per-model `apply_patch` ↔
    /// `edit`/`write` toggle (see `ToolRegistry.tools` in opencode).
    /// The test drives a scripted provider with a capture sink so
    /// we can introspect the exact `LlmRequest` the runner built.
    #[tokio::test]
    async fn e2e_gpt5_session_advertises_apply_patch_not_edit_or_write() {
        init_tracing();
        let (runner, _tmp, session_id, capture) = run_with_scripted_capture(
            "scripted",
            vec![vec![LlmEvent::Finish {
                reason: FinishReason::Stop,
                usage: None,
            }]],
            "/tmp",
            "gpt-5",
        )
        .await;

        let result = runner.run(&session_id).await;
        assert!(result.is_ok(), "run failed: {:?}", result.err());

        let captured: Vec<LlmRequest> = capture.lock().unwrap().clone();
        assert_eq!(
            captured.len(),
            1,
            "expected exactly one captured request, got {}",
            captured.len()
        );
        let request = &captured[0];
        let names: Vec<&str> = request.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(
            names.contains(&"apply_patch"),
            "apply_patch missing for gpt-5: {names:?}"
        );
        assert!(
            !names.contains(&"edit"),
            "edit should be dropped for gpt-5: {names:?}"
        );
        assert!(
            !names.contains(&"write"),
            "write should be dropped for gpt-5: {names:?}"
        );
        // Other tools still present so the model can read, search, etc.
        for name in ["glob", "grep", "read", "shell", "todo"] {
            assert!(names.contains(&name), "{name} missing for gpt-5: {names:?}");
        }
        // Stable wire order — prompt cache prefix depends on it.
        let sorted: Vec<&str> = {
            let mut s = names.clone();
            s.sort();
            s
        };
        assert_eq!(
            names, sorted,
            "tool list must be sorted alphabetically: {names:?}"
        );
    }

    /// End-to-end counterpart: a Claude session gets `edit` and
    /// `write` and does NOT get `apply_patch`.
    #[tokio::test]
    async fn e2e_claude_session_advertises_edit_and_write_not_apply_patch() {
        init_tracing();
        let (runner, _tmp, session_id, capture) = run_with_scripted_capture(
            "scripted",
            vec![vec![LlmEvent::Finish {
                reason: FinishReason::Stop,
                usage: None,
            }]],
            "/tmp",
            "claude-sonnet-4-20250514",
        )
        .await;

        let result = runner.run(&session_id).await;
        assert!(result.is_ok(), "run failed: {:?}", result.err());

        let captured: Vec<LlmRequest> = capture.lock().unwrap().clone();
        assert_eq!(captured.len(), 1);
        let request = &captured[0];
        let names: Vec<&str> = request.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(
            names.contains(&"edit"),
            "edit missing for claude: {names:?}"
        );
        assert!(
            names.contains(&"write"),
            "write missing for claude: {names:?}"
        );
        assert!(
            !names.contains(&"apply_patch"),
            "apply_patch should be dropped for claude: {names:?}"
        );
    }

    /// End-to-end: a minimax session (non-GPT, non-OSS) gets
    /// `edit` and `write` and not `apply_patch`, matching the
    /// default for everything that isn't the GPT-5 family.
    #[tokio::test]
    async fn e2e_minimax_session_advertises_edit_and_write_not_apply_patch() {
        init_tracing();
        let (runner, _tmp, session_id, capture) = run_with_scripted_capture(
            "scripted",
            vec![vec![LlmEvent::Finish {
                reason: FinishReason::Stop,
                usage: None,
            }]],
            "/tmp",
            "MiniMax-M3",
        )
        .await;

        let result = runner.run(&session_id).await;
        assert!(result.is_ok(), "run failed: {:?}", result.err());

        let captured: Vec<LlmRequest> = capture.lock().unwrap().clone();
        let names: Vec<&str> = captured[0].tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"edit"));
        assert!(names.contains(&"write"));
        assert!(!names.contains(&"apply_patch"));
    }

    /// Plan mode (read-only) on a GPT-5 model must not surface
    /// `apply_patch` even though the model class would normally
    /// get it. Plan mode is a safety gate, not a model preference.
    #[tokio::test]
    async fn e2e_plan_mode_on_gpt5_omits_apply_patch() {
        init_tracing();
        let (runner, _tmp, session_id, capture) = run_with_scripted_capture(
            "scripted",
            vec![vec![LlmEvent::Finish {
                reason: FinishReason::Stop,
                usage: None,
            }]],
            "/tmp",
            "gpt-5",
        )
        .await;
        let runner = runner.with_mode(crate::permission::PermissionMode::Plan);

        let result = runner.run(&session_id).await;
        assert!(result.is_ok(), "run failed: {:?}", result.err());

        let captured: Vec<LlmRequest> = capture.lock().unwrap().clone();
        let names: Vec<&str> = captured[0].tools.iter().map(|t| t.name.as_str()).collect();
        // Read-only subset only.
        assert_eq!(
            names,
            vec!["glob", "grep", "read", "webfetch", "websearch"],
            "Plan mode must be read-only regardless of model"
        );
        assert!(!names.contains(&"apply_patch"));
        assert!(!names.contains(&"edit"));
        assert!(!names.contains(&"write"));
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

    /// End-to-end: a `shell` tool call that produces more lines
    /// than the head-bounded cap should be tail-truncated (last N
    /// lines kept, head dropped) per opencode's `tail()` policy.
    /// A non-shell tool of the same size would keep the *first*
    /// N lines instead.
    #[tokio::test]
    async fn e2e_shell_uses_tail_bound_output() {
        use crate::llm::events::FinishReason;
        use crate::llm::events::LlmEvent;

        init_tracing();
        // Lines 0..MAX_TOOL_OUTPUT_LINES plus 50 more, then the
        // last line is a sentinel "TRAILING_ERROR". Tail-bound
        // output should keep the last MAX_TOOL_OUTPUT_LINES lines
        // and drop the head (lines 0..50). The sentinel should
        // survive.
        let extra: usize = 50;
        let total_lines: usize = crate::tools::output_bounds::MAX_TOOL_OUTPUT_LINES + extra;
        let shell_cmd = format!(
            "i=0; while [ $i -lt {total_lines} ]; do echo \"line $i\"; i=$((i+1)); done; echo TRAILING_ERROR"
        );

        // Build a runner that uses real `scripted` provider
        // (capture sink) and the real shell tool via the
        // `run_with_structured_tool_call` helper adapted to
        // produce a shell call.
        let tool_call = LlmEvent::ToolCall {
            id: "call_shell_1".to_string(),
            name: "shell".to_string(),
            input: serde_json::json!({ "command": shell_cmd }),
            provider_executed: Some(false),
        };

        // Two-turn script: turn 1 emits the shell call + finish;
        // turn 2 (after the tool result is in) is a brief
        // acknowledgment + stop.
        let turn1 = vec![
            tool_call,
            LlmEvent::Finish {
                reason: FinishReason::ToolCalls,
                usage: None,
            },
        ];
        let turn2 = vec![
            LlmEvent::TextDelta {
                id: "text-0".to_string(),
                text: "saw the trailing error.".to_string(),
            },
            LlmEvent::Finish {
                reason: FinishReason::Stop,
                usage: None,
            },
        ];

        let (runner, _tmp, session_id) =
            run_with_scripted("scripted", vec![turn1, turn2], "/tmp").await;
        // Plan mode is the runner default; it would deny shell.
        // Build mode allows it.
        let runner = runner.with_mode(crate::permission::PermissionMode::Build);

        let result = runner.run(&session_id).await;
        assert!(result.is_ok(), "run failed: {:?}", result.err());

        // Find the assistant message containing the shell call's
        // tool result. The runner dispatches shell through the
        // real tool, then bounds the result via
        // `tail_bound_output` (because tool name == "shell").
        let msgs = runner
            .conversation_service
            .get_messages(&session_id)
            .expect("get_messages");
        let assistant_with_result = msgs
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
                                .any(|p| matches!(p, ContentPart::ToolResult { .. }))
                        })
                        .unwrap_or(false)
            })
            .expect("non-empty assistant message containing a ToolResult");
        let parts: Vec<ContentPart> =
            serde_json::from_str(&assistant_with_result.content).expect("parts");

        let result_parts: Vec<&ContentPart> = parts
            .iter()
            .filter(|p| matches!(p, ContentPart::ToolResult { .. }))
            .collect();
        assert_eq!(
            result_parts.len(),
            1,
            "expected one ToolResult, got: {parts:#?}"
        );

        // The bounded text should:
        //  1. NOT contain the first 50 dropped lines.
        //  2. Contain the trailing sentinel (proves tail-style).
        //  3. End with the truncation marker.
        let result_json = serde_json::to_string(result_parts[0]).unwrap();
        assert!(
            !result_json.contains("line 0\n") && !result_json.contains("line 49\n"),
            "head of shell output should be tail-truncated, but the bounded result still contains early lines: {result_json}"
        );
        assert!(
            result_json.contains("TRAILING_ERROR"),
            "trailing sentinel should survive tail-bound truncation, got: {result_json}"
        );
        assert!(
            result_json.contains("output truncated"),
            "tail-bound result should include the truncation marker, got: {result_json}"
        );
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
