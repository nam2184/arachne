pub mod apply_patch;
pub mod edit;
pub mod external_directory;
pub mod glob;
pub mod grep;
pub mod invalid;
pub mod lsp;
pub mod output_bounds;
pub mod plan;
pub mod question;
pub mod read;
pub mod shell;
pub mod skill;
pub mod task;
pub mod todo;
pub mod webfetch;
pub mod websearch;
pub mod write;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::llm::SubagentRegistry;
use crate::permission::{PermissionMode, PermissionService};
use crate::permission_v2::PermissionService as V2PermissionService;
use crate::sandbox::{
    trigger_access, AccessDecision, AccessRequest, DoomLoopDetector, NetworkPolicy, SandboxPolicy,
    ShellExit, ShellPolicy,
};
use crate::{ToolCall, ToolResult};

pub use output_bounds::{
    bound_tool_output, estimate_tokens, BoundedOutput, CHARS_PER_TOKEN, GLOB_DEFAULT_LIMIT,
    GREP_DEFAULT_LIMIT, MAX_TOOL_OUTPUT_BYTES, MAX_TOOL_OUTPUT_LINES, READ_DEFAULT_LIMIT,
};

/// Resolve a path argument against the session's `project_root`.
///
/// File-system tools (`read`, `write`, `edit`, `apply_patch`, etc.)
/// historically passed the LLM's `path` argument directly to
/// `std::fs::*` or `FileMutationService`. That leaks the Tauri
/// process's working directory into the tool's view of the
/// world, so a relative `path: "src/lib.rs"` from the LLM
/// silently opened `/path/to/tauri/install/src/lib.rs` instead
/// of the session's project. This helper anchors relative
/// paths to `ctx.project_root` and emits a single tracing line
/// per call so the cwd vs. project_root split is observable.
///
/// Absolute paths and `~`-prefixed paths are passed through
/// unchanged. The sandbox (path containment) is the v2 path's
/// job, not this helper's.
pub fn resolve_session_path(requested: &str, ctx: &ToolContext, tool: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(requested);
    let absolute = path.is_absolute();
    let resolved = if absolute {
        path.to_path_buf()
    } else if !ctx.project_root.as_os_str().is_empty() {
        ctx.project_root.join(path)
    } else {
        // Last-resort fallback. With no project_root, relative
        // paths resolve against the process CWD, which is
        // almost always wrong for a Tauri app — log it so the
        // operator can spot the missing wiring.
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let process_cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    tracing::info!(
        tool,
        requested = %requested,
        absolute,
        project_root = %ctx.project_root.display(),
        resolved = %resolved.display(),
        process_cwd = %process_cwd,
        inside_project_root = project_root_contains(&ctx.project_root, &resolved),
        "tool path resolution"
    );
    resolved
}

fn project_root_contains(root: &std::path::Path, candidate: &std::path::Path) -> Option<bool> {
    if root.as_os_str().is_empty() {
        return None;
    }
    Some(crate::sandbox::path::contains_path(root, candidate))
}

async fn resolve_sandbox_path(
    requested: &str,
    ctx: &SandboxedContext,
    tool: &str,
) -> Result<std::path::PathBuf, String> {
    let process_cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    // Fast path: try the policy without an ask.
    if let Ok(resolved) = ctx.sandbox.lock().resolve(requested) {
        tracing::info!(
            tool,
            requested = %requested,
            project_root = %ctx.sandbox.lock().project_root.display(),
            resolved = %resolved.display(),
            process_cwd = %process_cwd,
            inside_project_root = project_root_contains(
                &ctx.sandbox.lock().project_root,
                &resolved
            ),
            sandbox_allowed = true,
            "sandbox path resolution allowed"
        );
        return Ok(resolved);
    }

    // The path is outside the sandbox. If the failure isn't a
    // simple "outside project root" case (e.g. symlink escape,
    // empty path), don't ask — surface the structural error.
    let initial_error = ctx.sandbox.lock().resolve(requested).unwrap_err();
    if !crate::sandbox::should_trigger_ask(&initial_error) {
        tracing::warn!(
            tool,
            requested = %requested,
            project_root = %ctx.sandbox.lock().project_root.display(),
            process_cwd = %process_cwd,
            sandbox_allowed = false,
            error = %initial_error,
            "sandbox path resolution rejected (structural error, no ask)"
        );
        return Err(format!("{initial_error}"));
    }

    // Outside project root: ask the user via the v2 permission
    // service. On approval, `trigger_access` promotes the parent
    // directory (or the directory itself, when the target is a
    // directory) into `external_roots` and re-resolves.
    match trigger_access(
        &ctx.sandbox,
        &ctx.permissions,
        AccessRequest { requested, tool },
    )
    .await
    {
        Ok(AccessDecision::Allowed { path, .. }) => {
            tracing::info!(
                tool,
                requested = %requested,
                project_root = %ctx.sandbox.lock().project_root.display(),
                resolved = %path.display(),
                process_cwd = %process_cwd,
                sandbox_allowed = true,
                "sandbox path resolution allowed (after ask)"
            );
            Ok(path)
        }
        Ok(AccessDecision::Rejected { message }) => {
            tracing::warn!(
                tool,
                requested = %requested,
                project_root = %ctx.sandbox.lock().project_root.display(),
                process_cwd = %process_cwd,
                sandbox_allowed = false,
                "sandbox access rejected by user"
            );
            Err(message)
        }
        Err(error) => Err(error),
    }
}

/// Runtime context passed to every tool invocation that needs to spawn
/// sub-sessions or write to the parent's conversation. Held behind an
/// `Arc` so it's cheap to clone into `tokio::task::spawn`.
#[derive(Clone)]
pub struct ToolRuntime {
    pub caller_session_id: String,
    pub session_service: Arc<crate::SessionService>,
    pub conversation_service: Arc<crate::ConversationService>,
    pub providers: Arc<crate::llm::ProviderRegistry>,
    pub subagent_registry: Arc<SubagentRegistry>,
    pub mode: PermissionMode,
    pub turn_id: u64,
    /// Project root for the caller's session. Propagated into the
    /// `ToolContext` used for non-task tools so file-system tools
    /// (read, glob, grep, …) resolve relative paths against the
    /// correct directory even when dispatched through the async
    /// path. Empty when unknown.
    pub project_root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ToolContext {
    pub mode: PermissionMode,
    /// Project root for the active session. Tools that take a `path`
    /// argument resolve relative paths against this root instead of
    /// the process CWD. An empty path means "no project root known"
    /// (falls back to the legacy behavior).
    pub project_root: PathBuf,
}

impl std::fmt::Display for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ToolContext(mode={:?}, project_root={})",
            self.mode,
            if self.project_root.as_os_str().is_empty() {
                "<unset>".to_string()
            } else {
                self.project_root.display().to_string()
            }
        )
    }
}

impl ToolContext {
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            mode,
            project_root: PathBuf::new(),
        }
    }

    pub fn with_project_root(mut self, root: PathBuf) -> Self {
        self.project_root = root;
        self
    }
}

impl Default for ToolContext {
    fn default() -> Self {
        Self::new(PermissionMode::Build)
    }
}

/// Bundled context for sandboxed tool execution. Created per-session.
/// Tools receive this and use its policies to gate their behavior.
///
/// The `sandbox` field is wrapped in `Arc<Mutex<_>>` so the policy's
/// `external_roots` can grow at runtime — specifically, `trigger_access`
/// promotes an approved out-of-root directory into `external_roots` so
/// the same access succeeds on the next call without re-asking.
#[derive(Clone)]
pub struct SandboxedContext {
    pub sandbox: Arc<parking_lot::Mutex<SandboxPolicy>>,
    pub shell_policy: ShellPolicy,
    pub network_policy: NetworkPolicy,
    pub permissions: Arc<V2PermissionService>,
    pub doom: Arc<DoomLoopDetector>,
}

impl SandboxedContext {
    pub fn new(sandbox: SandboxPolicy, permissions: Arc<V2PermissionService>) -> Self {
        let cwd = sandbox.project_root.clone();
        Self {
            sandbox: Arc::new(parking_lot::Mutex::new(sandbox)),
            shell_policy: ShellPolicy::new(cwd).with_timeout(Duration::from_secs(120)),
            network_policy: NetworkPolicy::new(),
            permissions,
            doom: Arc::new(DoomLoopDetector::default()),
        }
    }

    pub fn with_shell_timeout(mut self, timeout: Duration) -> Self {
        self.shell_policy = self.shell_policy.with_timeout(timeout);
        self
    }

    pub fn with_external_root(self, path: PathBuf) -> Self {
        self.sandbox.lock().external_roots.push(path);
        self
    }

    /// Snapshot the project root out of the locked policy.
    pub fn project_root(&self) -> PathBuf {
        self.sandbox.lock().project_root.clone()
    }
}

pub fn run_tool(call: &ToolCall) -> ToolResult {
    run_tool_with_context(call, &ToolContext::default())
}

pub fn run_tool_with_mode(call: &ToolCall, mode: PermissionMode) -> ToolResult {
    run_tool_with_context(call, &ToolContext::new(mode))
}

pub fn run_tool_with_context(call: &ToolCall, context: &ToolContext) -> ToolResult {
    if has_peer_session_id(call) {
        return failure(
            &call.name,
            "peer_session_id requires the agent runner async dispatch".to_string(),
        );
    }
    if let Err(error) = PermissionService::new(context.mode).assert_tool_call(call) {
        return failure(&call.name, error.to_string());
    }

    match call.name.as_str() {
        "read" | "read_file" => read::run_with_context(call, context),
        "write" | "write_file" => write::run_with_context(call, context),
        "edit" => edit::run_with_context(call, context),
        "apply_patch" => apply_patch::run_with_context(call, context),
        "glob" | "search_files" => glob::run_with_context(call, context),
        "grep" => grep::run_with_context(call, context),
        "shell" | "bash" => shell::run_with_context(call, context),
        "task" => task::run(call),
        "skill" => skill::run(call),
        "todo" | "todowrite" => todo::run(call),
        "question" => question::run(call),
        "webfetch" => webfetch::run(call),
        "websearch" => websearch::run(call),
        "lsp" => lsp::run(call),
        "plan" => plan::run(call),
        "external_directory" => external_directory::run(call),
        "invalid" => invalid::run(call),
        _ => invalid::unknown(call),
    }
}

/// Async tool dispatch for tools that need the `ToolRuntime` (sub-sessions,
/// etc.) or that need to await async I/O (HTTP). Falls back to the
/// sync path for everything else. The agent runner uses this for
/// any tool whose name is `task`, `webfetch`, or `websearch`, plus read-only tools carrying
/// `peer_session_id`.
pub async fn run_tool_async(call: &ToolCall, runtime: &ToolRuntime) -> ToolResult {
    if let Some(result) = dispatch_peer_tool_if_requested(call, runtime, None).await {
        return result;
    }

    if let Err(error) = PermissionService::new(PermissionMode::Build).assert_tool_call(call) {
        return failure(&call.name, error.to_string());
    }

    match call.name.as_str() {
        "task" => task::run_async(call, runtime.clone()).await,
        // Network tools perform real HTTP requests. Doing them on the
        // async path keeps the executor unblocked. It doesn't need
        // the `ToolRuntime` (no sub-sessions, no message bus).
        "webfetch" => webfetch::run_async(call).await,
        "websearch" => websearch::run_async(call).await,
        // Everything else: defer to the sync path. The caller is already
        // running in an async context, but the underlying tool is sync.
        // Thread the caller's project_root through so file-system
        // tools resolve paths against the right directory.
        other => {
            let _ = other;
            let ctx = ToolContext::new(PermissionMode::Build)
                .with_project_root(runtime.project_root.clone());
            run_tool_with_context(call, &ctx)
        }
    }
}

/// Async sandboxed dispatch. Keeps `task` and peer-targeted tools on the async runtime
/// while all ordinary tools use the same v2 sandbox path as sync dispatch.
pub async fn run_tool_async_sandboxed(
    call: &ToolCall,
    runtime: &ToolRuntime,
    ctx: &SandboxedContext,
) -> ToolResult {
    if let Some(result) = dispatch_peer_tool_if_requested(call, runtime, Some(ctx)).await {
        return result;
    }

    match call.name.as_str() {
        "task" => task::run_async(call, runtime.clone()).await,
        "webfetch" => webfetch_async_sandboxed(call, ctx).await,
        "websearch" => websearch_async_sandboxed(call, ctx).await,
        _ => run_tool_sandboxed(call, ctx).await,
    }
}

async fn dispatch_peer_tool_if_requested(
    call: &ToolCall,
    runtime: &ToolRuntime,
    sandboxed: Option<&SandboxedContext>,
) -> Option<ToolResult> {
    let peer_session_id = string_arg(call, "peer_session_id");
    if peer_session_id.trim().is_empty() {
        return None;
    }

    if !supports_peer_session_id(&call.name) {
        return Some(failure(
            &call.name,
            format!(
                "peer_session_id is only supported for read, glob, and grep plan-mode tool calls; omit peer_session_id for local work with {}",
                call.name
            ),
        ));
    }
    if runtime.mode != PermissionMode::Plan {
        return Some(failure(
            &call.name,
            "peer_session_id is only supported in plan mode for read-only context gathering"
                .to_string(),
        ));
    }

    let (peer_directory, subsession_id) = match resolve_peer_tool_target(runtime, &peer_session_id)
    {
        Ok(target) => target,
        Err(error) => return Some(failure(&call.name, error)),
    };

    tracing::info!(
        caller_session_id = %runtime.caller_session_id,
        peer_session_id = %peer_session_id,
        subsession_id = %subsession_id,
        tool = %call.name,
        peer_directory = %peer_directory.display(),
        "dispatching peer-targeted tool call through subsession"
    );

    let mut peer_call = call.clone();
    peer_call.arguments.remove("peer_session_id");

    let result = if let Some(parent_ctx) = sandboxed {
        let peer_ctx = SandboxedContext::new(
            SandboxPolicy::new(peer_directory),
            Arc::clone(&parent_ctx.permissions),
        );
        run_tool_sandboxed(&peer_call, &peer_ctx).await
    } else {
        let ctx = ToolContext::new(runtime.mode).with_project_root(peer_directory);
        run_tool_with_context(&peer_call, &ctx)
    };

    Some(result)
}

fn resolve_peer_tool_target(
    runtime: &ToolRuntime,
    peer_session_id: &str,
) -> Result<(PathBuf, String), String> {
    let caller = runtime
        .session_service
        .get_session(&runtime.caller_session_id)?
        .ok_or_else(|| format!("caller session not found: {}", runtime.caller_session_id))?;
    if peer_session_id == caller.id {
        return Err(
            "peer_session_id must refer to a different connected session; omit it for local work"
                .to_string(),
        );
    }

    crate::routing::integration::validate_connected_peer(
        &caller.id,
        peer_session_id,
        &runtime.session_service,
    )?;

    let peer = runtime
        .session_service
        .get_session(peer_session_id)?
        .ok_or_else(|| format!("peer session not found: {peer_session_id}"))?;

    if let Err(deny) = runtime
        .subagent_registry
        .check_spawn(&caller.id, Some(&peer.id))
    {
        let message = match deny {
            crate::llm::DenyReason::DepthExceeded => {
                "subsessions cannot spawn subsessions (depth cap exceeded)".to_string()
            }
            crate::llm::DenyReason::AncestorCycle => {
                "cannot target an ancestor session (cycle prevented)".to_string()
            }
            crate::llm::DenyReason::SelfTarget => {
                "peer_session_id must refer to a different connected session; omit it for local work"
                    .to_string()
            }
        };
        return Err(message);
    }

    let peer_directory = PathBuf::from(&peer.directory);
    let child_id = runtime.subagent_registry.get_or_create_peer_subsession(
        &caller.id,
        &peer.id,
        runtime.turn_id,
        || {
            runtime.session_service.create_session_with_parent(
                peer.project_id.clone(),
                peer.directory.clone(),
                caller.provider.clone(),
                caller.model.clone(),
                Some(caller.id.clone()),
            )
        },
    )?;

    Ok((peer_directory, child_id))
}

fn supports_peer_session_id(tool: &str) -> bool {
    matches!(
        tool,
        "read" | "read_file" | "glob" | "search_files" | "grep"
    )
}

fn has_peer_session_id(call: &ToolCall) -> bool {
    call.arguments
        .get("peer_session_id")
        .and_then(|value| value.as_str())
        .is_some_and(|value| !value.trim().is_empty())
}

/// Run a tool with the new sandboxed context. This is the v2 path that goes
/// through the permission service, doom loop detector, and sandbox policies
/// (path containment for fs tools, env-scrubbed shell, SSRF-guarded network).
///
/// Async because the sandboxed path can fire a UI ask through
/// `trigger_access` when the requested path lies outside the project root.
pub async fn run_tool_sandboxed(call: &ToolCall, ctx: &SandboxedContext) -> ToolResult {
    if has_peer_session_id(call) {
        return failure(
            &call.name,
            "peer_session_id requires the agent runner async dispatch".to_string(),
        );
    }

    // Doom loop check first.
    let args_repr = serde_json::to_string(&call.arguments).unwrap_or_default();
    if ctx.doom.record(&call.name, &args_repr) {
        return failure(
            &call.name,
            "doom loop: the same tool call has been made 3 times in a row".to_string(),
        );
    }

    // Permission check. Look up under the canonical permission name so
    // default rules work even when the LLM uses an alias.
    let permission = permission_for_tool(&call.name);
    let pattern = pattern_for(&call.name, call);
    let check = ctx.permissions.check(crate::permission_v2::CheckRequest {
        permission: permission.to_string(),
        pattern,
        tool: call.name.clone(),
        always: vec![],
        request_id: None,
    });
    if let Err(error) = check {
        return failure(&call.name, format!("{error}"));
    }

    // Dispatch.
    match call.name.as_str() {
        "read" | "read_file" => read_sandboxed(call, ctx).await,
        "write" | "write_file" => write_sandboxed(call, ctx).await,
        "edit" => edit_sandboxed(call, ctx).await,
        "apply_patch" => apply_patch_sandboxed(call, ctx).await,
        "glob" | "search_files" => glob_sandboxed(call, ctx).await,
        "grep" => grep_sandboxed(call, ctx).await,
        "shell" | "bash" => shell_sandboxed(call, ctx),
        "webfetch" => webfetch_sandboxed(call, ctx),
        "websearch" => websearch_sandboxed(call, ctx),
        "task" => failure(
            "task",
            "task requires the async runtime; the agent runner routes this tool to `run_tool_async`".to_string(),
        ),
        "skill" => skill::run(call),
        "todo" | "todowrite" => todo::run(call),
        "question" => question::run(call),
        "lsp" => lsp::run(call),
        "plan" => plan::run(call),
        "external_directory" => external_directory::run(call),
        "invalid" => invalid::run(call),
        _ => invalid::unknown(call),
    }
}

fn pattern_for(tool: &str, call: &ToolCall) -> String {
    let key = match tool {
        "read" | "read_file" | "write" | "write_file" | "edit" | "apply_patch" | "glob"
        | "grep" | "search_files" => "path",
        "shell" | "bash" => "command",
        "webfetch" => "url",
        "websearch" => "query",
        "external_directory" => "path",
        _ => "",
    };
    crate::tools::string_arg(call, key)
}

/// Map a tool name to the permission category used for rule lookup.
/// Aliases (e.g. `bash` for `shell`, `read_file` for `read`) collapse to a
/// canonical name so the default ruleset applies uniformly.
fn permission_for_tool(tool: &str) -> &'static str {
    match tool {
        "read" | "read_file" => "read",
        "glob" | "grep" | "search_files" => "glob",
        "write" | "write_file" => "write",
        "edit" => "edit",
        "apply_patch" => "apply_patch",
        "shell" | "bash" => "bash",
        "task" => "task",
        "skill" => "skill",
        "todo" | "todowrite" => "todo",
        "question" => "question",
        "webfetch" => "webfetch",
        "websearch" => "websearch",
        "lsp" => "lsp",
        "plan" => "plan",
        "external_directory" => "external_directory",
        "invalid" => "invalid",
        _ => "invalid",
    }
}

async fn read_sandboxed(call: &ToolCall, ctx: &SandboxedContext) -> ToolResult {
    let path = string_arg(call, "path");
    if path.is_empty() {
        return failure("read", "path is required".to_string());
    }
    match resolve_sandbox_path(&path, ctx, "read").await {
        Ok(canonical) => read::run_with_path(call, &canonical),
        Err(e) => failure("read", e),
    }
}

async fn write_sandboxed(call: &ToolCall, ctx: &SandboxedContext) -> ToolResult {
    let path = string_arg(call, "path");
    if path.is_empty() {
        return failure("write", "path is required".to_string());
    }
    match resolve_sandbox_path(&path, ctx, "write").await {
        Ok(canonical) => write::run_with_path(call, &canonical),
        Err(e) => failure("write", e),
    }
}

async fn edit_sandboxed(call: &ToolCall, ctx: &SandboxedContext) -> ToolResult {
    let path = string_arg(call, "path");
    let old = string_arg(call, "old_string");
    let new = string_arg(call, "new_string");
    if path.is_empty() {
        return failure("edit", "path is required".to_string());
    }
    if old.is_empty() {
        return failure("edit", "old_string is required".to_string());
    }
    match resolve_sandbox_path(&path, ctx, "edit").await {
        Ok(canonical) => edit::run_with_path(call, &canonical, &old, &new),
        Err(e) => failure("edit", e),
    }
}

async fn apply_patch_sandboxed(call: &ToolCall, ctx: &SandboxedContext) -> ToolResult {
    let patch_text = apply_patch::patch_text_arg(call);
    if patch_text.trim().is_empty() {
        return failure("apply_patch", "patchText is required".to_string());
    }
    match apply_patch::apply_patch_sandboxed(&patch_text, ctx).await {
        Ok(applied) => success("apply_patch", apply_patch::model_output(&applied)),
        Err(error) => failure("apply_patch", error),
    }
}

async fn glob_sandboxed(call: &ToolCall, ctx: &SandboxedContext) -> ToolResult {
    let path = string_arg(call, "path");
    let target = if path.is_empty() {
        ctx.project_root()
    } else {
        match resolve_sandbox_path(&path, ctx, "glob").await {
            Ok(p) => p,
            Err(e) => return failure("glob", e),
        }
    };
    glob::run_with_root(call, &target)
}

async fn grep_sandboxed(call: &ToolCall, ctx: &SandboxedContext) -> ToolResult {
    let path = string_arg(call, "path");
    let target = if path.is_empty() {
        ctx.project_root()
    } else {
        match resolve_sandbox_path(&path, ctx, "grep").await {
            Ok(p) => p,
            Err(e) => return failure("grep", e),
        }
    };
    grep::run_with_root(call, &target)
}

fn shell_sandboxed(call: &ToolCall, ctx: &SandboxedContext) -> ToolResult {
    let command = string_arg(call, "command");
    if command.is_empty() {
        return failure("shell", "command is required".to_string());
    }
    match crate::sandbox::run_shell(&command, &ctx.shell_policy) {
        Ok(out) => {
            let exit = match out.exit {
                ShellExit::Success => 0,
                ShellExit::NonZero(code) => code,
                ShellExit::Killed | ShellExit::TimedOut => 137,
                ShellExit::SpawnFailed => -1,
            };
            let body = if out.stderr.is_empty() {
                format!("{}\n[exit={}]", out.stdout, exit)
            } else {
                format!("{}\n[stderr]\n{}\n[exit={}]", out.stdout, out.stderr, exit)
            };
            if exit == 0 {
                success("shell", body)
            } else {
                failure("shell", body)
            }
        }
        Err(e) => failure("shell", format!("{e}")),
    }
}

fn webfetch_sandboxed(call: &ToolCall, ctx: &SandboxedContext) -> ToolResult {
    let url = string_arg(call, "url");
    if url.is_empty() {
        return failure("webfetch", "url is required".to_string());
    }
    if let Err(error) = ctx.network_policy.validate(&url) {
        return failure("webfetch", format!("{error}"));
    }
    // Real fetch isn't wired up yet; the v1 tool already returns the URL.
    webfetch::run(call)
}

async fn webfetch_async_sandboxed(call: &ToolCall, ctx: &SandboxedContext) -> ToolResult {
    let url = string_arg(call, "url");
    if url.is_empty() {
        return failure("webfetch", "url is required".to_string());
    }
    if let Err(error) = ctx.network_policy.validate(&url) {
        return failure("webfetch", format!("{error}"));
    }
    webfetch::run_async(call).await
}

fn websearch_sandboxed(call: &ToolCall, ctx: &SandboxedContext) -> ToolResult {
    let query = string_arg(call, "query");
    if query.trim().is_empty() {
        return failure("websearch", "query is required".to_string());
    }
    let config = match websearch::config_from_env() {
        Ok(config) => config,
        Err(error) => return failure("websearch", error),
    };
    let url = match websearch::build_search_url(&config.base_url, &query) {
        Ok(url) => url,
        Err(error) => return failure("websearch", error),
    };
    if let Err(error) = ctx.network_policy.validate(url.as_str()) {
        return failure("websearch", format!("{error}"));
    }
    websearch::run(call)
}

async fn websearch_async_sandboxed(call: &ToolCall, ctx: &SandboxedContext) -> ToolResult {
    let query = string_arg(call, "query");
    if query.trim().is_empty() {
        return failure("websearch", "query is required".to_string());
    }
    let config = match websearch::config_from_env() {
        Ok(config) => config,
        Err(error) => return failure("websearch", error),
    };
    let url = match websearch::build_search_url(&config.base_url, &query) {
        Ok(url) => url,
        Err(error) => return failure("websearch", error),
    };
    if let Err(error) = ctx.network_policy.validate(url.as_str()) {
        return failure("websearch", format!("{error}"));
    }
    websearch::run_with_async(call, &reqwest::Client::new(), &config).await
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use crate::permission::PermissionMode;
    use crate::permission_v2::{default_ruleset, PermissionService};
    use crate::ToolCall;

    use super::{
        resolve_session_path, run_tool_async_sandboxed, run_tool_sandboxed, run_tool_with_mode,
        SandboxedContext, ToolContext, ToolRuntime,
    };

    #[test]
    fn resolve_session_path_anchors_relative_paths_to_project_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let sub = tmp.path().join("nested");
        fs::create_dir_all(&sub).expect("mkdir");
        let file = sub.join("note.txt");
        fs::write(&file, "hi").expect("write");

        let ctx = ToolContext::default().with_project_root(tmp.path().to_path_buf());
        let resolved = resolve_session_path("nested/note.txt", &ctx, "read");
        assert_eq!(resolved, file);
    }

    #[test]
    fn resolve_session_path_passes_through_absolute_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("absolute.txt");
        let absolute = file.to_string_lossy().to_string();
        let ctx =
            ToolContext::default().with_project_root(PathBuf::from("/this/should/not/append"));
        let resolved = resolve_session_path(&absolute, &ctx, "read");
        assert_eq!(resolved, file);
    }

    #[test]
    fn resolve_session_path_falls_back_to_process_cwd_when_no_project_root() {
        // The session has no project_root — the resolver logs a
        // warning and uses the process cwd. The test only
        // asserts no panic; the actual path is whatever
        // `std::env::current_dir()` returns.
        let ctx = ToolContext::default();
        let resolved = resolve_session_path("note.txt", &ctx, "read");
        assert!(resolved.is_absolute() || resolved == PathBuf::from("note.txt"));
    }

    #[test]
    fn plan_mode_blocks_write_before_file_mutation() {
        let path = std::env::temp_dir().join(format!(
            "arachne-plan-deny-{}.txt",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let result = run_tool_with_mode(
            &call(
                "write",
                &[
                    ("path", path.to_string_lossy().as_ref()),
                    ("content", "blocked"),
                ],
            ),
            PermissionMode::Plan,
        );

        assert!(!result.success);
        assert!(!path.exists());
    }

    #[test]
    fn build_mode_allows_write() {
        let path = std::env::temp_dir().join(format!(
            "arachne-build-allow-{}.txt",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let result = run_tool_with_mode(
            &call(
                "write",
                &[
                    ("path", path.to_string_lossy().as_ref()),
                    ("content", "allowed"),
                ],
            ),
            PermissionMode::Build,
        );

        assert!(result.success);
        assert_eq!(fs::read_to_string(&path).unwrap(), "allowed");
        let _ = fs::remove_file(path);
    }

    fn call(name: &str, args: &[(&str, &str)]) -> ToolCall {
        ToolCall {
            name: name.to_string(),
            arguments: args
                .iter()
                .map(|(key, value)| (key.to_string(), json!(value)))
                .collect::<HashMap<_, _>>(),
        }
    }

    fn make_context() -> (SandboxedContext, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = crate::sandbox::SandboxPolicy::new(dir.path().to_path_buf());
        let (svc, _rx) = PermissionService::new("sandboxed-test", default_ruleset());
        // Leak the receiver so the channel doesn't close mid-test.
        Box::leak(Box::new(_rx));
        let ctx = SandboxedContext::new(sandbox, svc);
        (ctx, dir)
    }

    #[tokio::test]
    async fn sandboxed_read_within_root_succeeds() {
        let (ctx, dir) = make_context();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hello").unwrap();
        let result =
            run_tool_sandboxed(&call("read", &[("path", file.to_str().unwrap())]), &ctx).await;
        assert!(result.success, "result: {:?}", result);
        assert!(result.output.contains("hello"));
    }

    #[tokio::test]
    async fn sandboxed_read_outside_root_rejected() {
        let (ctx, _dir) = make_context();
        let result = run_tool_sandboxed(&call("read", &[("path", "/etc/passwd")]), &ctx).await;
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("outside"));
    }

    #[tokio::test]
    async fn sandboxed_read_escapes_via_dotdot() {
        let (ctx, dir) = make_context();
        let escape = format!("{}/../etc/passwd", dir.path().display());
        let result = run_tool_sandboxed(&call("read", &[("path", escape.as_str())]), &ctx).await;
        assert!(!result.success);
    }

    #[tokio::test]
    async fn sandboxed_write_creates_file() {
        let (ctx, dir) = make_context();
        let file = dir.path().join("new.txt");
        let result = run_tool_sandboxed(
            &call(
                "write",
                &[("path", file.to_str().unwrap()), ("content", "wrote")],
            ),
            &ctx,
        )
        .await;
        assert!(result.success, "result: {:?}", result);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "wrote");
    }

    #[tokio::test]
    async fn sandboxed_write_outside_root_rejected() {
        let (ctx, _dir) = make_context();
        let result = run_tool_sandboxed(
            &call(
                "write",
                &[("path", "/tmp/should-not-write.txt"), ("content", "x")],
            ),
            &ctx,
        )
        .await;
        assert!(!result.success);
    }

    #[tokio::test]
    async fn sandboxed_glob_within_root() {
        let (ctx, dir) = make_context();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();
        let result = run_tool_sandboxed(&call("glob", &[("pattern", "*.txt")]), &ctx).await;
        assert!(result.success, "result: {:?}", result);
        assert!(result.output.contains("a.txt"));
    }

    #[tokio::test]
    async fn sandboxed_glob_with_dotdot_root_rejected() {
        let (ctx, _dir) = make_context();
        let result =
            run_tool_sandboxed(&call("glob", &[("path", "/etc"), ("pattern", "*")]), &ctx).await;
        assert!(!result.success);
    }

    #[tokio::test]
    async fn sandboxed_grep_finds_match() {
        let (ctx, dir) = make_context();
        std::fs::write(dir.path().join("a.txt"), "the quick brown fox").unwrap();
        let result = run_tool_sandboxed(&call("grep", &[("pattern", "brown")]), &ctx).await;
        assert!(result.success);
        assert!(result.output.contains("brown"));
    }

    #[tokio::test]
    async fn sandboxed_shell_runs_in_cwd() {
        let (ctx, dir) = make_context();
        let result = run_tool_sandboxed(&call("shell", &[("command", "pwd")]), &ctx).await;
        assert!(result.success, "result: {:?}", result);
        // The output should be the canonicalized cwd.
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        assert!(
            result.output.contains(canonical.to_str().unwrap())
                || result.output.contains(dir.path().to_str().unwrap()),
            "output did not contain cwd: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn sandboxed_shell_captures_non_zero_exit() {
        let (ctx, _dir) = make_context();
        let result = run_tool_sandboxed(&call("shell", &[("command", "false")]), &ctx).await;
        assert!(!result.success);
    }

    #[tokio::test]
    async fn sandboxed_doom_loop_blocks_repeats() {
        let (ctx, _dir) = make_context();
        let args = &[("command", "echo hi")][..];
        let c = || call("shell", args);
        // Two repeats are fine; the third triggers.
        assert!(run_tool_sandboxed(&c(), &ctx).await.success);
        assert!(run_tool_sandboxed(&c(), &ctx).await.success);
        let result = run_tool_sandboxed(&c(), &ctx).await;
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("doom"));
    }

    #[tokio::test]
    async fn sandboxed_different_args_dont_trigger_doom() {
        let (ctx, _dir) = make_context();
        let result1 = run_tool_sandboxed(&call("shell", &[("command", "ls")]), &ctx).await;
        let result2 = run_tool_sandboxed(&call("shell", &[("command", "pwd")]), &ctx).await;
        let result3 = run_tool_sandboxed(&call("shell", &[("command", "echo hi")]), &ctx).await;
        assert!(result1.success);
        assert!(result2.success);
        assert!(result3.success);
    }

    #[tokio::test]
    async fn sandboxed_webfetch_blocks_loopback() {
        let (ctx, _dir) = make_context();
        let result = run_tool_sandboxed(
            &call("webfetch", &[("url", "http://127.0.0.1:8080/secret")]),
            &ctx,
        )
        .await;
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("blocked"));
    }

    #[tokio::test]
    async fn sandboxed_webfetch_allows_public_url() {
        // The sandboxed path is the sync dispatcher; real network
        // I/O is on the async path. The sandboxed success
        // criterion here is therefore: the URL passes the
        // network policy check (i.e. isn't loopback-blocked), and
        // the dispatch reaches the `webfetch` tool. The
        // async-required error is the expected terminal state
        // for the sync path; the previous "validated URL"
        // success was a stub.
        let (ctx, _dir) = make_context();
        let result =
            run_tool_sandboxed(&call("webfetch", &[("url", "https://example.com")]), &ctx).await;
        let error = result.error.as_deref().unwrap_or("");
        assert!(
            !result.success && error.contains("async runtime"),
            "expected async-routing error (network policy passed), got: {result:?}"
        );
    }

    #[tokio::test]
    async fn peer_session_id_read_uses_peer_directory_and_reuses_subsession() {
        let db_dir = tempfile::tempdir().unwrap();
        let conv_dir = tempfile::tempdir().unwrap();
        let caller_dir = tempfile::tempdir().unwrap();
        let peer_dir = tempfile::tempdir().unwrap();
        std::fs::write(caller_dir.path().join("note.txt"), "caller").unwrap();
        std::fs::write(peer_dir.path().join("note.txt"), "peer secret").unwrap();

        let db_path = db_dir.path().join("sessions.sqlite");
        let db = crate::database::Database::new(db_path.clone()).unwrap();
        db.init().unwrap();
        crate::database::ProjectRepository::insert(
            &db,
            &crate::Project {
                id: "caller-project".to_string(),
                path: caller_dir.path().display().to_string(),
                name: "caller".to_string(),
                tech_stack: vec![],
                created_at: chrono::Utc::now(),
            },
        )
        .unwrap();
        crate::database::ProjectRepository::insert(
            &db,
            &crate::Project {
                id: "peer-project".to_string(),
                path: peer_dir.path().display().to_string(),
                name: "peer".to_string(),
                tech_stack: vec![],
                created_at: chrono::Utc::now(),
            },
        )
        .unwrap();
        drop(db);

        let session_service = crate::SessionService::new(db_path.clone());
        let conversation_service = crate::ConversationService::new(conv_dir.path().to_path_buf());
        let caller_id = session_service
            .create_session(
                "caller-project".to_string(),
                caller_dir.path().display().to_string(),
                "openai".to_string(),
                "gpt-5".to_string(),
            )
            .unwrap();
        let peer_id = session_service
            .create_session(
                "peer-project".to_string(),
                peer_dir.path().display().to_string(),
                "openai".to_string(),
                "gpt-5".to_string(),
            )
            .unwrap();
        session_service
            .create_group(vec![caller_id.clone(), peer_id.clone()])
            .unwrap();

        let registry = crate::llm::SubagentRegistry::new(db_path);
        let runtime = ToolRuntime {
            caller_session_id: caller_id.clone(),
            session_service,
            conversation_service,
            providers: std::sync::Arc::new(crate::llm::ProviderRegistry::new()),
            subagent_registry: registry.clone(),
            mode: PermissionMode::Plan,
            turn_id: 11,
            project_root: caller_dir.path().to_path_buf(),
        };
        let (permissions, rx) = PermissionService::new("peer-read-test", default_ruleset());
        Box::leak(Box::new(rx));
        let ctx = SandboxedContext::new(
            crate::sandbox::SandboxPolicy::new(caller_dir.path().to_path_buf()),
            permissions,
        );

        let result = run_tool_async_sandboxed(
            &call(
                "read",
                &[("path", "note.txt"), ("peer_session_id", peer_id.as_str())],
            ),
            &runtime,
            &ctx,
        )
        .await;
        assert!(result.success, "result: {result:?}");
        assert!(result.output.contains("peer secret"));
        assert!(!result.output.contains("caller"));

        let second = run_tool_async_sandboxed(
            &call(
                "read",
                &[("path", "note.txt"), ("peer_session_id", peer_id.as_str())],
            ),
            &runtime,
            &ctx,
        )
        .await;
        assert!(second.success, "result: {second:?}");
        assert_eq!(registry.live_children(&caller_id).len(), 1);
    }
}

pub(crate) fn string_arg(call: &ToolCall, key: &str) -> String {
    call.arguments
        .get(key)
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}

pub(crate) fn usize_arg(call: &ToolCall, key: &str) -> Option<usize> {
    call.arguments
        .get(key)
        .and_then(|value| value.as_u64())
        .and_then(|value| usize::try_from(value).ok())
}

pub(crate) fn success(tool: &str, output: String) -> ToolResult {
    ToolResult {
        tool: tool.to_string(),
        success: true,
        output,
        error: None,
    }
}

pub(crate) fn failure(tool: &str, error: String) -> ToolResult {
    ToolResult {
        tool: tool.to_string(),
        success: false,
        output: String::new(),
        error: Some(error),
    }
}

pub(crate) fn not_implemented(tool: &str, detail: &str) -> ToolResult {
    failure(
        tool,
        format!("{tool} requires runtime integration: {detail}"),
    )
}

pub(crate) fn wildcard_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" || pattern == "**" {
        return true;
    }
    if !pattern.contains('*') {
        return value.contains(pattern);
    }

    let mut remaining = value;
    for part in pattern.split('*').filter(|part| !part.is_empty()) {
        if let Some(index) = remaining.find(part) {
            remaining = &remaining[index + part.len()..];
        } else {
            return false;
        }
    }
    true
}
