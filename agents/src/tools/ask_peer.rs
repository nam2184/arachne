//! `ask_peer`: read-only cross-repo question routing.
//!
//! The LLM of session A invokes this tool with a `peer_session_id` and a
//! `question`. We:
//!
//! 1. Verify the peer exists and that the canonicalized directory of the
//!    caller is different from the canonicalized directory of the peer
//!    (per spec: "different filepath, that is all").
//! 2. Spawn a child session in the peer's directory with
//!    `parent_session_id = caller`. The child is read-only — its tool
//!    set is restricted to `read`, `glob`, `grep`, `webfetch`,
//!    `websearch` (the same set `ask_peer` itself would let the peer
//!    use; we explicitly exclude `write`, `edit`, `apply_patch`,
//!    `shell`, `task`, `ask_peer`).
//! 3. Seed the child conversation with a synthetic user message that
//!    frames the question.
//! 4. Run the child, return its final text as the tool result.
//!
//! Like the `task` tool, the child is persisted as a normal
//! `AgentSession` row so the canvas can render it under the caller.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::llm::session::SessionRunner;
use crate::llm::{ChildCompletion, ChildKind, ProviderRegistry};
use crate::tools::{string_arg, ToolRuntime};

use super::{failure, success, ToolCall, ToolResult};

const DEFAULT_PEER_MAX_TURNS: u32 = 3;

pub fn run(_call: &ToolCall) -> ToolResult {
    failure(
        "ask_peer",
        "ask_peer requires the async runtime; the agent runner routes this tool to `run_tool_async`".to_string(),
    )
}

pub async fn run_async(call: &ToolCall, runtime: ToolRuntime) -> ToolResult {
    let peer_id = string_arg(call, "peer_session_id");
    let question = string_arg(call, "question");
    let max_turns = call
        .arguments
        .get("max_turns")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .unwrap_or(DEFAULT_PEER_MAX_TURNS)
        .clamp(1, 5);

    if peer_id.is_empty() {
        return failure("ask_peer", "peer_session_id is required".to_string());
    }
    if question.is_empty() {
        return failure("ask_peer", "question is required".to_string());
    }

    let caller = match runtime
        .session_service
        .get_session(&runtime.caller_session_id)
    {
        Ok(Some(s)) => s,
        Ok(None) => {
            return failure(
                "ask_peer",
                format!("caller session not found: {}", runtime.caller_session_id),
            );
        }
        Err(e) => return failure("ask_peer", format!("session lookup failed: {e}")),
    };

    let peer = match runtime.session_service.get_session(&peer_id) {
        Ok(Some(s)) => s,
        Ok(None) => return failure("ask_peer", format!("peer session not found: {peer_id}")),
        Err(e) => return failure("ask_peer", format!("peer lookup failed: {e}")),
    };

    // 1. Different-filepath gate.
    let caller_dir = canonicalize_lossy(&caller.directory);
    let peer_dir = canonicalize_lossy(&peer.directory);
    if caller_dir == peer_dir {
        return failure(
            "ask_peer",
            "ask_peer denied: caller and peer share the same directory; cross-repo calls only"
                .to_string(),
        );
    }

    // 2. Depth cap + ancestor cycle (a peer should not be an ancestor).
    if let Err(deny) = runtime
        .subagent_registry
        .check_spawn(&caller.id, Some(&peer.id))
    {
        let msg = match deny {
            crate::llm::DenyReason::DepthExceeded => {
                "sub-agents cannot ask peers (depth cap exceeded)".to_string()
            }
            crate::llm::DenyReason::AncestorCycle => {
                "cannot ask an ancestor session (cycle prevented)".to_string()
            }
            crate::llm::DenyReason::SelfTarget => "cannot ask the caller itself".to_string(),
        };
        return failure("ask_peer", msg);
    }

    // 3. Spawn the child. The child is rooted at the peer's directory and
    //    inherits the peer's provider/model.
    let child_id = match runtime.session_service.create_session_with_parent(
        caller.project_id.clone(),
        peer.directory.clone(),
        peer.provider.clone(),
        peer.model.clone(),
        Some(caller.id.clone()),
    ) {
        Ok(id) => id,
        Err(e) => return failure("ask_peer", format!("failed to create child session: {e}")),
    };
    runtime
        .subagent_registry
        .register_child(&caller.id, &child_id);

    // 4. Seed the child conversation with a synthetic user message
    //    framing the peer's role and the question.
    let synthetic = format!(
        "[A peer agent from a different working directory is asking you about \
         your patterns. You may only read your own files; write/edit/shell \
         tools are disabled for this turn. Answer concisely.]\n\n\
         Peer question: {question}"
    );
    if let Err(e) =
        runtime
            .conversation_service
            .append_message(&child_id, crate::MessageRole::User, synthetic)
    {
        return failure(
            "ask_peer",
            format!("failed to seed child conversation: {e}"),
        );
    }

    // 5. Run the child with a read-only tool set.
    let registry = runtime.subagent_registry.clone();
    let parent_id = caller.id.clone();
    let outcome = run_child_readonly(child_id.clone(), runtime, max_turns).await;

    registry.push_completion(
        &parent_id,
        ChildCompletion {
            child_session_id: child_id.clone(),
            kind: ChildKind::AskPeer,
            text: outcome.text.clone(),
            success: outcome.success,
        },
    );

    if outcome.success {
        success("ask_peer", outcome.text)
    } else {
        ToolResult {
            tool: "ask_peer".to_string(),
            success: false,
            output: outcome.text,
            error: outcome.error,
        }
    }
}

struct ChildOutcome {
    success: bool,
    text: String,
    error: Option<String>,
}

async fn run_child_readonly(
    child_id: String,
    runtime: ToolRuntime,
    max_turns: u32,
) -> ChildOutcome {
    let runner = match runtime.session_service.get_session(&child_id) {
        Ok(Some(_child)) => SessionRunner::new(
            Arc::clone(&runtime.session_service),
            Arc::clone(&runtime.conversation_service),
            child_provider_registry(&runtime),
        )
        // The readonly tool set is enforced via the `TaskKind::AskPeer`
        // flag in the runner. (We pass a hint that says "this is a
        // read-only invocation".)
        .with_readonly_tools(true)
        .with_max_turns(max_turns),
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
            text: format!("peer query failed: {e}"),
            error: Some(e.to_string()),
        },
    }
}

fn child_provider_registry(runtime: &ToolRuntime) -> Arc<ProviderRegistry> {
    Arc::clone(&runtime.providers)
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

fn canonicalize_lossy(path: &str) -> PathBuf {
    let p = Path::new(path);
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalize_lossy_returns_path_even_when_missing() {
        let path = canonicalize_lossy("/nonexistent/path/that/does/not/exist");
        assert_eq!(path, Path::new("/nonexistent/path/that/does/not/exist"));
    }

    #[test]
    fn canonicalize_lossy_resolves_existing_path() {
        let dir = tempfile::tempdir().unwrap();
        let canon = canonicalize_lossy(dir.path().to_str().unwrap());
        // The canonicalized path should still point inside the temp dir.
        assert!(canon.starts_with(dir.path().canonicalize().unwrap().parent().unwrap()));
    }
}
