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

use chrono::Utc;

use crate::database::{Database, ProjectRepository, SessionGroupRepository, SessionRepository};
use crate::llm::session::{SessionCancelToken, SessionRunner};
use crate::llm::{ChildCompletion, ChildKind, ProviderRegistry};
use crate::permission_v2::{default_ruleset, PermissionService};
use crate::sandbox::SandboxPolicy;
use crate::tools::{string_arg, SandboxedContext, ToolRuntime};
use crate::{AgentSession, ConversationService, Project, SessionGroup, SessionService};

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

    let child_id = format!("task-{}", uuid::Uuid::new_v4());
    runtime
        .subagent_registry
        .register_child(&caller.id, &child_id);

    let envelope_open = format!(
        "<task id=\"{child_id}\" state=\"running\">\n<summary>{description}: spawned by caller {caller_id}</summary>\n",
        description = if description.is_empty() { "subagent".to_string() } else { description.clone() },
        caller_id = caller.id,
    );

    if background {
        spawn_background(caller.clone(), child_id.clone(), prompt, runtime);
        return success(
            "task",
            format!(
                "{envelope_open}<task_result>Background subagent started. You will see its result in the next turn.</task_result>\n</task>"
            ),
        );
    }

    // Foreground: run a bounded SessionRunner for the child, then return
    // its final assistant text. The runner uses a one-shot runtime
    // (Arc<SubagentRegistry> + a fresh ProviderRegistry copy) so the child
    // stays bounded.
    let registry = runtime.subagent_registry.clone();
    let parent_id = caller.id.clone();
    let child_prompt = prompt.clone();

    let outcome = run_child_foreground(child_id.clone(), child_prompt, runtime, caller).await;

    let completion = ChildCompletion {
        child_session_id: child_id.clone(),
        kind: ChildKind::Task,
        text: outcome.text.clone(),
        success: outcome.success,
    };
    registry.push_completion(&parent_id, completion);
    registry.complete_child(&parent_id, &child_id);

    let body = format!(
        "{envelope_open}<task_result>{}</task_result>\n</task>",
        escape_for_envelope(&outcome.text)
    );
    success_or_failure("task", outcome.success, body, outcome.error)
}

fn spawn_background(parent: AgentSession, child_id: String, prompt: String, runtime: ToolRuntime) {
    let parent_id = parent.id.clone();
    let registry = Arc::clone(&runtime.subagent_registry);
    tokio::spawn(async move {
        let outcome = run_child_foreground(child_id.clone(), prompt, runtime, parent).await;
        registry.push_completion(
            &parent_id,
            ChildCompletion {
                child_session_id: child_id.clone(),
                kind: ChildKind::Task,
                text: outcome.text,
                success: outcome.success,
            },
        );
        registry.complete_child(&parent_id, &child_id);
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
    parent: AgentSession,
) -> ChildOutcome {
    let ephemeral = match prepare_ephemeral_child_runtime(&child_id, &parent, &runtime) {
        Ok(runtime) => runtime,
        Err(error) => {
            return ChildOutcome {
                success: false,
                text: String::new(),
                error: Some(error),
            };
        }
    };

    // 1. Append the parent's question as a synthetic user message on the
    //    child's conversation. This is what the child "sees" as the
    //    starting point.
    let prompt_id = match ephemeral.runtime.conversation_service.append_message(
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
    let runner = match ephemeral.runtime.session_service.get_session(&child_id) {
        Ok(Some(child)) => {
            let (permissions, _rx) = PermissionService::new(child.id.clone(), default_ruleset());
            let sandbox = SandboxPolicy::new(std::path::PathBuf::from(&child.directory));
            let sandboxed_ctx = Arc::new(
                SandboxedContext::new(sandbox, Arc::clone(&permissions)).with_caller_session(
                    child.id.clone(),
                    Arc::clone(&ephemeral.runtime.session_service),
                ),
            );
            SessionRunner::new(
                Arc::clone(&ephemeral.runtime.session_service),
                Arc::clone(&ephemeral.runtime.conversation_service),
                child_provider_registry(&ephemeral.runtime),
            )
            .with_permissions(permissions)
            .with_sandboxed_context(sandboxed_ctx)
            .with_subagent_registry(Arc::clone(&ephemeral.runtime.subagent_registry))
            .with_max_turns(DEFAULT_FOREGROUND_MAX_TURNS)
        }
        _ => {
            return ChildOutcome {
                success: false,
                text: String::new(),
                error: Some("child session disappeared before run".to_string()),
            };
        }
    };

    let cancellation = SessionCancelToken::new();
    ephemeral
        .runtime
        .subagent_registry
        .register_cancellation(&child_id, cancellation.clone());
    let result = Box::pin(runner.with_cancellation(cancellation).run(&child_id)).await;
    ephemeral
        .runtime
        .subagent_registry
        .unregister_cancellation(&child_id);

    match result {
        Ok(_result) => {
            // Extract the last assistant text from the child's
            // conversation.
            let text = last_assistant_text(&ephemeral.runtime.conversation_service, &child_id)
                .unwrap_or_default();
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

struct EphemeralChildRuntime {
    runtime: ToolRuntime,
    _db_dir: tempfile::TempDir,
    _conversation_dir: tempfile::TempDir,
}

fn prepare_ephemeral_child_runtime(
    child_id: &str,
    parent: &AgentSession,
    runtime: &ToolRuntime,
) -> Result<EphemeralChildRuntime, String> {
    let db_dir = tempfile::tempdir().map_err(|e| format!("temp db dir: {e}"))?;
    let conversation_dir =
        tempfile::tempdir().map_err(|e| format!("temp conversation dir: {e}"))?;
    let db_path = db_dir.path().join("task.sqlite");
    let db = Database::new(db_path.clone()).map_err(|e| e.to_string())?;
    db.init()?;
    let root = runtime
        .session_service
        .root_session(&parent.id)
        .unwrap_or_else(|_| parent.clone());
    seed_child_routing_context(&db, &root, runtime)?;

    let mut child = AgentSession::child_of(
        &root,
        parent.directory.clone(),
        parent.provider.clone(),
        parent.model.clone(),
    );
    child.id = child_id.to_string();
    SessionRepository::insert(&db, &child)?;
    drop(db);

    let session_service = SessionService::new(db_path);
    let conversation_service = ConversationService::new(conversation_dir.path().to_path_buf());
    conversation_service.create_conversation(child_id)?;

    Ok(EphemeralChildRuntime {
        runtime: ToolRuntime {
            caller_session_id: child_id.to_string(),
            session_service,
            conversation_service,
            providers: Arc::clone(&runtime.providers),
            subagent_registry: Arc::clone(&runtime.subagent_registry),
            mode: runtime.mode,
            turn_id: runtime.turn_id,
            runtime_config: runtime.runtime_config.clone(),
            mcp_manager: Arc::clone(&runtime.mcp_manager),
            project_root: runtime.project_root.clone(),
        },
        _db_dir: db_dir,
        _conversation_dir: conversation_dir,
    })
}

fn seed_child_routing_context(
    db: &Database,
    root: &AgentSession,
    runtime: &ToolRuntime,
) -> Result<(), String> {
    insert_project_for_session(db, root)?;
    SessionRepository::insert(db, root)?;

    let Some(group_id) = root.group_id.as_deref() else {
        return Ok(());
    };

    let mut session_ids = Vec::new();
    for session in runtime.session_service.sessions_in_group(group_id)? {
        insert_project_for_session(db, &session)?;
        if session.id != root.id {
            SessionRepository::insert(db, &session)?;
        }
        session_ids.push(session.id);
    }

    if !session_ids.iter().any(|id| id == &root.id) {
        session_ids.push(root.id.clone());
    }

    SessionGroupRepository::insert(
        db,
        &SessionGroup {
            id: group_id.to_string(),
            name: None,
            session_ids,
            created_at: Utc::now(),
        },
    )
}

fn insert_project_for_session(db: &Database, session: &AgentSession) -> Result<(), String> {
    ProjectRepository::insert(
        db,
        &Project {
            id: session.project_id.clone(),
            path: session.directory.clone(),
            name: session.project_id.clone(),
            tech_stack: vec![],
            created_at: Utc::now(),
        },
    )
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
            metadata: None,
        }
    }
}
