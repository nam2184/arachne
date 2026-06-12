use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ShellPolicy {
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    pub max_output_bytes: usize,
}

impl ShellPolicy {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            timeout: Duration::from_secs(120),
            env: std::env::vars_os().collect(),
            max_output_bytes: 100_000,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_env(mut self, env: Vec<(std::ffi::OsString, std::ffi::OsString)>) -> Self {
        self.env = env;
        self
    }

    pub fn with_max_output_bytes(mut self, n: usize) -> Self {
        self.max_output_bytes = n;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellExit {
    Success,
    NonZero(i32),
    TimedOut,
    SpawnFailed,
    Killed,
}

#[derive(Debug, Clone)]
pub struct ShellOutput {
    pub exit: ShellExit,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone)]
pub enum ShellError {
    Sandbox(String),
}

impl std::fmt::Display for ShellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sandbox(msg) => write!(f, "sandbox violation: {msg}"),
        }
    }
}

impl std::error::Error for ShellError {}

/// Run a shell command under the policy. The command is executed via
/// `/bin/sh -c` (or `cmd /C` on Windows). The policy's cwd is enforced
/// regardless of what the LLM asked for. Output is truncated to
/// `max_output_bytes`. The timeout is enforced via a watchdog thread that
/// kills the child if it exceeds the budget.
pub fn run_shell(command: &str, policy: &ShellPolicy) -> Result<ShellOutput, ShellError> {
    if command.trim().is_empty() {
        return Err(ShellError::Sandbox("empty command".to_string()));
    }

    let mut cmd = if cfg!(windows) {
        let mut cmd = Command::new("cmd");
        cmd.args(["/C", command]);
        cmd
    } else {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", command]);
        cmd
    };
    cmd.current_dir(&policy.cwd);
    // Critical: pipe stdout/stderr so we can capture them. Without this,
    // the child inherits the parent's fds and wait_with_output reads nothing.
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    // Replace the entire env with the policy's.
    cmd.env_clear();
    for (k, v) in &policy.env {
        cmd.env(k, v);
    }

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Ok(ShellOutput {
                exit: ShellExit::SpawnFailed,
                stdout: String::new(),
                stderr: format!("failed to spawn shell: {e}"),
            });
        }
    };

    let (tx, rx) = mpsc::channel::<()>();
    let child_pid = child.id();

    // Watchdog: kill the child if the timeout elapses.
    let timeout = policy.timeout;
    let watchdog = thread::spawn(move || {
        if rx.recv_timeout(timeout).is_ok() {
            return;
        }
        // Timeout: best-effort kill. The child may have already exited.
        #[cfg(unix)]
        {
            use std::process::Command as Cmd;
            let _ = Cmd::new("kill")
                .arg("-9")
                .arg(child_pid.to_string())
                .status();
        }
        #[cfg(windows)]
        {
            use std::process::Command as Cmd;
            let _ = Cmd::new("taskkill")
                .args(["/F", "/PID", &child_pid.to_string()])
                .status();
        }
    });

    let output = match child.wait_with_output() {
        Ok(out) => out,
        Err(e) => {
            let _ = tx.send(());
            return Ok(ShellOutput {
                exit: ShellExit::SpawnFailed,
                stdout: String::new(),
                stderr: format!("failed to wait on child: {e}"),
            });
        }
    };
    let _ = tx.send(());

    // Join the watchdog so it doesn't leak.
    let _ = watchdog.join();

    let stdout = truncate(
        &String::from_utf8_lossy(&output.stdout),
        policy.max_output_bytes,
    );
    let stderr = truncate(
        &String::from_utf8_lossy(&output.stderr),
        policy.max_output_bytes,
    );

    let exit = match output.status.code() {
        Some(0) => ShellExit::Success,
        Some(code) => ShellExit::NonZero(code),
        None => ShellExit::Killed,
    };
    Ok(ShellOutput {
        exit,
        stdout,
        stderr,
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut out = s[..max].to_string();
    out.push_str("\n...[truncated]");
    out
}

/// Sanity-check that the policy is sensible. Returns Err if cwd doesn't
/// exist (which would make every shell call fail).
pub fn validate(policy: &ShellPolicy) -> Result<(), ShellError> {
    if !policy.cwd.exists() {
        return Err(ShellError::Sandbox(format!(
            "cwd '{}' does not exist",
            policy.cwd.display()
        )));
    }
    if !policy.cwd.is_dir() {
        return Err(ShellError::Sandbox(format!(
            "cwd '{}' is not a directory",
            policy.cwd.display()
        )));
    }
    Ok(())
}

// (No more helper types needed; the channel is just `()`.)

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn policy_for(dir: &Path) -> ShellPolicy {
        ShellPolicy::new(dir).with_timeout(Duration::from_secs(10))
    }

    #[test]
    fn runs_simple_command() {
        let dir = tempfile::tempdir().unwrap();
        let out = run_shell("echo hello", &policy_for(dir.path())).unwrap();
        assert_eq!(out.exit, ShellExit::Success);
        assert!(out.stdout.contains("hello"));
    }

    #[test]
    fn enforces_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let policy = policy_for(dir.path());
        let out = run_shell("pwd", &policy).unwrap();
        // pwd returns the canonicalized path; both forms should be acceptable.
        let actual = std::fs::canonicalize(dir.path()).unwrap();
        assert!(
            out.stdout.trim() == actual.to_string_lossy()
                || out.stdout.trim() == dir.path().to_string_lossy()
        );
    }

    #[test]
    fn captures_non_zero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let out = run_shell("false", &policy_for(dir.path())).unwrap();
        assert_eq!(out.exit, ShellExit::NonZero(1));
    }

    #[test]
    fn captures_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let out = run_shell("echo error >&2; exit 3", &policy_for(dir.path())).unwrap();
        assert_eq!(out.exit, ShellExit::NonZero(3));
        assert!(out.stderr.contains("error"));
    }

    #[test]
    fn times_out_long_running_command() {
        let dir = tempfile::tempdir().unwrap();
        let policy = policy_for(dir.path()).with_timeout(Duration::from_millis(300));
        // `sleep 5` should exceed the 300ms timeout.
        let out = run_shell("sleep 5", &policy).unwrap();
        // The watchdog should kill the child; the result is either NonZero
        // (killed signal) or TimedOut (depending on platform timing).
        assert!(
            matches!(
                out.exit,
                ShellExit::Killed | ShellExit::NonZero(_) | ShellExit::TimedOut
            ),
            "expected kill/timeout, got {:?}",
            out.exit
        );
    }

    #[test]
    fn empty_command_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_shell("   ", &policy_for(dir.path()));
        assert!(result.is_err());
    }

    #[test]
    fn truncates_long_output() {
        let dir = tempfile::tempdir().unwrap();
        let policy = policy_for(dir.path()).with_max_output_bytes(10);
        let out = run_shell("yes | head -n 100", &policy).unwrap();
        assert!(out.stdout.contains("[truncated]"));
    }

    #[test]
    fn env_scrubbing_removes_secrets() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("OPENAI_API_KEY_FOR_TEST", "sk-should-not-leak");
        let env: Vec<_> = std::env::vars_os()
            .filter(|(k, _)| k.to_string_lossy() != "OPENAI_API_KEY_FOR_TEST")
            .chain(std::iter::once((
                std::ffi::OsString::from("OPENAI_API_KEY_FOR_TEST"),
                std::ffi::OsString::from("sk-should-not-leak"),
            )))
            .collect();
        let policy = policy_for(dir.path()).with_env(crate::sandbox::env::scrub_env(&env));
        let out = run_shell("env | grep OPENAI || true", &policy).unwrap();
        assert!(!out.stdout.contains("sk-should-not-leak"));
        std::env::remove_var("OPENAI_API_KEY_FOR_TEST");
    }

    #[test]
    fn validate_rejects_nonexistent_cwd() {
        let policy = ShellPolicy::new("/nonexistent/path/abc");
        assert!(validate(&policy).is_err());
    }

    #[test]
    fn validate_accepts_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let policy = policy_for(dir.path());
        assert!(validate(&policy).is_ok());
    }
}
