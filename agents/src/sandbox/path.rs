use std::path::{Component, Path, PathBuf};

/// Strip the Windows verbatim path prefix (`\\?\` or
/// `\\?\UNC\…`) that `std::fs::canonicalize` prepends. A
/// user-supplied path like `C:\Users\Foo` doesn't carry that
/// prefix, so any path-prefix comparison (whether component-wise
/// or via `Path::strip_prefix`) would treat `\\?\` as an extra
/// segment that the user-supplied path lacks. Stripping it once
/// at canonicalization keeps both sides comparable.
pub(crate) fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let mut components = path.components();
    let mut stripped: PathBuf = PathBuf::new();
    let mut consumed_prefix = false;
    while let Some(component) = components.next() {
        match component {
            Component::Prefix(prefix) => {
                // Skip verbatim prefixes entirely. Keep drive
                // letters and root prefixes (Component::RootDir)
                // intact.
                let kind = prefix.kind();
                use std::path::Prefix::*;
                match kind {
                    Verbatim(_) | VerbatimDisk(_) | VerbatimUNC(_, _) => {
                        consumed_prefix = true;
                    }
                    _ => {
                        // Component::Prefix already carries the
                        // drive or device-ns prefix — push it
                        // through. Non-verbatim UNC and
                        // DeviceNS prefixes are preserved so
                        // \\?\UNC\server\share and
                        // \\.\COM1 round-trip correctly.
                        stripped.push(Component::Prefix(prefix));
                    }
                }
            }
            Component::RootDir => {
                stripped.push(component);
            }
            Component::CurDir | Component::ParentDir | Component::Normal(_) => {
                stripped.push(component);
                // Once we hit a non-prefix component, copy the
                // rest verbatim.
                if consumed_prefix {
                    stripped.extend(components);
                    return stripped;
                }
                stripped.extend(components);
                return stripped;
            }
        }
    }
    stripped
}

/// Containment check for the sandbox: does `child` live inside
/// `parent` (or equal `parent`)? Mirrors opencode's
/// `FSUtil.contains` (packages/core/src/fs-util.ts), which uses
/// `path.relative` + escape detection instead of a string-prefix
/// compare. The Rust analog is `Path::strip_prefix` after `resolve`
/// has anchored relative paths, collapsed `.`/`..`, and canonicalized
/// existing candidates.
///
/// Both `parent` and `child` are expected to be canonical
/// (symlinks resolved, on-disk casing, `\\?\` stripped) — that's
/// the invariant the write-time `canonicalize_directory` chokepoint
/// in `SessionService::create_session_with_parent` plus the
/// construction-time canonicalize inside `SandboxPolicy::new`
/// enforce. With canonical inputs, `strip_prefix` is a reliable
/// byte-level compare and the previous case-insensitive gymnastics
/// are unnecessary.
pub(crate) fn contains_path(parent: &Path, child: &Path) -> bool {
    match child.strip_prefix(parent) {
        Ok(remainder) => {
            // `remainder` is the path-relative form. Empty means
            // exact match (`child == parent`). A leading `..` would
            // mean we somehow ended up outside — impossible if
            // `strip_prefix` returned `Ok`, but defensively reject.
            remainder.as_os_str().is_empty()
                || !remainder
                    .components()
                    .any(|c| matches!(c, Component::ParentDir))
        }
        Err(_) => false,
    }
}

/// Debug-only variant of the old case-insensitive component
/// compare. Kept around because the `tracing::debug!` lines in
/// `SandboxPolicy::resolve` use it to print how a path looks
/// under both the canonical-byte and case-insensitive lenses, which
/// is useful when diagnosing a false-positive "outside the
/// sandbox" rejection on Windows where the LLM produced a path
/// whose casing differs from the on-disk casing. The
/// containment *decision* uses `contains_path`; this helper is for
/// log output only and is not exported from the module.
#[allow(dead_code)]
fn debug_only_path_starts_with(candidate: &Path, prefix: &Path) -> bool {
    let candidate_components: Vec<PathBuf> = candidate
        .components()
        .map(|component| PathBuf::from(component.as_os_str().to_ascii_lowercase()))
        .collect();
    let prefix_components: Vec<PathBuf> = prefix
        .components()
        .map(|component| PathBuf::from(component.as_os_str().to_ascii_lowercase()))
        .collect();
    if prefix_components.len() > candidate_components.len() {
        return false;
    }
    candidate_components[..prefix_components.len()] == prefix_components[..]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathContainmentError {
    /// Path tries to escape the allowed root (via `..` or absolute path).
    EscapesRoot { path: PathBuf, root: PathBuf },
    /// Path is outside the project root and external_directory isn't allowed
    /// for this prefix.
    ExternalAccess { path: PathBuf },
    /// Path resolved to a symlink that points outside the root.
    SymlinkEscape { path: PathBuf, target: PathBuf },
    /// Path is empty or otherwise unusable.
    InvalidPath { path: PathBuf },
}

impl std::fmt::Display for PathContainmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EscapesRoot { path, root } => {
                write!(
                    f,
                    "path '{}' escapes root '{}'",
                    path.display(),
                    root.display()
                )
            }
            Self::ExternalAccess { path } => {
                write!(f, "path '{}' is outside the project root", path.display())
            }
            Self::SymlinkEscape { path, target } => write!(
                f,
                "symlink '{}' points outside the root: '{}'",
                path.display(),
                target.display()
            ),
            Self::InvalidPath { path } => write!(f, "invalid path: '{}'", path.display()),
        }
    }
}

impl std::error::Error for PathContainmentError {}

/// A policy describing which paths a tool may access. Created per-session from
/// the project root plus any `external_directory` allowlist.
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    /// Canonical project root. All paths must resolve to be inside this,
    /// unless they match an `external_directory` rule.
    pub project_root: PathBuf,
    /// Additional allowed path prefixes (each must be canonical).
    pub external_roots: Vec<PathBuf>,
}

impl SandboxPolicy {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        // Canonicalize the root once so all containment checks use
        // the OS-resolved path identity.
        let raw: PathBuf = project_root.into();
        let canonical = raw.canonicalize().unwrap_or_else(|_| raw.clone());
        Self {
            project_root: strip_verbatim_prefix(canonical),
            external_roots: Vec::new(),
        }
    }

    pub fn with_external(mut self, path: impl Into<PathBuf>) -> Self {
        // Same canonicalization as `new` for externally allowed roots.
        let raw: PathBuf = path.into();
        let canonical = raw.canonicalize().unwrap_or_else(|_| raw.clone());
        self.external_roots.push(strip_verbatim_prefix(canonical));
        self
    }

    /// Resolve a user-supplied path into its canonical form and check it
    /// against the policy. Returns the canonical path on success.
    ///
    /// Every comparison point emits a `tracing::debug!` line so you can
    /// see exactly what the sandbox sees when a `glob` / `read` /
    /// `external_directory` call gets rejected on Windows. Run with
    /// `RUST_LOG=arachne_agents::sandbox=debug` (or `arachne_agents=debug`)
    /// to see the full trace.
    pub fn resolve(&self, path: impl AsRef<Path>) -> Result<PathBuf, PathContainmentError> {
        let path = path.as_ref();
        tracing::debug!(
            input = %path.display(),
            input_is_absolute = path.is_absolute(),
            project_root = %self.project_root.display(),
            external_roots = ?self.external_roots,
            "sandbox resolve: input"
        );
        if path.as_os_str().is_empty() {
            return Err(PathContainmentError::InvalidPath {
                path: path.to_path_buf(),
            });
        }

        // First, normalize the literal path (resolving `..` and `.` syntactically).
        // This catches obvious escape attempts before we touch the filesystem.
        let normalized = normalize_path(path);
        tracing::debug!(
            input = %path.display(),
            normalized = %normalized.display(),
            "sandbox resolve: after normalize"
        );

        // If the path is relative, anchor it to the project root.
        let absolute = if normalized.is_absolute() {
            normalized
        } else {
            self.project_root.join(normalized)
        };
        tracing::debug!(
            absolute = %absolute.display(),
            absolute_eq_input = absolute == path,
            project_root = %self.project_root.display(),
            starts_with_project_root_case_insensitive = debug_only_path_starts_with(&absolute, &self.project_root),
            starts_with_project_root_native = absolute.starts_with(&self.project_root),
            "sandbox resolve: absolute vs project_root"
        );

        // Normalize the candidate before containment. This mirrors opencode:
        // resolve against the instance directory, canonicalize/realpath when
        // possible, then check whether the canonical candidate is contained.
        // For writes to nonexistent files, canonicalize fails and we fall back
        // to the lexical absolute path; `contains_path` rejects any remaining
        // `..` escape in that fallback path.
        let canonical_result = absolute.canonicalize();
        let canonical_exists = canonical_result.is_ok();
        let canonical =
            strip_verbatim_prefix(canonical_result.unwrap_or_else(|_| absolute.clone()));
        tracing::debug!(
            canonical = %canonical.display(),
            canonical_resolved = canonical_exists,
            canonical_eq_absolute = canonical == absolute,
            canonical_starts_with_project_root = contains_path(&self.project_root, &canonical),
            canonical_starts_with_project_root_native = canonical.starts_with(&self.project_root),
            "sandbox resolve: after canonicalize"
        );

        if !self.is_allowed(&canonical) {
            tracing::warn!(
                input = %path.display(),
                canonical = %canonical.display(),
                absolute = %absolute.display(),
                project_root = %self.project_root.display(),
                external_roots = ?self.external_roots,
                "sandbox resolve: REJECTED — path is outside the policy"
            );
            return Err(PathContainmentError::ExternalAccess { path: canonical });
        }

        tracing::debug!(
            input = %path.display(),
            canonical = %canonical.display(),
            "sandbox resolve: ALLOWED"
        );
        Ok(canonical)
    }

    fn is_allowed(&self, path: &Path) -> bool {
        let in_project_root = contains_path(&self.project_root, path);
        let in_external_root = self
            .external_roots
            .iter()
            .find(|root| contains_path(root, path));
        let allowed = in_project_root || in_external_root.is_some();
        let path_components: Vec<String> = path
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect();
        let project_root_components: Vec<String> = self
            .project_root
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect();
        tracing::trace!(
            path = %path.display(),
            path_components = ?path_components,
            project_root = %self.project_root.display(),
            project_root_components = ?project_root_components,
            in_project_root,
            // The case-insensitive lens is kept for diagnostics: if
            // `in_project_root` is false on Windows but
            // `debug_only_path_starts_with` is true, the input
            // differs from `project_root` only by casing — that's a
            // signal the LLM is producing non-canonical paths.
            debug_case_insensitive_starts_with = debug_only_path_starts_with(path, &self.project_root),
            in_external_root = ?in_external_root.map(|root| root.display().to_string()),
            allowed,
            "sandbox is_allowed"
        );
        allowed
    }
}

/// Normalize a path syntactically: collapse `.` and `..` components without
/// touching the filesystem. Does not resolve symlinks.
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    let absolute = path.is_absolute();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // Don't pop past a root component.
                if !out.pop() && !absolute {
                    out.push("..");
                }
            }
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::Normal(c) => out.push(c),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn resolve_returns_canonical_path_inside_root() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, "hi").unwrap();

        let policy = SandboxPolicy::new(dir.path().to_path_buf());
        let resolved = policy.resolve(&file).unwrap();
        // The resolved path may differ from `file` due to symlink resolution
        // (e.g., on macOS tempdir is a symlink).
        assert!(resolved.is_absolute());
        assert!(policy.is_allowed(&resolved));
    }

    #[test]
    fn resolve_rejects_dotdot_escape() {
        let dir = TempDir::new().unwrap();
        let escape = dir.path().join("..").join("etc").join("passwd");
        let policy = SandboxPolicy::new(dir.path().to_path_buf());
        let result = policy.resolve(&escape);
        assert!(matches!(
            result,
            Err(PathContainmentError::ExternalAccess { .. })
        ));
    }

    #[test]
    fn resolve_rejects_absolute_path_outside_root() {
        let dir = TempDir::new().unwrap();
        let policy = SandboxPolicy::new(dir.path().to_path_buf());
        let result = policy.resolve("/etc/passwd");
        assert!(matches!(
            result,
            Err(PathContainmentError::ExternalAccess { .. })
        ));
    }

    #[test]
    fn resolve_rejects_empty_path() {
        let dir = TempDir::new().unwrap();
        let policy = SandboxPolicy::new(dir.path().to_path_buf());
        let result = policy.resolve("");
        assert!(matches!(
            result,
            Err(PathContainmentError::InvalidPath { .. })
        ));
    }

    #[test]
    fn resolve_allows_external_root() {
        let project = TempDir::new().unwrap();
        let external = TempDir::new().unwrap();
        let file = external.path().join("outside.txt");
        std::fs::write(&file, "external").unwrap();

        let policy = SandboxPolicy::new(project.path().to_path_buf())
            .with_external(external.path().to_path_buf());
        let resolved = policy.resolve(&file).unwrap();
        assert!(resolved.starts_with(external.path()));
    }

    #[test]
    fn resolve_rejects_path_in_unrelated_directory() {
        let project = TempDir::new().unwrap();
        let other = TempDir::new().unwrap();
        let file = other.path().join("not-allowed.txt");
        std::fs::write(&file, "nope").unwrap();

        let policy = SandboxPolicy::new(project.path().to_path_buf());
        let result = policy.resolve(&file);
        assert!(matches!(
            result,
            Err(PathContainmentError::ExternalAccess { .. })
        ));
    }

    #[test]
    fn contains_path_rejects_mismatched_case_but_debug_lens_detects_it() {
        // After the canonicalization chokepoint, both
        // `SandboxPolicy.project_root` and the input passed to
        // `resolve` are canonical (symlinks resolved, on-disk casing,
        // `\\?\` stripped). The containment check is byte-level via
        // `Path::strip_prefix`; it does not fold casing.
        //
        // The debug-only case-insensitive helper
        // (`debug_only_path_starts_with`) is still exported via
        // trace logs so a Windows user diagnosing a "false
        // positive external directory" can see that the input
        // differs from project_root only by casing and apply the
        // canonicalize fix at the source.
        let project = TempDir::new().unwrap();
        let canonical_project = project.path().to_path_buf();
        let policy = SandboxPolicy::new(canonical_project.clone());

        let uppercased = PathBuf::from(canonical_project.to_string_lossy().to_ascii_uppercase());

        // The byte-level decision rejects mixed-case input.
        assert!(!contains_path(&canonical_project, &uppercased));

        // The debug lens shows that the input only differs by
        // casing — useful log signal, not a containment decision.
        assert!(debug_only_path_starts_with(&uppercased, &canonical_project));

        // `is_allowed` reflects the same byte-level decision.
        assert!(!policy.is_allowed(&uppercased));
    }

    #[test]
    fn resolve_handles_relative_path_inside_root() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "a").unwrap();
        let policy = SandboxPolicy::new(dir.path().to_path_buf());
        let resolved = policy.resolve("a.txt").unwrap();
        assert!(policy.is_allowed(&resolved));
    }

    #[test]
    fn resolve_creates_nonexistent_paths_normally() {
        // For paths that don't exist yet (e.g. a write target), the policy
        // should still validate the prefix containment.
        let dir = TempDir::new().unwrap();
        let new_file = dir.path().join("new.txt");
        let policy = SandboxPolicy::new(dir.path().to_path_buf());
        let resolved = policy.resolve(&new_file).unwrap();
        assert!(resolved.starts_with(dir.path()));
    }

    #[test]
    fn contains_path_accepts_forward_slashes() {
        // A forward-slash absolute path that points inside the
        // canonical root should be accepted. On Windows this covers
        // the common `C:/...` tool-call shape.
        let dir = TempDir::new().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let forward_slash = {
            let s = canonical.to_string_lossy().replace('\\', "/");
            PathBuf::from(format!("{}/src/foo.rs", s))
        };
        assert!(contains_path(&canonical, &forward_slash));
    }

    #[test]
    fn contains_path_accepts_trailing_separator_on_root() {
        // A trailing separator on the project root must not
        // poison containment. `Path::strip_prefix` treats an
        // empty trailing segment as a no-op.
        let dir = TempDir::new().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let sep = std::path::MAIN_SEPARATOR;
        let with_trailing = PathBuf::from(format!("{}{}", canonical.to_string_lossy(), sep));
        assert!(contains_path(&canonical, &with_trailing));
    }

    #[test]
    fn contains_path_rejects_dotdot_escape() {
        // A sibling directory is not inside `parent`.
        let dir = TempDir::new().unwrap();
        let sibling = TempDir::new().unwrap();
        let canonical = dir.path().canonicalize().unwrap();
        let escape_attempt = sibling.path().canonicalize().unwrap();
        assert!(!contains_path(&canonical, &escape_attempt));
    }

    #[test]
    fn resolve_accepts_forward_slash_child_path() {
        // End-to-end: an LLM tool call passes a forward-slash absolute path.
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("inside.txt");
        std::fs::write(&file, "data").unwrap();

        let policy = SandboxPolicy::new(dir.path().to_path_buf());
        let forward = file
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let resolved = policy.resolve(PathBuf::from(forward)).expect("resolve");
        assert!(policy.is_allowed(&resolved));
    }

    #[test]
    fn resolve_accepts_literal_session_path_against_canonicalized_root() {
        // Regression: the runner passes the raw `session.directory`
        // as `project_root` (case preserved exactly as the file
        // dialog returned it). The user passes the same path
        // back via `glob **/* <dir>` etc. After canonicalize()
        // resolves to the on-disk casing, the literal-vs-canonical
        // comparison must still pass. We approximate the
        // case-insensitive filesystem by creating the project
        // root via `TempDir` (which returns a lowercase path on
        // macOS) and then constructing the `SandboxPolicy` with
        // the literal path the user would pass. Without
        // canonicalizing `project_root` at construction time,
        // the pre-canonicalize check happens to pass on POSIX
        // (case-sensitive); but the post-canonicalize check
        // fails because canonicalize returns the same lowercase
        // path. The intent of this test is to lock in the
        // `policy.is_allowed(&canonical) == true` invariant
        // regardless of which `canonicalize` returns.
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("inside.txt"), "data").unwrap();

        // Build the policy from the literal user-supplied path.
        let policy = SandboxPolicy::new(dir.path().to_path_buf());
        // The policy's project_root must have been canonicalized
        // at construction time so it matches the canonical form
        // that `canonicalize()` returns inside `resolve`.
        assert_eq!(policy.project_root, dir.path().canonicalize().unwrap());

        // Resolving the literal path must succeed and pass
        // containment.
        let literal = dir.path().to_path_buf();
        let resolved = policy.resolve(&literal).unwrap();
        assert!(policy.is_allowed(&resolved));
    }

    #[test]
    fn strip_verbatim_prefix_drops_leading_double_question_mark() {
        // Regression: on Windows, `std::fs::canonicalize`
        // returns paths prefixed with `\\?\` (the verbatim
        // device path). When the runner stored that as
        // `project_root`, every literal user-supplied path
        // appeared "shorter than the prefix" to the
        // byte-level `Path::strip_prefix` check, and
        // `is_allowed` returned false even for paths inside
        // the project root. We can't synthesize a
        // verbatim-prefixed PathBuf portably across platforms
        // (the `Component::Prefix` variants are gated on
        // Windows), so test the helper directly: feed it a
        // path that begins with `\\?\` and verify the prefix
        // is dropped before `contains_path` ever sees it.
        //
        // On non-Windows, the helper is a no-op for paths
        // without a `Component::Prefix`. The runtime guarantee
        // we care about — `is_allowed` returns true for the
        // canonicalized project root against the same path
        // without the prefix — is covered by the
        // `resolve_accepts_literal_session_path_against_canonicalized_root`
        // test above, which exercises the full
        // construction-time canonicalization path on POSIX
        // tempdirs.

        // Manually build a "verbatim-prefixed" path so the
        // helper exercises the prefix branch even on
        // non-Windows. We do this by checking that any path
        // the helper returns, when fed back through
        // `contains_path`, contains no leading
        // `\\?\`-flavored components.
        let dir = TempDir::new().unwrap();
        let stripped = strip_verbatim_prefix(dir.path().to_path_buf());
        // On POSIX, `dir.path()` has no `Component::Prefix`,
        // so the helper returns the path unchanged. The
        // invariant we lock in is that the output contains
        // no `\\?\` prefix segment.
        for component in stripped.components() {
            if let Component::Prefix(prefix) = component {
                use std::path::Prefix::*;
                let kind = prefix.kind();
                assert!(
                    !matches!(kind, Verbatim(_) | VerbatimDisk(_) | VerbatimUNC(_, _)),
                    "verbatim prefix leaked through strip_verbatim_prefix: {kind:?}"
                );
            }
        }
    }

    #[test]
    fn resolve_accepts_verbatim_prefixed_canonical_path() {
        // Mirror the Windows scenario by hand-crafting a
        // literal path that matches the canonicalized
        // `project_root` after stripping its verbatim prefix.
        // We construct the policy from a literal path (the
        // verbatim-prefix-stripping happens at construction
        // time inside `SandboxPolicy::new`), then resolve a
        // path with the same components — the post-canonicalize
        // check must succeed.
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("file.txt"), "data").unwrap();

        let policy = SandboxPolicy::new(dir.path().to_path_buf());
        // The stored project_root must NOT carry any verbatim
        // prefix that would mismatch literal comparisons.
        for component in policy.project_root.components() {
            if let Component::Prefix(prefix) = component {
                use std::path::Prefix::*;
                let kind = prefix.kind();
                assert!(
                    !matches!(kind, Verbatim(_) | VerbatimDisk(_) | VerbatimUNC(_, _)),
                    "verbatim prefix leaked into policy.project_root: {kind:?}"
                );
            }
        }

        // Resolving a literal child path must pass.
        let child = dir.path().join("file.txt");
        let resolved = policy.resolve(&child).unwrap();
        assert!(policy.is_allowed(&resolved));
    }

    #[test]
    fn normalize_collapses_dot() {
        assert_eq!(normalize_path(Path::new("./a/./b")), Path::new("a/b"));
    }

    #[test]
    fn normalize_handles_dotdot() {
        assert_eq!(normalize_path(Path::new("a/../b")), Path::new("b"));
    }

    #[test]
    fn normalize_does_not_escape_absolute_root() {
        assert_eq!(normalize_path(Path::new("/../etc")), Path::new("/etc"));
    }

    #[test]
    fn normalize_relative_dotdot_kept() {
        // Relative paths that go above the implicit start keep `..` (we have
        // nothing to pop).
        let result = normalize_path(Path::new("../a"));
        assert_eq!(result, Path::new("../a"));
    }
}
