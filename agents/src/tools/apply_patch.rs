use std::path::Path;

use crate::file_mutation::{FileMutationError, FileMutationService, FileTarget};
use crate::patch::{self, Hunk};
use crate::sandbox::{trigger_access, AccessDecision, AccessRequest};
use crate::{ToolCall, ToolResult};

use super::{
    failure, resolve_session_path, success_with_metadata, unified_diff, SandboxedContext,
    ToolContext,
};

pub fn run(call: &ToolCall) -> ToolResult {
    run_with_context(call, &ToolContext::default())
}

pub fn run_with_context(call: &ToolCall, ctx: &ToolContext) -> ToolResult {
    let patch_text = patch_text_arg(call);
    if patch_text.trim().is_empty() {
        return failure("apply_patch", "patchText is required".to_string());
    }

    let cwd = if ctx.project_root.as_os_str().is_empty() {
        None
    } else {
        Some(ctx.project_root.clone())
    };
    match apply_patch_in(&patch_text, cwd.as_deref()) {
        Ok(applied) => success_with_metadata(
            "apply_patch",
            model_output(&applied),
            diff_metadata(&applied),
        ),
        Err(error) => failure("apply_patch", error),
    }
}

pub fn apply_patch(patch_text: &str) -> Result<Vec<AppliedPatch>, String> {
    apply_patch_in(patch_text, None)
}

/// Sandboxed variant: every hunk's `path` is resolved through
/// the session's `SandboxPolicy`. A hunk whose path resolves
/// outside `project_root` (and any `external_roots`) is rejected
/// before any file is touched. If the failure is a
/// `PathContainmentError::ExternalAccess` we route the user
/// through `trigger_access` to add the parent directory to
/// `external_roots` and continue. This is the path the v2
/// production runner takes.
pub async fn apply_patch_sandboxed(
    patch_text: &str,
    ctx: &SandboxedContext,
) -> Result<Vec<AppliedPatch>, String> {
    let sandbox = &ctx.sandbox;
    let cwd = ctx.project_root();
    let hunks = patch::parse(patch_text)
        .map_err(|error| format!("apply_patch verification failed: {error}"))?;
    if hunks.is_empty() {
        return Err("patch rejected: empty patch".to_string());
    }
    if hunks.iter().any(|hunk| {
        matches!(
            hunk,
            Hunk::Update {
                move_path: Some(_),
                ..
            }
        )
    }) {
        return Err("apply_patch moves are not supported yet".to_string());
    }

    // Pre-flight: validate every hunk path through the sandbox
    // before we touch any file. This avoids partial-apply
    // failures when the patch is partly out-of-root. If a hunk
    // path triggers an `external_access` ask, the user can
    // approve the parent directory and we keep going.
    for hunk in &hunks {
        let path = match hunk {
            Hunk::Add { path, .. } | Hunk::Delete { path, .. } | Hunk::Update { path, .. } => path,
        };
        if sandbox.lock().resolve(path).is_ok() {
            continue;
        }
        let initial_error = sandbox.lock().resolve(path).unwrap_err();
        if !crate::sandbox::should_trigger_ask(&initial_error) {
            log_sandbox_reject("apply_patch", path, &initial_error);
            return Err(format!(
                "apply_patch: hunk path '{path}' is outside the sandbox (project_root='{}'): {initial_error}",
                ctx.project_root().display()
            ));
        }
        match trigger_access(
            sandbox,
            &ctx.permissions,
            AccessRequest {
                requested: path,
                tool: "apply_patch",
            },
        )
        .await
        {
            Ok(AccessDecision::Allowed { .. }) => {
                tracing::info!(
                    tool = "apply_patch",
                    requested = %path,
                    "sandbox access approved via trigger_access"
                );
            }
            Ok(AccessDecision::Rejected { message }) => {
                return Err(format!(
                    "apply_patch: user rejected external access for '{path}': {message}"
                ));
            }
            Err(error) => return Err(error),
        }
    }

    let mutation = FileMutationService::new();
    let mut prepared = Vec::new();
    for hunk in hunks {
        let prep = prepare_hunk_sandboxed(&mutation, hunk, &cwd, ctx).await?;
        prepared.push(prep);
    }

    let mut applied = Vec::new();
    for change in prepared {
        let result = apply_prepared(&mutation, &change)
            .map_err(|error| partial_error(&applied, change.path(), error))?;
        applied.push(result);
    }

    Ok(applied)
}

fn log_sandbox_reject(tool: &str, requested: &str, error: &crate::sandbox::PathContainmentError) {
    let process_cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    tracing::warn!(
        tool,
        requested = %requested,
        process_cwd = %process_cwd,
        inside_project_root = false,
        sandbox_allowed = false,
        error = %error,
        "sandbox path resolution rejected (structural error, no ask)"
    );
}

async fn prepare_hunk_sandboxed(
    mutation: &FileMutationService,
    hunk: Hunk,
    cwd: &Path,
    ctx: &SandboxedContext,
) -> Result<PreparedPatch, String> {
    let _ = cwd; // cwd is implicit in the sandbox resolve
    let path_str = match &hunk {
        Hunk::Add { path, .. } | Hunk::Delete { path, .. } | Hunk::Update { path, .. } => path,
    };
    let canonical = resolve_hunk_path(path_str, ctx).await?;
    let target = mutation
        .target(&canonical)
        .map_err(|error| error.to_string())?;
    match hunk {
        Hunk::Add { contents, .. } => {
            let content = if contents.ends_with('\n') || contents.is_empty() {
                contents
            } else {
                format!("{contents}\n")
            };
            Ok(PreparedPatch::Add {
                target,
                content: content.into_bytes(),
            })
        }
        Hunk::Delete { .. } => {
            require_file(&target)?;
            let expected = std::fs::read(&target.canonical).map_err(|error| error.to_string())?;
            Ok(PreparedPatch::Delete { target, expected })
        }
        Hunk::Update { path, chunks, .. } => {
            require_file(&target)?;
            let expected = std::fs::read(&target.canonical).map_err(|error| error.to_string())?;
            let original = String::from_utf8(expected.clone())
                .map_err(|_| format!("{} is not valid UTF-8", target.resource))?;
            let update =
                patch::derive(&path, &chunks, &original).map_err(|error| error.to_string())?;
            Ok(PreparedPatch::Update {
                path,
                target,
                expected,
                content: patch::join_bom(&update.content, update.bom).into_bytes(),
            })
        }
    }
}

async fn resolve_hunk_path(
    raw: &str,
    ctx: &SandboxedContext,
) -> Result<std::path::PathBuf, String> {
    let process_cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    if let Ok(canonical) = ctx.sandbox.lock().resolve(raw) {
        tracing::info!(
            tool = "apply_patch",
            requested = %raw,
            project_root = %ctx.sandbox.lock().project_root.display(),
            resolved = %canonical.display(),
            process_cwd = %process_cwd,
            inside_project_root = canonical.starts_with(&ctx.sandbox.lock().project_root),
            sandbox_allowed = true,
            "sandbox path resolution allowed"
        );
        return Ok(canonical);
    }
    let initial_error = ctx.sandbox.lock().resolve(raw).unwrap_err();
    if !crate::sandbox::should_trigger_ask(&initial_error) {
        tracing::warn!(
            tool = "apply_patch",
            requested = %raw,
            project_root = %ctx.sandbox.lock().project_root.display(),
            process_cwd = %process_cwd,
            inside_project_root = false,
            sandbox_allowed = false,
            error = %initial_error,
            "sandbox path resolution rejected (structural error, no ask)"
        );
        return Err(format!("{initial_error}"));
    }
    match trigger_access(
        &ctx.sandbox,
        &ctx.permissions,
        AccessRequest {
            requested: raw,
            tool: "apply_patch",
        },
    )
    .await
    {
        Ok(AccessDecision::Allowed { path, .. }) => {
            tracing::info!(
                tool = "apply_patch",
                requested = %raw,
                resolved = %path.display(),
                process_cwd = %process_cwd,
                sandbox_allowed = true,
                "sandbox path resolution allowed (after ask)"
            );
            Ok(path)
        }
        Ok(AccessDecision::Rejected { message }) => {
            tracing::warn!(
                tool = "apply_patch",
                requested = %raw,
                process_cwd = %process_cwd,
                sandbox_allowed = false,
                "sandbox access rejected by user"
            );
            Err(message)
        }
        Err(error) => Err(error),
    }
}

/// `cwd`, when set, anchors any relative `path` in the patch
/// hunks. When `None` (legacy behavior), the process cwd is
/// used, which is almost always wrong for a Tauri app.
pub fn apply_patch_in(patch_text: &str, cwd: Option<&Path>) -> Result<Vec<AppliedPatch>, String> {
    let hunks = patch::parse(patch_text)
        .map_err(|error| format!("apply_patch verification failed: {error}"))?;
    if hunks.is_empty() {
        return Err("patch rejected: empty patch".to_string());
    }
    if hunks.iter().any(|hunk| {
        matches!(
            hunk,
            Hunk::Update {
                move_path: Some(_),
                ..
            }
        )
    }) {
        return Err("apply_patch moves are not supported yet".to_string());
    }

    let mutation = FileMutationService::new();
    let mut prepared = Vec::new();
    for hunk in hunks {
        prepared.push(prepare_hunk_in(&mutation, hunk, cwd)?);
    }

    let mut applied = Vec::new();
    for change in prepared {
        let result = apply_prepared(&mutation, &change)
            .map_err(|error| partial_error(&applied, change.path(), error))?;
        applied.push(result);
    }

    Ok(applied)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedPatch {
    pub kind: AppliedPatchKind,
    pub resource: String,
    pub target: String,
    pub diff: String,
    pub additions: usize,
    pub deletions: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppliedPatchKind {
    Add,
    Update,
    Delete,
}

enum PreparedPatch {
    Add {
        target: FileTarget,
        content: Vec<u8>,
    },
    Update {
        path: String,
        target: FileTarget,
        expected: Vec<u8>,
        content: Vec<u8>,
    },
    Delete {
        target: FileTarget,
        expected: Vec<u8>,
    },
}

impl PreparedPatch {
    fn path(&self) -> &str {
        match self {
            PreparedPatch::Add { target, .. } => &target.resource,
            PreparedPatch::Update { path, .. } => path,
            PreparedPatch::Delete { target, .. } => &target.resource,
        }
    }
}

pub(crate) fn patch_text_arg(call: &ToolCall) -> String {
    for key in ["patchText", "patch_text", "patch"] {
        if let Some(value) = call.arguments.get(key).and_then(|value| value.as_str()) {
            return value.to_string();
        }
    }
    String::new()
}

fn prepare_hunk_in(
    mutation: &FileMutationService,
    hunk: Hunk,
    cwd: Option<&Path>,
) -> Result<PreparedPatch, String> {
    let resolve = |raw: &str| -> Result<FileTarget, String> {
        if let Some(cwd) = cwd {
            let resolved = resolve_session_path(
                raw,
                &ToolContext::default().with_project_root(cwd.to_path_buf()),
                "apply_patch",
            );
            mutation
                .target_in(&resolved, cwd)
                .map_err(|error| error.to_string())
        } else {
            mutation
                .target(Path::new(raw))
                .map_err(|error| error.to_string())
        }
    };
    match hunk {
        Hunk::Add { path, contents } => {
            let target = resolve(&path)?;
            let content = if contents.ends_with('\n') || contents.is_empty() {
                contents
            } else {
                format!("{contents}\n")
            };
            Ok(PreparedPatch::Add {
                target,
                content: content.into_bytes(),
            })
        }
        Hunk::Delete { path } => {
            let target = resolve(&path)?;
            require_file(&target)?;
            let expected = std::fs::read(&target.canonical).map_err(|error| error.to_string())?;
            Ok(PreparedPatch::Delete { target, expected })
        }
        Hunk::Update { path, chunks, .. } => {
            let target = resolve(&path)?;
            require_file(&target)?;
            let expected = std::fs::read(&target.canonical).map_err(|error| error.to_string())?;
            let original = String::from_utf8(expected.clone())
                .map_err(|_| format!("{} is not valid UTF-8", target.resource))?;
            let update =
                patch::derive(&path, &chunks, &original).map_err(|error| error.to_string())?;
            Ok(PreparedPatch::Update {
                path,
                target,
                expected,
                content: patch::join_bom(&update.content, update.bom).into_bytes(),
            })
        }
    }
}

fn apply_prepared(
    mutation: &FileMutationService,
    change: &PreparedPatch,
) -> Result<AppliedPatch, FileMutationError> {
    match change {
        PreparedPatch::Add { target, content } => {
            let diff = diff_for_bytes(&target.resource, None, Some(content));
            let result = mutation.create(target, content)?;
            Ok(applied(
                AppliedPatchKind::Add,
                result.resource,
                result.target,
                diff,
            ))
        }
        PreparedPatch::Update {
            target,
            expected,
            content,
            ..
        } => {
            let diff = diff_for_bytes(&target.resource, Some(expected), Some(content));
            let result = mutation.write_if_unmodified(target, expected, content)?;
            Ok(applied(
                AppliedPatchKind::Update,
                result.resource,
                result.target,
                diff,
            ))
        }
        PreparedPatch::Delete { target, expected } => {
            let diff = diff_for_bytes(&target.resource, Some(expected), None);
            let result = mutation.remove(target)?;
            Ok(applied(
                AppliedPatchKind::Delete,
                result.resource,
                result.target,
                diff,
            ))
        }
    }
}

fn require_file(target: &FileTarget) -> Result<(), String> {
    let metadata = std::fs::metadata(&target.canonical).map_err(|error| error.to_string())?;
    if metadata.is_file() {
        Ok(())
    } else {
        Err(format!("{} is not a file", target.resource))
    }
}

fn applied(
    kind: AppliedPatchKind,
    resource: String,
    target: std::path::PathBuf,
    diff: super::ToolDiff,
) -> AppliedPatch {
    AppliedPatch {
        kind,
        resource,
        target: target.to_string_lossy().to_string(),
        diff: diff.diff,
        additions: diff.additions,
        deletions: diff.deletions,
    }
}

fn diff_for_bytes(path: &str, before: Option<&[u8]>, after: Option<&[u8]>) -> super::ToolDiff {
    let before = before.and_then(|value| std::str::from_utf8(value).ok());
    let after = after.and_then(|value| std::str::from_utf8(value).ok());
    unified_diff(path, before, after)
}

pub(crate) fn diff_metadata(applied: &[AppliedPatch]) -> serde_json::Value {
    serde_json::json!({
        "diff": applied.iter().map(|item| item.diff.as_str()).collect::<Vec<_>>().join("\n"),
        "additions": applied.iter().map(|item| item.additions).sum::<usize>(),
        "deletions": applied.iter().map(|item| item.deletions).sum::<usize>(),
        "files": applied.iter().map(|item| {
            serde_json::json!({
                "file": item.resource,
                "diff": item.diff,
                "additions": item.additions,
                "deletions": item.deletions,
            })
        }).collect::<Vec<_>>(),
    })
}

fn partial_error(applied: &[AppliedPatch], path: &str, error: FileMutationError) -> String {
    if applied.is_empty() {
        return format!("Unable to apply patch at {path}: {error}");
    }
    format!(
        "Patch partially applied before failing at {path}: {error}. Applied: {}",
        applied
            .iter()
            .map(|item| item.resource.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

pub(crate) fn model_output(applied: &[AppliedPatch]) -> String {
    let mut lines = vec!["Applied patch sequentially:".to_string()];
    lines.extend(applied.iter().map(|item| {
        let status = match item.kind {
            AppliedPatchKind::Add => "A",
            AppliedPatchKind::Update => "M",
            AppliedPatchKind::Delete => "D",
        };
        format!("{status} {}", item.resource)
    }));
    lines.join("\n")
}
