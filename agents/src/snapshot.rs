use serde::{Deserialize, Serialize};
use std::collections::{hash_map::DefaultHasher, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;

use crate::AgentSession;

const LARGE_FILE_LIMIT: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct SessionFileDiff {
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    pub additions: u32,
    pub deletions: u32,
    pub status: DiffStatus,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiffStatus {
    Added,
    Deleted,
    Modified,
}

pub struct SnapshotService {
    base_dir: PathBuf,
    locks: Arc<SnapshotLocks>,
}

impl SnapshotService {
    pub fn new(base_dir: PathBuf) -> Arc<Self> {
        static LOCKS: OnceLock<Arc<SnapshotLocks>> = OnceLock::new();

        Arc::new(Self {
            base_dir,
            locks: LOCKS
                .get_or_init(|| Arc::new(SnapshotLocks::default()))
                .clone(),
        })
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    pub fn track(&self, session: &AgentSession) -> Option<String> {
        let state = SnapshotState::new(&self.base_dir, session);
        if !state.worktree.join(".git").exists() {
            return None;
        }
        let lock = self.locks.lock_for(&state.worktree);
        let _guard = lock.lock();
        match state
            .ensure_repo()
            .and_then(|_| state.stage())
            .and_then(|_| {
                let result = state.git(["write-tree"])?;
                Ok(result.trim().to_string())
            }) {
            Ok(hash) if !hash.is_empty() => Some(hash),
            Ok(_) => None,
            Err(error) => {
                tracing::debug!(session_id = %session.id, error = %error, "snapshot tracking skipped");
                None
            }
        }
    }

    pub fn diff_full(&self, session: &AgentSession, from: &str, to: &str) -> Vec<SessionFileDiff> {
        let state = SnapshotState::new(&self.base_dir, session);
        match state.diff_full(from, to) {
            Ok(diff) => diff,
            Err(error) => {
                tracing::debug!(session_id = %session.id, error = %error, "snapshot diff skipped");
                Vec::new()
            }
        }
    }
}

#[derive(Default)]
struct SnapshotLocks {
    locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

impl SnapshotLocks {
    fn lock_for(&self, worktree: &Path) -> Arc<Mutex<()>> {
        let key = snapshot_lock_key(worktree);
        let mut locks = self.locks.lock();
        locks.entry(key).or_default().clone()
    }
}

struct SnapshotState {
    git_dir: PathBuf,
    worktree: PathBuf,
}

impl SnapshotState {
    fn new(base_dir: &Path, session: &AgentSession) -> Self {
        Self {
            git_dir: base_dir
                .join(&session.project_id)
                .join(hash_path(&session.directory)),
            worktree: PathBuf::from(&session.directory),
        }
    }

    fn ensure_repo(&self) -> Result<(), String> {
        if self.git_dir.join("HEAD").exists() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.git_dir).map_err(|error| error.to_string())?;
        self.run_git(["init"], None)?;
        self.run_git(["config", "core.autocrlf", "false"], None)?;
        self.run_git(["config", "core.longpaths", "true"], None)?;
        self.run_git(["config", "core.quotepath", "false"], None)?;
        Ok(())
    }

    fn stage(&self) -> Result<(), String> {
        let files = self.changed_files()?;
        let allowed = files
            .into_iter()
            .filter(|file| !self.is_large_untracked(file))
            .collect::<Vec<_>>();
        if allowed.is_empty() {
            return Ok(());
        }
        self.git_with_stdin(
            [
                "add",
                "--all",
                "--sparse",
                "--pathspec-from-file=-",
                "--pathspec-file-nul",
            ],
            &nul_list(&allowed),
        )?;
        Ok(())
    }

    fn changed_files(&self) -> Result<Vec<String>, String> {
        let tracked = self
            .git(["diff-files", "--name-only", "-z", "--", "."])?
            .split('\0')
            .filter(|item| !item.is_empty())
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let untracked = self
            .git([
                "ls-files",
                "--others",
                "--exclude-standard",
                "-z",
                "--",
                ".",
            ])?
            .split('\0')
            .filter(|item| !item.is_empty())
            .map(ToString::to_string)
            .collect::<Vec<_>>();

        let mut all = tracked;
        for file in untracked {
            if !all.contains(&file) {
                all.push(file);
            }
        }
        Ok(all)
    }

    fn is_large_untracked(&self, file: &str) -> bool {
        std::fs::metadata(self.worktree.join(file))
            .map(|metadata| metadata.is_file() && metadata.len() > LARGE_FILE_LIMIT)
            .unwrap_or(false)
    }

    fn diff_full(&self, from: &str, to: &str) -> Result<Vec<SessionFileDiff>, String> {
        let status_output = self.git([
            "diff",
            "--no-ext-diff",
            "--name-status",
            "--no-renames",
            from,
            to,
            "--",
            ".",
        ])?;
        let numstat_output = self.git([
            "diff",
            "--no-ext-diff",
            "--no-renames",
            "--numstat",
            from,
            to,
            "--",
            ".",
        ])?;
        let statuses = status_output
            .lines()
            .filter_map(|line| {
                let (code, file) = line.split_once('\t')?;
                Some((file.to_string(), status_from_code(code)))
            })
            .collect::<std::collections::HashMap<_, _>>();

        let mut diffs = Vec::new();
        for line in numstat_output.lines() {
            let mut parts = line.split('\t');
            let Some(additions) = parts.next() else {
                continue;
            };
            let Some(deletions) = parts.next() else {
                continue;
            };
            let Some(file) = parts.next() else { continue };
            let binary = additions == "-" && deletions == "-";
            diffs.push(SessionFileDiff {
                file: file.to_string(),
                patch: if binary {
                    None
                } else {
                    self.file_patch(from, to, file).ok()
                },
                additions: additions.parse().unwrap_or(0),
                deletions: deletions.parse().unwrap_or(0),
                status: statuses.get(file).copied().unwrap_or(DiffStatus::Modified),
            });
        }
        Ok(diffs)
    }

    fn file_patch(&self, from: &str, to: &str, file: &str) -> Result<String, String> {
        self.git([
            "diff",
            "--no-ext-diff",
            "--no-renames",
            from,
            to,
            "--",
            file,
        ])
        .map(|patch| patch.trim().to_string())
    }

    fn git<I, S>(&self, args: I) -> Result<String, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.run_git(args, None)
    }

    fn git_with_stdin<I, S>(&self, args: I, stdin: &str) -> Result<String, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.run_git(args, Some(stdin))
    }

    fn run_git<I, S>(&self, args: I, stdin: Option<&str>) -> Result<String, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut command = Command::new("git");
        command
            .arg("-c")
            .arg("core.autocrlf=false")
            .arg("-c")
            .arg("core.longpaths=true")
            .arg("-c")
            .arg("core.quotepath=false")
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.worktree)
            .current_dir(&self.worktree)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for arg in args {
            command.arg(arg.as_ref());
        }
        if stdin.is_some() {
            command.stdin(Stdio::piped());
        }

        let mut child = command.spawn().map_err(|error| error.to_string())?;
        if let Some(input) = stdin {
            use std::io::Write;
            if let Some(mut child_stdin) = child.stdin.take() {
                child_stdin
                    .write_all(input.as_bytes())
                    .map_err(|error| error.to_string())?;
            }
        }
        let output = child
            .wait_with_output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

fn status_from_code(code: &str) -> DiffStatus {
    if code.starts_with('A') {
        DiffStatus::Added
    } else if code.starts_with('D') {
        DiffStatus::Deleted
    } else {
        DiffStatus::Modified
    }
}

fn hash_path(path: &str) -> String {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

fn snapshot_lock_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn nul_list(files: &[String]) -> String {
    let mut out = files.join("\0");
    out.push('\0');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn session(dir: &Path) -> AgentSession {
        AgentSession {
            id: "s".to_string(),
            project_id: "p".to_string(),
            directory: dir.display().to_string(),
            provider: "test".to_string(),
            model: "test".to_string(),
            title: None,
            group_id: None,
            summary_json: None,
            parent_session_id: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn tracks_and_diffs_untracked_file() {
        let worktree = tempfile::tempdir().unwrap();
        Command::new("git")
            .arg("init")
            .current_dir(worktree.path())
            .output()
            .unwrap();
        let snapshots = tempfile::tempdir().unwrap();
        let service = SnapshotService::new(snapshots.path().to_path_buf());
        let session = session(worktree.path());
        let before = service.track(&session).unwrap();
        std::fs::write(worktree.path().join("hello.txt"), "hi\n").unwrap();
        let after = service.track(&session).unwrap();
        let diff = service.diff_full(&session, &before, &after);
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].file, "hello.txt");
        assert_eq!(diff[0].status, DiffStatus::Added);
        assert!(diff[0].patch.as_deref().unwrap_or_default().contains("+hi"));
    }
}
