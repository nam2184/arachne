pub mod apply_patch;
pub mod ask_peer;
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
use crate::sandbox::{DoomLoopDetector, NetworkPolicy, SandboxPolicy, ShellExit, ShellPolicy};
use crate::{Tool, ToolCall, ToolResult};

pub use output_bounds::{
    bound_tool_output, estimate_tokens, BoundedOutput, CHARS_PER_TOKEN, GLOB_DEFAULT_LIMIT,
    GREP_DEFAULT_LIMIT, MAX_TOOL_OUTPUT_BYTES, MAX_TOOL_OUTPUT_LINES, READ_DEFAULT_LIMIT,
};

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
#[derive(Clone)]
pub struct SandboxedContext {
    pub sandbox: SandboxPolicy,
    pub shell_policy: ShellPolicy,
    pub network_policy: NetworkPolicy,
    pub permissions: Arc<V2PermissionService>,
    pub doom: Arc<DoomLoopDetector>,
}

impl SandboxedContext {
    pub fn new(sandbox: SandboxPolicy, permissions: Arc<V2PermissionService>) -> Self {
        let cwd = sandbox.project_root.clone();
        Self {
            sandbox,
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

    pub fn with_external_root(mut self, path: PathBuf) -> Self {
        self.sandbox = self.sandbox.with_external(path);
        self
    }
}

pub fn default_tools() -> Vec<Tool> {
    vec![
        Tool::new("read", "Read a file from disk"),
        Tool::new("write", "Write content to a file"),
        Tool::new("edit", "Replace text in an existing file"),
        Tool::new("apply_patch", "Apply a file-oriented patch"),
        Tool::new("glob", "Find files by glob-like pattern"),
        Tool::new("grep", "Search file contents"),
        Tool::new("shell", "Run a shell command"),
        Tool::new("task", "Run a subagent task"),
        Tool::new("skill", "Load a skill by name"),
        Tool::new("todo", "Update the session todo list"),
        Tool::new("question", "Ask the user a structured question"),
        Tool::new("webfetch", "Fetch a web URL"),
        Tool::new("websearch", "Search the web"),
        Tool::new("lsp", "Query language-server context"),
        Tool::new("plan", "Enter or exit planning mode"),
        Tool::new("external_directory", "Request external directory access"),
        Tool::new("invalid", "Report invalid tool calls"),
    ]
}

pub fn run_tool(call: &ToolCall) -> ToolResult {
    run_tool_with_context(call, &ToolContext::default())
}

pub fn run_tool_with_mode(call: &ToolCall, mode: PermissionMode) -> ToolResult {
    run_tool_with_context(call, &ToolContext::new(mode))
}

pub fn run_tool_with_context(call: &ToolCall, context: &ToolContext) -> ToolResult {
    if let Err(error) = PermissionService::new(context.mode).assert_tool_call(call) {
        return failure(&call.name, error.to_string());
    }

    match call.name.as_str() {
        "read" | "read_file" => read::run_with_context(call, context),
        "write" | "write_file" => write::run(call),
        "edit" => edit::run(call),
        "apply_patch" => apply_patch::run(call),
        "glob" | "search_files" => glob::run_with_context(call, context),
        "grep" => grep::run_with_context(call, context),
        "shell" | "bash" => shell::run(call),
        "task" => task::run(call),
        "ask_peer" => {
            failure("ask_peer", "ask_peer requires the async runtime; the agent runner routes this tool to `run_tool_async`".to_string())
        }
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
/// etc.). Falls back to the sync path for everything else. The agent
/// runner uses this for any tool whose name is `task` or `ask_peer`.
pub async fn run_tool_async(call: &ToolCall, runtime: &ToolRuntime) -> ToolResult {
    if let Err(error) = PermissionService::new(PermissionMode::Build).assert_tool_call(call) {
        return failure(&call.name, error.to_string());
    }

    match call.name.as_str() {
        "task" => task::run_async(call, runtime.clone()).await,
        "ask_peer" => ask_peer::run_async(call, runtime.clone()).await,
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

/// Run a tool with the new sandboxed context. This is the v2 path that goes
/// through the permission service, doom loop detector, and sandbox policies
/// (path containment for fs tools, env-scrubbed shell, SSRF-guarded network).
pub fn run_tool_sandboxed(call: &ToolCall, ctx: &SandboxedContext) -> ToolResult {
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
        "read" | "read_file" => read_sandboxed(call, ctx),
        "write" | "write_file" => write_sandboxed(call, ctx),
        "edit" => not_implemented_sandboxed("edit"),
        "apply_patch" => not_implemented_sandboxed("apply_patch"),
        "glob" | "search_files" => glob_sandboxed(call, ctx),
        "grep" => grep_sandboxed(call, ctx),
        "shell" | "bash" => shell_sandboxed(call, ctx),
        "webfetch" => webfetch_sandboxed(call, ctx),
        "websearch" => not_implemented_sandboxed("websearch"),
        "task" => failure(
            "task",
            "task requires the async runtime; the agent runner routes this tool to `run_tool_async`".to_string(),
        ),
        "ask_peer" => failure(
            "ask_peer",
            "ask_peer requires the async runtime; the agent runner routes this tool to `run_tool_async`".to_string(),
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

fn read_sandboxed(call: &ToolCall, ctx: &SandboxedContext) -> ToolResult {
    let path = string_arg(call, "path");
    if path.is_empty() {
        return failure("read", "path is required".to_string());
    }
    match ctx.sandbox.resolve(&path) {
        Ok(canonical) => read::run_with_path(call, &canonical),
        Err(e) => failure("read", format!("{e}")),
    }
}

fn write_sandboxed(call: &ToolCall, ctx: &SandboxedContext) -> ToolResult {
    let path = string_arg(call, "path");
    if path.is_empty() {
        return failure("write", "path is required".to_string());
    }
    match ctx.sandbox.resolve(&path) {
        Ok(canonical) => write::run_with_path(call, &canonical),
        Err(e) => failure("write", format!("{e}")),
    }
}

fn glob_sandboxed(call: &ToolCall, ctx: &SandboxedContext) -> ToolResult {
    let path = string_arg(call, "path");
    let target = if path.is_empty() {
        ctx.sandbox.project_root.clone()
    } else {
        match ctx.sandbox.resolve(&path) {
            Ok(p) => p,
            Err(e) => return failure("glob", format!("{e}")),
        }
    };
    glob::run_with_root(call, &target)
}

fn grep_sandboxed(call: &ToolCall, ctx: &SandboxedContext) -> ToolResult {
    let path = string_arg(call, "path");
    let target = if path.is_empty() {
        ctx.sandbox.project_root.clone()
    } else {
        match ctx.sandbox.resolve(&path) {
            Ok(p) => p,
            Err(e) => return failure("grep", format!("{e}")),
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

fn not_implemented_sandboxed(tool: &str) -> ToolResult {
    failure(
        tool,
        format!("{tool} sandboxed variant not implemented yet; using legacy path"),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use serde_json::json;

    use crate::permission::PermissionMode;
    use crate::permission_v2::{default_ruleset, PermissionService};
    use crate::ToolCall;

    use super::{run_tool_sandboxed, run_tool_with_mode, SandboxedContext};

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

    #[test]
    fn sandboxed_read_within_root_succeeds() {
        let (ctx, dir) = make_context();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hello").unwrap();
        let result = run_tool_sandboxed(&call("read", &[("path", file.to_str().unwrap())]), &ctx);
        assert!(result.success, "result: {:?}", result);
        assert!(result.output.contains("hello"));
    }

    #[test]
    fn sandboxed_read_outside_root_rejected() {
        let (ctx, _dir) = make_context();
        let result = run_tool_sandboxed(&call("read", &[("path", "/etc/passwd")]), &ctx);
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("outside"));
    }

    #[test]
    fn sandboxed_read_escapes_via_dotdot() {
        let (ctx, dir) = make_context();
        let escape = format!("{}/../etc/passwd", dir.path().display());
        let result = run_tool_sandboxed(&call("read", &[("path", escape.as_str())]), &ctx);
        assert!(!result.success);
    }

    #[test]
    fn sandboxed_write_creates_file() {
        let (ctx, dir) = make_context();
        let file = dir.path().join("new.txt");
        let result = run_tool_sandboxed(
            &call(
                "write",
                &[("path", file.to_str().unwrap()), ("content", "wrote")],
            ),
            &ctx,
        );
        assert!(result.success, "result: {:?}", result);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "wrote");
    }

    #[test]
    fn sandboxed_write_outside_root_rejected() {
        let (ctx, _dir) = make_context();
        let result = run_tool_sandboxed(
            &call(
                "write",
                &[("path", "/tmp/should-not-write.txt"), ("content", "x")],
            ),
            &ctx,
        );
        assert!(!result.success);
    }

    #[test]
    fn sandboxed_glob_within_root() {
        let (ctx, dir) = make_context();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();
        let result = run_tool_sandboxed(&call("glob", &[("pattern", "*.txt")]), &ctx);
        assert!(result.success, "result: {:?}", result);
        assert!(result.output.contains("a.txt"));
    }

    #[test]
    fn sandboxed_glob_with_dotdot_root_rejected() {
        let (ctx, _dir) = make_context();
        let result = run_tool_sandboxed(&call("glob", &[("path", "/etc"), ("pattern", "*")]), &ctx);
        assert!(!result.success);
    }

    #[test]
    fn sandboxed_grep_finds_match() {
        let (ctx, dir) = make_context();
        std::fs::write(dir.path().join("a.txt"), "the quick brown fox").unwrap();
        let result = run_tool_sandboxed(&call("grep", &[("pattern", "brown")]), &ctx);
        assert!(result.success);
        assert!(result.output.contains("brown"));
    }

    #[test]
    fn sandboxed_shell_runs_in_cwd() {
        let (ctx, dir) = make_context();
        let result = run_tool_sandboxed(&call("shell", &[("command", "pwd")]), &ctx);
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

    #[test]
    fn sandboxed_shell_captures_non_zero_exit() {
        let (ctx, _dir) = make_context();
        let result = run_tool_sandboxed(&call("shell", &[("command", "false")]), &ctx);
        assert!(!result.success);
    }

    #[test]
    fn sandboxed_doom_loop_blocks_repeats() {
        let (ctx, _dir) = make_context();
        let args = &[("command", "echo hi")][..];
        let c = || call("shell", args);
        // Two repeats are fine; the third triggers.
        assert!(run_tool_sandboxed(&c(), &ctx).success);
        assert!(run_tool_sandboxed(&c(), &ctx).success);
        let result = run_tool_sandboxed(&c(), &ctx);
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("doom"));
    }

    #[test]
    fn sandboxed_different_args_dont_trigger_doom() {
        let (ctx, _dir) = make_context();
        let result1 = run_tool_sandboxed(&call("shell", &[("command", "ls")]), &ctx);
        let result2 = run_tool_sandboxed(&call("shell", &[("command", "pwd")]), &ctx);
        let result3 = run_tool_sandboxed(&call("shell", &[("command", "echo hi")]), &ctx);
        assert!(result1.success);
        assert!(result2.success);
        assert!(result3.success);
    }

    #[test]
    fn sandboxed_webfetch_blocks_loopback() {
        let (ctx, _dir) = make_context();
        let result = run_tool_sandboxed(
            &call("webfetch", &[("url", "http://127.0.0.1:8080/secret")]),
            &ctx,
        );
        assert!(!result.success);
        assert!(result.error.as_ref().unwrap().contains("blocked"));
    }

    #[test]
    fn sandboxed_webfetch_allows_public_url() {
        let (ctx, _dir) = make_context();
        let result = run_tool_sandboxed(&call("webfetch", &[("url", "https://example.com")]), &ctx);
        assert!(result.success, "result: {:?}", result);
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
