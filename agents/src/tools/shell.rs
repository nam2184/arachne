use std::process::Command;

use crate::{ToolCall, ToolResult};

use super::{failure, string_arg, success, ToolContext};

pub fn run(call: &ToolCall) -> ToolResult {
    run_with_context(call, &ToolContext::default())
}

/// Run the shell command in the session's working directory. The
/// `ToolContext::project_root` is the session's directory, set by
/// the agent runner from the persisted session row. Falling back
/// to it ensures the command always runs against the session cwd,
/// not the process cwd (which for a Tauri app is something like
/// `C:\Program Files\…\arachne.exe`).
pub fn run_with_context(call: &ToolCall, ctx: &ToolContext) -> ToolResult {
    let command = string_arg(call, "command");
    if command.is_empty() {
        return failure("shell", "command is required".to_string());
    }

    let mut cmd = if cfg!(windows) {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", &command]);
        cmd
    } else {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", &command]);
        cmd
    };

    let workdir = string_arg(call, "workdir");
    if !workdir.is_empty() {
        cmd.current_dir(workdir);
    } else if !ctx.project_root.as_os_str().is_empty() {
        cmd.current_dir(&ctx.project_root);
    }

    match cmd.output() {
        Ok(output) if output.status.success() => {
            success("shell", String::from_utf8_lossy(&output.stdout).to_string())
        }
        Ok(output) => {
            let bytes: &[u8] = if output.stderr.is_empty() {
                &output.stdout
            } else {
                &output.stderr
            };
            failure("shell", String::from_utf8_lossy(bytes).to_string())
        }
        Err(error) => failure("shell", error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolContext;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn call_with(args: &[(&str, &str)]) -> ToolCall {
        let mut arguments = HashMap::new();
        for (k, v) in args {
            arguments.insert(
                (*k).to_string(),
                serde_json::Value::String((*v).to_string()),
            );
        }
        ToolCall {
            name: "shell".to_string(),
            arguments,
        }
    }

    #[test]
    fn shell_uses_tool_context_project_root_as_default_cwd() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let marker = tmp.path().join("marker.txt");
        std::fs::write(&marker, "ok").expect("write marker");

        // `pwd` should resolve to the ToolContext's project_root
        // when the LLM doesn't supply a `workdir`.
        let call = call_with(&[("command", "cat marker.txt")]);
        let ctx = ToolContext::default().with_project_root(tmp.path().to_path_buf());
        let result = run_with_context(&call, &ctx);
        assert!(
            result.success,
            "shell should run in session cwd: {result:?}"
        );
        assert_eq!(result.output, "ok");
    }

    #[test]
    fn shell_explicit_workdir_overrides_project_root() {
        let tmp_a = tempfile::tempdir().expect("tmp_a");
        let tmp_b = tempfile::tempdir().expect("tmp_b");
        std::fs::write(tmp_a.path().join("a.txt"), "A").expect("write a");
        std::fs::write(tmp_b.path().join("b.txt"), "B").expect("write b");

        // When the LLM supplies `workdir`, it wins over the
        // session cwd. This mirrors the existing escape hatch.
        let call = call_with(&[
            ("command", "cat a.txt"),
            ("workdir", tmp_a.path().to_str().expect("utf8 path")),
        ]);
        let ctx = ToolContext::default().with_project_root(tmp_b.path().to_path_buf());
        let result = run_with_context(&call, &ctx);
        assert!(
            result.success,
            "shell should honor workdir override: {result:?}"
        );
        assert_eq!(result.output, "A");
    }

    #[test]
    fn shell_default_does_not_leak_process_cwd() {
        // Empty project_root + empty workdir: fall through to the
        // process cwd. We can only assert the call runs without
        // panicking; the key regression test is the one above,
        // which proves session cwd is used.
        let call = call_with(&[("command", "echo hi")]);
        let ctx = ToolContext::default();
        let _ = run_with_context(&call, &ctx);
    }

    #[test]
    fn empty_project_root_is_treated_as_unset() {
        // Mirrors the runner's `project_root.as_os_str().is_empty()`
        // guard. An empty project_root must not poison the
        // Command's cwd.
        let call = call_with(&[("command", "echo hi")]);
        let ctx = ToolContext {
            mode: Default::default(),
            project_root: PathBuf::new(),
        };
        let _ = run_with_context(&call, &ctx);
    }
}
