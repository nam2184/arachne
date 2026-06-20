use std::collections::HashMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTarget {
    pub canonical: PathBuf,
    pub resource: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteResult {
    pub operation: FileOperation,
    pub target: PathBuf,
    pub resource: String,
    pub existed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOperation {
    Write,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileMutationError {
    Io { path: PathBuf, message: String },
    TargetExists { path: PathBuf },
    StaleContent { path: PathBuf },
}

impl fmt::Display for FileMutationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FileMutationError::Io { path, message } => {
                write!(formatter, "{}: {message}", path.display())
            }
            FileMutationError::TargetExists { path } => {
                write!(formatter, "target already exists: {}", path.display())
            }
            FileMutationError::StaleContent { path } => {
                write!(formatter, "file changed before write: {}", path.display())
            }
        }
    }
}

impl std::error::Error for FileMutationError {}

#[derive(Clone)]
pub struct FileMutationService {
    locks: Arc<KeyedLocks>,
}

impl Default for FileMutationService {
    fn default() -> Self {
        Self::new()
    }
}

impl FileMutationService {
    pub fn new() -> Self {
        static LOCKS: OnceLock<Arc<KeyedLocks>> = OnceLock::new();

        Self {
            locks: LOCKS
                .get_or_init(|| Arc::new(KeyedLocks::default()))
                .clone(),
        }
    }

    pub fn target(&self, path: impl AsRef<Path>) -> Result<FileTarget, FileMutationError> {
        FileTarget::resolve(path.as_ref())
    }

    /// Resolve a path with an explicit session cwd. Relative
    /// paths are anchored to `cwd` instead of the process cwd.
    /// This is what the v1 tool runner uses so file mutations
    /// in a session land in the session's directory, not the
    /// Tauri install dir.
    pub fn target_in(
        &self,
        path: impl AsRef<Path>,
        cwd: &Path,
    ) -> Result<FileTarget, FileMutationError> {
        FileTarget::resolve_with_cwd(path.as_ref(), cwd)
    }

    pub fn create(
        &self,
        target: &FileTarget,
        content: impl AsRef<[u8]>,
    ) -> Result<WriteResult, FileMutationError> {
        let lock = self.locks.lock_for(&target.canonical);
        let _guard = lock.lock();
        if let Some(parent) = target.canonical.parent() {
            fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target.canonical)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    FileMutationError::TargetExists {
                        path: target.canonical.clone(),
                    }
                } else {
                    io_error(&target.canonical, error)
                }
            })?;
        file.write_all(content.as_ref())
            .map_err(|error| io_error(&target.canonical, error))?;
        Ok(write_result(target, FileOperation::Write, false))
    }

    pub fn write(
        &self,
        target: &FileTarget,
        content: impl AsRef<[u8]>,
    ) -> Result<WriteResult, FileMutationError> {
        let lock = self.locks.lock_for(&target.canonical);
        let _guard = lock.lock();
        let existed = target.canonical.exists();
        if let Some(parent) = target.canonical.parent() {
            fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
        }
        fs::write(&target.canonical, content)
            .map_err(|error| io_error(&target.canonical, error))?;
        Ok(write_result(target, FileOperation::Write, existed))
    }

    pub fn write_text_preserving_bom(
        &self,
        target: &FileTarget,
        content: &str,
    ) -> Result<WriteResult, FileMutationError> {
        let lock = self.locks.lock_for(&target.canonical);
        let _guard = lock.lock();
        let current = match fs::read(&target.canonical) {
            Ok(content) => Some(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(io_error(&target.canonical, error)),
        };
        let next = split_utf8_bom(content);
        let content = join_utf8_bom(
            next.text.as_bytes(),
            current.as_deref().is_some_and(has_utf8_bom) || next.bom,
        );
        if let Some(parent) = target.canonical.parent() {
            fs::create_dir_all(parent).map_err(|error| io_error(parent, error))?;
        }
        fs::write(&target.canonical, content)
            .map_err(|error| io_error(&target.canonical, error))?;
        Ok(write_result(
            target,
            FileOperation::Write,
            current.is_some(),
        ))
    }

    pub fn write_if_unmodified(
        &self,
        target: &FileTarget,
        expected: &[u8],
        content: impl AsRef<[u8]>,
    ) -> Result<WriteResult, FileMutationError> {
        let lock = self.locks.lock_for(&target.canonical);
        let _guard = lock.lock();
        let current =
            fs::read(&target.canonical).map_err(|error| io_error(&target.canonical, error))?;
        if current != expected {
            return Err(FileMutationError::StaleContent {
                path: target.canonical.clone(),
            });
        }
        fs::write(&target.canonical, content)
            .map_err(|error| io_error(&target.canonical, error))?;
        Ok(write_result(target, FileOperation::Write, true))
    }

    pub fn remove(&self, target: &FileTarget) -> Result<WriteResult, FileMutationError> {
        let lock = self.locks.lock_for(&target.canonical);
        let _guard = lock.lock();
        match fs::remove_file(&target.canonical) {
            Ok(()) => Ok(write_result(target, FileOperation::Remove, true)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(write_result(target, FileOperation::Remove, false))
            }
            Err(error) => Err(io_error(&target.canonical, error)),
        }
    }
}

impl FileTarget {
    pub fn resolve(path: &Path) -> Result<Self, FileMutationError> {
        Self::resolve_with_cwd(path, Path::new(""))
    }

    /// Like `resolve` but anchors relative paths to `cwd`
    /// instead of the process cwd. An empty `cwd` preserves the
    /// legacy behavior (process-cwd fallback for relative
    /// paths). The `cwd` is expected to be a session
    /// `project_root` set by the runner.
    pub fn resolve_with_cwd(path: &Path, cwd: &Path) -> Result<Self, FileMutationError> {
        let resource = path.to_string_lossy().to_string();
        if resource.trim().is_empty() {
            return Err(FileMutationError::Io {
                path: path.to_path_buf(),
                message: "path is required".to_string(),
            });
        }
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else if !cwd.as_os_str().is_empty() {
            cwd.join(path)
        } else {
            // No cwd available: fall back to the canonical
            // process-cwd behavior, which is what older code
            // did.
            path.to_path_buf()
        };
        Ok(Self {
            canonical: canonical_path(&absolute)?,
            resource,
        })
    }
}

#[derive(Default)]
struct KeyedLocks {
    locks: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

impl KeyedLocks {
    fn lock_for(&self, path: &Path) -> Arc<Mutex<()>> {
        let mut locks = self.locks.lock();
        locks.entry(path.to_path_buf()).or_default().clone()
    }
}

struct SplitBom<'a> {
    bom: bool,
    text: &'a str,
}

fn canonical_path(path: &Path) -> Result<PathBuf, FileMutationError> {
    if let Ok(canonical) = path.canonicalize() {
        return Ok(canonical);
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| io_error(Path::new("."), error))?
            .join(path)
    };
    Ok(normalize_path(&absolute))
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn write_result(target: &FileTarget, operation: FileOperation, existed: bool) -> WriteResult {
    WriteResult {
        operation,
        target: target.canonical.clone(),
        resource: target.resource.clone(),
        existed,
    }
}

fn split_utf8_bom(text: &str) -> SplitBom<'_> {
    let text_without_bom = text.trim_start_matches('\u{FEFF}');
    SplitBom {
        bom: text_without_bom.len() != text.len(),
        text: text_without_bom,
    }
}

fn join_utf8_bom(content: &[u8], bom: bool) -> Vec<u8> {
    if !bom {
        return content.to_vec();
    }
    let mut output = vec![0xef, 0xbb, 0xbf];
    output.extend_from_slice(strip_utf8_bom(content));
    output
}

fn strip_utf8_bom(content: &[u8]) -> &[u8] {
    if has_utf8_bom(content) {
        &content[3..]
    } else {
        content
    }
}

fn has_utf8_bom(content: &[u8]) -> bool {
    content.starts_with(&[0xef, 0xbb, 0xbf])
}

fn io_error(path: &Path, error: std::io::Error) -> FileMutationError {
    FileMutationError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{FileMutationError, FileMutationService};

    #[test]
    fn detects_stale_content() {
        let service = FileMutationService::new();
        let path = std::env::temp_dir().join(format!(
            "arachne-stale-{}.txt",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let target = service.target(&path).unwrap();
        service.write(&target, b"first").unwrap();
        fs::write(&path, b"second").unwrap();
        let error = service
            .write_if_unmodified(&target, b"first", b"third")
            .unwrap_err();
        assert!(matches!(error, FileMutationError::StaleContent { .. }));
        let _ = fs::remove_file(path);
    }
}
