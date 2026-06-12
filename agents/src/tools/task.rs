//! Subagent tool: spawn a child session, run it, return its final text.
//!
//! Behaviour:
//!
//! - **Foreground (default)**: the parent's `SessionRunner` is blocked on
//!   this call until the child finishes. The child's final assistant text
//!   is returned as the tool result. The child session is persisted with
//!   `parent_session_id = caller` so the canvas can show it as a node
//!   under the parent.
//! - **Background (`background: true`)**: the child is spawned on a
//!   detached `tokio` task and this tool returns immediately with a
//!   `<task id=… state="running">` envelope. When the child finishes, its
//!   result is drained into the parent's next LLM turn by
//!   `SubagentRegistry::take_completions`.
//!
//! Loop control: the registry enforces (a) a depth cap (sub-agents of
//! sub-agents are forbidden) and (b) ancestor-cycle prevention. We
//! surface the deny reason in the failure path so the LLM can recover.

use std::sync::Arc;

use crate::llm::session::SessionRunner;
use crate::llm::{ChildCompletion, ChildKind, ProviderRegistry};
use crate::tools::{string_arg, ToolRuntime};

use super::{failure, not_implemented, success, ToolCall, ToolResult};

const DEFAULT_FOREGROUND_MAX_TURNS: u32 = 5;

pub fn run(call: &ToolCall) -> ToolResult {
    let _ = call;
    not_implemented(
        "task",
        "requires the agent runner's async dispatch; the runner routes `task` to `run_tool_async`",
    )
}

pub async fn run_async(call: &ToolCall, runtime: ToolRuntime) -> ToolResult {
    let description = string_arg(call, "description");
    let prompt = string_arg(call, "prompt");
    let subagent_type = string_arg(call, "subagent_type");
    let background = call
        .arguments
        .get("background")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if prompt.is_empty() {
        return failure("task", "prompt is required".to_string());
    }
    if subagent_type.is_empty() {
        return failure(
            "task",
            "subagent_type is required (e.g. \"general\", \"explore\", \"build\")".to_string(),
        );
    }

    // Look up the caller. The agent runner guarantees this exists.
    let caller = match runtime
        .session_service
        .get_session(&runtime.caller_session_id)
    {
        Ok(Some(s)) => s,
        Ok(None) => {
            return failure(
                "task",
                format!("caller session not found: {}", runtime.caller_session_id),
            );
        }
        Err(e) => return failure("task", format!("session lookup failed: {e}")),
    };

    // Loop control. The depth cap and ancestor cycle are checked here
    // before we even create a child row.
    if let Err(deny) = runtime.subagent_registry.check_spawn(&caller.id, None) {
        let msg = match deny {
            crate::llm::DenyReason::DepthExceeded => {
                "sub-agents cannot spawn sub-agents (depth cap exceeded)".to_string()
            }
            crate::llm::DenyReason::AncestorCycle => {
                "cannot target an ancestor session (cycle prevented)".to_string()
            }
            crate::llm::DenyReason::SelfTarget => "cannot target the caller itself".to_string(),
        };
        return failure("task", msg);
    }

    // Build a child session. The child inherits the caller's project,
    // provider, and model. The directory is the caller's directory by
    // default; `ask_peer` overrides this when it wants to use a peer's.
    let child_id = match runtime.session_service.create_session_with_parent(
        caller.project_id.clone(),
        caller.directory.clone(),
        caller.provider.clone(),
        caller.model.clone(),
        Some(caller.id.clone()),
    ) {
        Ok(id) => id,
        Err(e) => return failure("task", format!("failed to create child session: {e}")),
    };
    runtime
        .subagent_registry
        .register_child(&caller.id, &child_id);

    let envelope_open = format!(
        "<task id=\"{child_id}\" state=\"running\">\n<summary>{description}: spawned by caller {caller_id}</summary>\n",
        description = if description.is_empty() { "subagent".to_string() } else { description.clone() },
        caller_id = caller.id,
    );

    if background {
        spawn_background(caller.id.clone(), child_id.clone(), prompt, runtime);
        return success(
            "task",
            format!(
                "{envelope_open}<task_result>Background subagent started. You will see its result in the next turn.</task_result>\n</task>"
            ),
        );
    }

    // Foreground: run a bounded SessionRunner for the child, then return
    // its final assistant text. The runner uses a one-shot runtime
    // (Arc<SubagentRegistry> + a fresh ProviderRegistry copy) so the
    // child's tools are restricted: no `task` and no `ask_peer`.
    let registry = runtime.subagent_registry.clone();
    let parent_id = caller.id.clone();
    let child_prompt = prompt.clone();

    let outcome = run_child_foreground(child_id.clone(), child_prompt, runtime).await;

    let completion = ChildCompletion {
        child_session_id: child_id.clone(),
        kind: ChildKind::Task,
        text: outcome.text.clone(),
        success: outcome.success,
    };
    registry.push_completion(&parent_id, completion);

    let body = format!(
        "{envelope_open}<task_result>{}</task_result>\n</task>",
        escape_for_envelope(&outcome.text)
    );
    success_or_failure("task", outcome.success, body, outcome.error)
}

fn spawn_background(parent_id: String, child_id: String, prompt: String, runtime: ToolRuntime) {
    let registry = Arc::clone(&runtime.subagent_registry);
    tokio::spawn(async move {
        let outcome = run_child_foreground(child_id.clone(), prompt, runtime).await;
        registry.push_completion(
            &parent_id,
            ChildCompletion {
                child_session_id: child_id,
                kind: ChildKind::Task,
                text: outcome.text,
                success: outcome.success,
            },
        );
    });
}

struct ChildOutcome {
    success: bool,
    text: String,
    error: Option<String>,
}

async fn run_child_foreground(
    child_id: String,
    prompt: String,
    runtime: ToolRuntime,
) -> ChildOutcome {
    // 1. Append the parent's question as a synthetic user message on the
    //    child's conversation. This is what the child "sees" as the
    //    starting point.
    let prompt_id = match runtime.conversation_service.append_message(
        &child_id,
        crate::MessageRole::User,
        prompt,
    ) {
        Ok(id) => id,
        Err(e) => {
            return ChildOutcome {
                success: false,
                text: String::new(),
                error: Some(format!("failed to seed child conversation: {e}")),
            };
        }
    };
    let _ = prompt_id;

    // 2. Build a SessionRunner with a fresh ProviderRegistry copy so the
    //    child has the same providers as the parent. We do not propagate
    //    the caller's event sink — the child's events are not surfaced
    //    to the UI as primary-session activity.
    let runner = match runtime.session_service.get_session(&child_id) {
        Ok(Some(_child)) => SessionRunner::new(
            Arc::clone(&runtime.session_service),
            Arc::clone(&runtime.conversation_service),
            child_provider_registry(&runtime),
        )
        .with_max_turns(DEFAULT_FOREGROUND_MAX_TURNS),
        _ => {
            return ChildOutcome {
                success: false,
                text: String::new(),
                error: Some("child session disappeared before run".to_string()),
            };
        }
    };

    match Box::pin(runner.run(&child_id)).await {
        Ok(_result) => {
            // Extract the last assistant text from the child's
            // conversation.
            let text =
                last_assistant_text(&runtime.conversation_service, &child_id).unwrap_or_default();
            ChildOutcome {
                success: true,
                text,
                error: None,
            }
        }
        Err(e) => ChildOutcome {
            success: false,
            text: format!("child session failed: {e}"),
            error: Some(e.to_string()),
        },
    }
}

fn child_provider_registry(_runtime: &ToolRuntime) -> Arc<ProviderRegistry> {
    Arc::clone(&_runtime.providers)
}

fn last_assistant_text(
    conversation_service: &crate::ConversationService,
    session_id: &str,
) -> Result<String, String> {
    let messages = conversation_service.get_messages(session_id)?;
    Ok(messages
        .iter()
        .rev()
        .find(|m| m.role == "assistant")
        .map(|m| m.content.clone())
        .unwrap_or_default())
}

fn escape_for_envelope(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn success_or_failure(tool: &str, ok: bool, output: String, error: Option<String>) -> ToolResult {
    if ok {
        success(tool, output)
    } else {
        ToolResult {
            tool: tool.to_string(),
            success: false,
            output,
            error,
        }
    }
}
