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
    call: Option<&ToolCall>,
) -> Result<std::path::PathBuf, String> {
    let process_cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    if let Some(peer_session_id) = peer_session_id_for_read_only_call(call, tool) {
        return resolve_peer_sandbox_path(requested, ctx, tool, &peer_session_id, &process_cwd);
    }

    // Fast path: try the policy without an ask. Keep the guard scoped
    // outside the logging block; otherwise the `if let` temporary can
    // hold the mutex while the log fields try to lock it again.
    let fast_result = {
        let sandbox = ctx.sandbox.lock();
        let project_root = sandbox.project_root.clone();
        sandbox
            .resolve(requested)
            .map(|resolved| (resolved, project_root))
    };
    if let Ok((resolved, project_root)) = fast_result {
        tracing::info!(
            tool,
            requested = %requested,
            project_root = %project_root.display(),
            resolved = %resolved.display(),
            process_cwd = %process_cwd,
            inside_project_root = project_root_contains(
                &project_root,
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

fn peer_session_id_for_read_only_call(call: Option<&ToolCall>, tool: &str) -> Option<String> {
    if !supports_peer_session_id(tool) {
        return None;
    }
    call.and_then(|call| {
        let peer_session_id = string_arg(call, "peer_session_id");
        let trimmed = peer_session_id.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn resolve_peer_sandbox_path(
    requested: &str,
    ctx: &SandboxedContext,
    tool: &str,
    peer_session_id: &str,
    process_cwd: &str,
) -> Result<std::path::PathBuf, String> {
    let (peer_root, peer_policy) = ctx.peer_policy(peer_session_id)?;
    match peer_policy.resolve(requested) {
        Ok(canonical) => {
            tracing::info!(
                tool,
                caller_session_id = %ctx.caller_session_id.as_deref().unwrap_or("<unknown>"),
                peer_session_id = %peer_session_id,
                peer_root = %peer_root.display(),
                requested = %requested,
                canonical = %canonical.display(),
                process_cwd = %process_cwd,
                sandbox_allowed = true,
                "sandbox path resolution allowed via peer"
            );
            Ok(canonical)
        }
        Err(error) => {
            tracing::warn!(
                tool,
                caller_session_id = %ctx.caller_session_id.as_deref().unwrap_or("<unknown>"),
                peer_session_id = %peer_session_id,
                peer_root = %peer_root.display(),
                requested = %requested,
                process_cwd = %process_cwd,
                sandbox_allowed = false,
                error = %error,
                "sandbox path resolution rejected by peer containment"
            );
            Err(format!(
                "path '{}' is outside peer session directory '{}'",
                requested,
                peer_root.display()
            ))
        }
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
/// Read-only calls may also carry `peer_session_id`; when this context has
/// caller metadata, those paths resolve against the named connected peer's
/// directory and must remain contained there.
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
    pub session_service: Option<Arc<crate::SessionService>>,
    pub caller_session_id: Option<String>,
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
            session_service: None,
            caller_session_id: None,
        }
    }

    pub fn with_caller_session(
        mut self,
        caller_session_id: impl Into<String>,
        session_service: Arc<crate::SessionService>,
    ) -> Self {
        self.caller_session_id = Some(caller_session_id.into());
        self.session_service = Some(session_service);
        self
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

    fn peer_policy(&self, peer_session_id: &str) -> Result<(PathBuf, SandboxPolicy), String> {
        let session_service = self.session_service.as_ref().ok_or_else(|| {
            "peer_session_id requires sandboxed context to be associated with a caller session"
                .to_string()
        })?;
        let caller_session_id = self.caller_session_id.as_ref().ok_or_else(|| {
            "peer_session_id requires sandboxed context to be associated with a caller session"
                .to_string()
        })?;
        if peer_session_id == caller_session_id {
            return Err(
                "peer_session_id must refer to a different connected session; omit it for local work"
                    .to_string(),
            );
        }

        crate::routing::integration::validate_connected_peer(
            caller_session_id,
            peer_session_id,
            session_service,
        )?;

        let peer = session_service
            .get_session(peer_session_id)?
            .ok_or_else(|| format!("peer session not found: {peer_session_id}"))?;
        let policy = SandboxPolicy::new(PathBuf::from(&peer.directory));
        let peer_root = policy.project_root.clone();
        Ok((peer_root, policy))
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
    if has_peer_session_id(call) {
        if let Err(error) = validate_peer_tool_request(call, runtime.mode) {
            return failure(&call.name, error);
        }
        if let Err(error) = resolve_peer_tool_target(runtime, &string_arg(call, "peer_session_id"))
        {
            return failure(&call.name, error);
        }
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

    if let Err(error) = validate_peer_tool_request(call, runtime.mode) {
        return Some(failure(&call.name, error));
    }

    let peer_directory = match resolve_peer_tool_target(runtime, &peer_session_id) {
        Ok(target) => target,
        Err(error) => return Some(failure(&call.name, error)),
    };

    tracing::info!(
        caller_session_id = %runtime.caller_session_id,
        peer_session_id = %peer_session_id,
        tool = %call.name,
        peer_directory = %peer_directory.display(),
        "dispatching peer-targeted tool call through temporary peer sandbox"
    );

    let mut peer_call = call.clone();
    peer_call.arguments.remove("peer_session_id");
    normalize_peer_path_argument(&mut peer_call, &peer_directory);

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
) -> Result<PathBuf, String> {
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
                "child sessions cannot target peer sessions (depth cap exceeded)".to_string()
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

    Ok(PathBuf::from(&peer.directory))
}

fn normalize_peer_path_argument(call: &mut ToolCall, peer_directory: &std::path::Path) {
    let Some(value) = call.arguments.get_mut("path") else {
        return;
    };
    let Some(requested) = value.as_str() else {
        return;
    };
    let normalized = normalize_peer_requested_path(requested, peer_directory);
    if normalized != requested {
        *value = serde_json::Value::String(normalized);
    }
}

fn normalize_peer_requested_path(requested: &str, peer_directory: &std::path::Path) -> String {
    let requested = requested.trim();
    if requested.is_empty() || requested == "." {
        return requested.to_string();
    }

    let requested_path = std::path::Path::new(requested);
    if requested_path == peer_directory {
        return ".".to_string();
    }
    if let Ok(relative) = requested_path.strip_prefix(peer_directory) {
        return path_for_tool_arg(relative);
    }

    let requested_norm = normalize_path_text(requested);
    let peer_norm = normalize_path_text(&peer_directory.display().to_string());
    if requested_norm == peer_norm {
        return ".".to_string();
    }
    let peer_prefix = format!("{peer_norm}/");
    if let Some(rest) = requested_norm.strip_prefix(&peer_prefix) {
        return if rest.is_empty() {
            ".".to_string()
        } else {
            rest.to_string()
        };
    }

    // Models sometimes copy a Windows peer path from the prompt without the
    // drive prefix (e.g. \Users\name\repo). On Windows that is rooted on the
    // current drive; on Unix it is just a relative path containing backslashes.
    // If that drive-less form points at the peer root, normalize it too.
    let requested_suffix = requested_norm.trim_start_matches('/');
    for peer_suffix in peer_root_suffixes(&peer_norm) {
        if requested_suffix == peer_suffix {
            return ".".to_string();
        }
        let peer_suffix_prefix = format!("{peer_suffix}/");
        if let Some(rest) = requested_suffix.strip_prefix(&peer_suffix_prefix) {
            return if rest.is_empty() {
                ".".to_string()
            } else {
                rest.to_string()
            };
        }
    }

    requested.to_string()
}

fn peer_root_suffixes(peer_norm: &str) -> Vec<&str> {
    let mut suffixes = Vec::new();
    if let Some((_, rest)) = peer_norm.split_once(":/") {
        suffixes.push(rest);
    }
    if let Some(rest) = peer_norm.strip_prefix("/mnt/") {
        if let Some((drive, suffix)) = rest.split_once('/') {
            if drive.len() == 1 && !suffix.is_empty() {
                suffixes.push(suffix);
            }
        }
    }
    if suffixes.is_empty() {
        suffixes.push(peer_norm);
    }
    suffixes
}

fn normalize_path_text(path: &str) -> String {
    let mut path = path.trim().replace('\\', "/");
    while path.contains("//") {
        path = path.replace("//", "/");
    }
    path.trim_end_matches('/').to_ascii_lowercase()
}

fn path_for_tool_arg(path: &std::path::Path) -> String {
    if path.as_os_str().is_empty() {
        ".".to_string()
    } else {
        path.to_string_lossy().replace('\\', "/")
    }
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
    if has_peer_session_id(call) && !supports_peer_session_id(&call.name) {
        return failure(
            &call.name,
            format!(
                "peer_session_id is only supported for read, glob, and grep plan-mode tool calls; omit peer_session_id for local work with {}",
                call.name
            ),
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
    match resolve_sandbox_path(&path, ctx, "read", Some(call)).await {
        Ok(canonical) => read::run_with_path(call, &canonical),
        Err(e) => failure("read", e),
    }
}

async fn write_sandboxed(call: &ToolCall, ctx: &SandboxedContext) -> ToolResult {
    let path = string_arg(call, "path");
    if path.is_empty() {
        return failure("write", "path is required".to_string());
    }
    match resolve_sandbox_path(&path, ctx, "write", None).await {
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
    match resolve_sandbox_path(&path, ctx, "edit", None).await {
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
        Ok(applied) => success_with_metadata(
            "apply_patch",
            apply_patch::model_output(&applied),
            apply_patch::diff_metadata(&applied),
        ),
        Err(error) => failure("apply_patch", error),
    }
}

async fn glob_sandboxed(call: &ToolCall, ctx: &SandboxedContext) -> ToolResult {
    let path = string_arg(call, "path");
    let target = if path.is_empty() {
        if has_peer_session_id(call) {
            match resolve_sandbox_path(".", ctx, "glob", Some(call)).await {
                Ok(p) => p,
                Err(e) => return failure("glob", e),
            }
        } else {
            ctx.project_root()
        }
    } else {
        match resolve_sandbox_path(&path, ctx, "glob", Some(call)).await {
            Ok(p) => p,
            Err(e) => return failure("glob", e),
        }
    };
    tracing::debug!(
        requested_path = %path,
        target = %target.display(),
        arguments = ?call.arguments,
        "glob sandboxed before run_with_root"
    );
    let started = std::time::Instant::now();
    let result = glob::run_with_root(call, &target);
    tracing::debug!(
        requested_path = %path,
        target = %target.display(),
        elapsed_ms = started.elapsed().as_millis(),
        success = result.success,
        output = %result.output,
        error = %result.error.as_deref().unwrap_or(""),
        "glob sandboxed after run_with_root"
    );
    result
}

fn validate_peer_tool_request(call: &ToolCall, mode: PermissionMode) -> Result<(), String> {
    if !supports_peer_session_id(&call.name) {
        return Err(format!(
            "peer_session_id is only supported for read, glob, and grep plan-mode tool calls; omit peer_session_id for local work with {}",
            call.name
        ));
    }
    if mode != PermissionMode::Plan {
        return Err(
            "peer_session_id is only supported in plan mode for read-only context gathering"
                .to_string(),
        );
    }
    Ok(())
}

async fn grep_sandboxed(call: &ToolCall, ctx: &SandboxedContext) -> ToolResult {
    let path = string_arg(call, "path");
    let target = if path.is_empty() {
        if has_peer_session_id(call) {
            match resolve_sandbox_path(".", ctx, "grep", Some(call)).await {
                Ok(p) => p,
                Err(e) => return failure("grep", e),
            }
        } else {
            ctx.project_root()
        }
    } else {
        match resolve_sandbox_path(&path, ctx, "grep", Some(call)).await {
            Ok(p) => p,
            Err(e) => return failure("grep", e),
        }
    };
    tracing::debug!(
        requested_path = %path,
        target = %target.display(),
        arguments = ?call.arguments,
        "grep sandboxed before run_with_root"
    );
    let started = std::time::Instant::now();
    let result = grep::run_with_root(call, &target);
    tracing::debug!(
        requested_path = %path,
        target = %target.display(),
        elapsed_ms = started.elapsed().as_millis(),
        success = result.success,
        output = %result.output,
        error = %result.error.as_deref().unwrap_or(""),
        "grep sandboxed after run_with_root"
    );
    result
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
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use crate::permission::PermissionMode;
    use crate::permission_v2::{default_ruleset, PermissionService};
    use crate::ToolCall;

    use super::{
        normalize_peer_requested_path, resolve_session_path, run_tool_async_sandboxed,
        run_tool_sandboxed, run_tool_with_mode, SandboxedContext, ToolContext, ToolRuntime,
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

    struct PeerFixture {
        _db_dir: tempfile::TempDir,
        _conv_dir: tempfile::TempDir,
        caller_dir: tempfile::TempDir,
        peer_dir: tempfile::TempDir,
        outsider_dir: tempfile::TempDir,
        peer_id: String,
        outsider_id: String,
        runtime: ToolRuntime,
        ctx: SandboxedContext,
    }

    fn insert_project(db: &crate::database::Database, id: &str, dir: &std::path::Path) {
        crate::database::ProjectRepository::insert(
            db,
            &crate::Project {
                id: id.to_string(),
                path: dir.display().to_string(),
                name: id.to_string(),
                tech_stack: vec![],
                created_at: chrono::Utc::now(),
            },
        )
        .unwrap();
    }

    fn make_peer_fixture() -> PeerFixture {
        let db_dir = tempfile::tempdir().unwrap();
        let conv_dir = tempfile::tempdir().unwrap();
        let caller_dir = tempfile::tempdir().unwrap();
        let peer_dir = tempfile::tempdir().unwrap();
        let outsider_dir = tempfile::tempdir().unwrap();

        let db_path = db_dir.path().join("sessions.sqlite");
        let db = crate::database::Database::new(db_path.clone()).unwrap();
        db.init().unwrap();
        insert_project(&db, "caller-project", caller_dir.path());
        insert_project(&db, "peer-project", peer_dir.path());
        insert_project(&db, "outsider-project", outsider_dir.path());
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
        let outsider_id = session_service
            .create_session(
                "outsider-project".to_string(),
                outsider_dir.path().display().to_string(),
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
            session_service: Arc::clone(&session_service),
            conversation_service,
            providers: Arc::new(crate::llm::ProviderRegistry::new()),
            subagent_registry: registry,
            mode: PermissionMode::Plan,
            turn_id: 17,
            project_root: caller_dir.path().to_path_buf(),
        };
        let (permissions, rx) = PermissionService::new("peer-fixture", default_ruleset());
        Box::leak(Box::new(rx));
        let ctx = SandboxedContext::new(
            crate::sandbox::SandboxPolicy::new(caller_dir.path().to_path_buf()),
            permissions,
        )
        .with_caller_session(caller_id.clone(), Arc::clone(&session_service));

        PeerFixture {
            _db_dir: db_dir,
            _conv_dir: conv_dir,
            caller_dir,
            peer_dir,
            outsider_dir,
            peer_id,
            outsider_id,
            runtime,
            ctx,
        }
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
    async fn peer_session_id_read_uses_peer_directory_without_persisting_child_session() {
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
            session_service: Arc::clone(&session_service),
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
        )
        .with_caller_session(caller_id.clone(), Arc::clone(&session_service));

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
        assert_eq!(registry.live_children(&caller_id).len(), 0);
        let sessions = session_service.get_all_sessions().unwrap();
        assert!(sessions
            .iter()
            .all(|session| session.parent_session_id.as_deref() != Some(caller_id.as_str())));
    }

    #[tokio::test]
    async fn sandboxed_peer_read_resolves_relative_path_against_peer_not_caller() {
        let fixture = make_peer_fixture();
        std::fs::write(fixture.caller_dir.path().join("note.txt"), "caller").unwrap();
        std::fs::write(fixture.peer_dir.path().join("note.txt"), "peer").unwrap();

        let result = run_tool_async_sandboxed(
            &call(
                "read",
                &[
                    ("path", "note.txt"),
                    ("peer_session_id", fixture.peer_id.as_str()),
                ],
            ),
            &fixture.runtime,
            &fixture.ctx,
        )
        .await;

        assert!(result.success, "result: {result:?}");
        assert!(result.output.contains("peer"));
        assert!(!result.output.contains("caller"));
    }

    #[tokio::test]
    async fn sandboxed_peer_read_rejects_path_outside_named_peer() {
        let fixture = make_peer_fixture();
        let caller_file = fixture.caller_dir.path().join("caller-only.txt");
        std::fs::write(&caller_file, "caller").unwrap();

        let result = run_tool_async_sandboxed(
            &call(
                "read",
                &[
                    ("path", caller_file.to_str().unwrap()),
                    ("peer_session_id", fixture.peer_id.as_str()),
                ],
            ),
            &fixture.runtime,
            &fixture.ctx,
        )
        .await;

        assert!(!result.success, "result: {result:?}");
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("outside peer session directory"));
    }

    #[test]
    fn normalize_peer_requested_path_accepts_wsl_peer_root_without_drive() {
        let peer_root = std::path::Path::new("/mnt/c/Users/caona/Documents/static-websites");

        assert_eq!(
            normalize_peer_requested_path(r"\Users\caona\Documents\static-websites", peer_root),
            "."
        );
        assert_eq!(
            normalize_peer_requested_path(
                r"\Users\caona\Documents\static-websites\index.html",
                peer_root
            ),
            "index.html"
        );
    }

    #[tokio::test]
    async fn peer_glob_accepts_peer_root_path_argument() {
        let fixture = make_peer_fixture();
        std::fs::write(fixture.peer_dir.path().join("index.html"), "<h1>peer</h1>").unwrap();

        let result = run_tool_async_sandboxed(
            &call(
                "glob",
                &[
                    ("path", fixture.peer_dir.path().to_str().unwrap()),
                    ("pattern", "**/*"),
                    ("peer_session_id", fixture.peer_id.as_str()),
                ],
            ),
            &fixture.runtime,
            &fixture.ctx,
        )
        .await;

        assert!(result.success, "result: {result:?}");
        assert!(result.output.contains("index.html"));
    }

    #[tokio::test]
    async fn peer_tools_do_not_persist_child_sessions_across_turns() {
        let fixture = make_peer_fixture();
        std::fs::write(fixture.peer_dir.path().join("note.txt"), "peer").unwrap();

        let first = run_tool_async_sandboxed(
            &call(
                "read",
                &[
                    ("path", "note.txt"),
                    ("peer_session_id", fixture.peer_id.as_str()),
                ],
            ),
            &fixture.runtime,
            &fixture.ctx,
        )
        .await;
        assert!(first.success, "result: {first:?}");
        let mut next_runtime = fixture.runtime.clone();
        next_runtime.turn_id += 1;
        let second = run_tool_async_sandboxed(
            &call(
                "read",
                &[
                    ("path", "note.txt"),
                    ("peer_session_id", fixture.peer_id.as_str()),
                ],
            ),
            &next_runtime,
            &fixture.ctx,
        )
        .await;

        assert!(second.success, "result: {second:?}");
        assert_eq!(
            next_runtime
                .subagent_registry
                .live_children(&next_runtime.caller_session_id)
                .len(),
            0
        );
        let sessions = next_runtime.session_service.get_all_sessions().unwrap();
        assert!(sessions.iter().all(|session| {
            session.parent_session_id.as_deref() != Some(next_runtime.caller_session_id.as_str())
        }));
    }

    #[tokio::test]
    async fn sandboxed_peer_read_rejects_unconnected_peer_without_ask() {
        let fixture = make_peer_fixture();
        std::fs::write(fixture.outsider_dir.path().join("note.txt"), "outsider").unwrap();

        let result = run_tool_async_sandboxed(
            &call(
                "read",
                &[
                    ("path", "note.txt"),
                    ("peer_session_id", fixture.outsider_id.as_str()),
                ],
            ),
            &fixture.runtime,
            &fixture.ctx,
        )
        .await;

        assert!(!result.success, "result: {result:?}");
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("is not connected to this session"));
    }

    #[tokio::test]
    async fn sandboxed_peer_id_on_write_does_not_mutate_peer() {
        let fixture = make_peer_fixture();
        let peer_file = fixture.peer_dir.path().join("write.txt");

        let result = run_tool_async_sandboxed(
            &call(
                "write",
                &[
                    ("path", peer_file.to_str().unwrap()),
                    ("content", "nope"),
                    ("peer_session_id", fixture.peer_id.as_str()),
                ],
            ),
            &fixture.runtime,
            &fixture.ctx,
        )
        .await;

        assert!(!result.success, "result: {result:?}");
        assert!(!peer_file.exists());
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
        metadata: None,
    }
}

pub(crate) fn success_with_metadata(
    tool: &str,
    output: String,
    metadata: serde_json::Value,
) -> ToolResult {
    ToolResult {
        tool: tool.to_string(),
        success: true,
        output,
        error: None,
        metadata: Some(metadata),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolDiff {
    pub diff: String,
    pub additions: usize,
    pub deletions: usize,
}

pub(crate) fn unified_diff(path: &str, before: Option<&str>, after: Option<&str>) -> ToolDiff {
    if before == after {
        return ToolDiff {
            diff: String::new(),
            additions: 0,
            deletions: 0,
        };
    }

    let before_lines = before.map(split_diff_lines).unwrap_or_default();
    let after_lines = after.map(split_diff_lines).unwrap_or_default();
    let before_start = if before_lines.is_empty() { 0 } else { 1 };
    let after_start = if after_lines.is_empty() { 0 } else { 1 };
    let mut diff = format!(
        "--- a/{path}\n+++ b/{path}\n@@ -{},{} +{},{} @@\n",
        before_start,
        before_lines.len(),
        after_start,
        after_lines.len(),
    );

    for line in &before_lines {
        diff.push('-');
        diff.push_str(line);
        diff.push('\n');
    }
    for line in &after_lines {
        diff.push('+');
        diff.push_str(line);
        diff.push('\n');
    }

    ToolDiff {
        diff,
        additions: after_lines.len(),
        deletions: before_lines.len(),
    }
}

fn split_diff_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    text.lines().map(str::to_string).collect()
}

pub(crate) fn failure(tool: &str, error: String) -> ToolResult {
    ToolResult {
        tool: tool.to_string(),
        success: false,
        output: String::new(),
        error: Some(error),
        metadata: None,
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
    if let Some(root_optional_pattern) = pattern.strip_prefix("**/") {
        return wildcard_match(root_optional_pattern, value)
            || value.contains('/')
                && wildcard_match(
                    root_optional_pattern,
                    value.rsplit('/').next().unwrap_or(value),
                );
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
