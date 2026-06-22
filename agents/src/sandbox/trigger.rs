//! UI ask flow for out-of-sandbox path access.
//!
//! Mirrors opencode's `assertExternalDirectoryEffect` (tool/external-directory.ts):
//! when a tool resolves a path that lives outside the session's project root,
//! we ask the user for an `external_directory` allow, and on approval we
//! promote the relevant directory into the sandbox's `external_roots` so
//! subsequent calls in the same session pass without re-asking.
//!
//! The agents crate doesn't depend on Tauri. The UI ask is exposed via the
//! v2 `PermissionService` which publishes the `PermissionRequest` over an
//! `mpsc::UnboundedSender`. The Tauri command `permission_reply` in
//! `src-tauri/src/commands/permission_commands.rs` already drains that
//! channel and forwards replies to the `oneshot` reply channel this
//! function awaits on. From the user's perspective, the workflow is:
//!
//! 1. Tool resolves a path → `SandboxPolicy::resolve` fails with
//!    `PathContainmentError::ExternalAccess`.
//! 2. This function canonicalizes the requested path, stats it to decide
//!    which directory to promote (the dir itself for a directory target,
//!    the parent for a file target, the parent when the target doesn't
//!    exist yet, e.g. a new file), and posts an `external_directory`
//!    permission request whose pattern is `<dir>/**`.
//! 3. The Tauri layer surfaces the request in the UI (the existing
//!    `permission-changed` event already carries the `session_id`).
//! 4. The user clicks Allow once / Always / Reject in the UI; the Tauri
//!    command `permission_reply` invokes `PermissionService::reply`.
//! 5. `trigger_access` resumes: on Allow/Always, it canonicalizes the
//!    chosen directory and pushes it into `external_roots` so the same
//!    access succeeds on the next try without a prompt.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::permission_v2::{
    CheckError, CheckOutcome, CheckRequest, PermissionAction, PermissionService,
};
use crate::sandbox::path::PathContainmentError;
use crate::sandbox::SandboxPolicy;

/// A request to the user for out-of-sandbox access.
///
/// `trigger_access` figures out which directory to add to
/// `external_roots` itself by canonicalizing + stat'ing the requested
/// path; the caller just supplies the path and the originating tool
/// (used in the permission request's `tool` field and the log lines).
#[derive(Debug, Clone)]
pub struct AccessRequest<'a> {
    pub requested: &'a str,
    pub tool: &'a str,
}

/// Outcome of `trigger_access`. On `Allowed`, the chosen directory has
/// been added to the policy's `external_roots` and the canonical
/// resolved path is returned.
#[derive(Debug)]
pub enum AccessDecision {
    /// User approved (Once or Always). `path` is the canonical
    /// resolved path the caller should use. `always` is true when
    /// the user picked "Always" — callers may want to skip future
    /// asks for the same shape, though the v2 service already
    /// persists the rule.
    Allowed { path: PathBuf, always: bool },
    /// User rejected the access.
    Rejected { message: String },
}

/// Try to resolve `requested` against the sandbox policy. If it
/// passes, return immediately. If it fails because the path is
/// outside the project root (and not in `external_roots`), post an
/// `external_directory` permission ask to the user. On approval,
/// promote the parent directory (for files) or the directory itself
/// (for directory targets) into `external_roots` and re-resolve.
pub async fn trigger_access(
    policy: &Arc<Mutex<SandboxPolicy>>,
    permissions: &PermissionService,
    request: AccessRequest<'_>,
) -> Result<AccessDecision, String> {
    // 1. Try the existing policy first.
    {
        let policy_guard = policy.lock();
        if let Ok(canonical) = policy_guard.resolve(request.requested) {
            return Ok(AccessDecision::Allowed {
                path: canonical,
                always: false,
            });
        }
    }

    // 2. The path is outside the sandbox. Compute the absolute
    //    target, canonicalize it, and pick the right directory to
    //    promote. The LLM never tells us whether a path is a file
    //    or a directory, so we stat it ourselves. If the file
    //    doesn't exist yet (e.g. `write` creating a new file), the
    //    canonicalize falls back to the absolute path and we use
    //    its parent.
    let project_root = policy.lock().project_root.clone();
    let absolute = if Path::new(request.requested).is_absolute() {
        PathBuf::from(request.requested)
    } else {
        project_root.join(request.requested)
    };
    let canonical_target = absolute.canonicalize().unwrap_or(absolute);
    let promote_dir = pick_promote_dir(&canonical_target);

    let pattern = format!("{}/**", promote_dir.display());

    tracing::info!(
        tool = %request.tool,
        requested = %request.requested,
        target_dir = %promote_dir.display(),
        project_root = %project_root.display(),
        pattern = %pattern,
        "sandbox access ask: path is outside project root; requesting external_directory"
    );

    let check = permissions
        .check_async(CheckRequest {
            permission: "external_directory".to_string(),
            pattern: pattern.clone(),
            tool: request.tool.to_string(),
            always: vec![pattern.clone()],
            request_id: None,
        })
        .await;

    match check {
        Ok(CheckOutcome::Allowed) => {
            // The v2 service stores an `allow` rule when the user
            // replied `Always`; for `Once` it just lets the call
            // through this once. We additionally promote the
            // directory into the sandbox's `external_roots` so the
            // caller can re-resolve the same path (or a sibling)
            // without re-asking.
            let is_always = permissions
                .base_ruleset()
                .evaluate("external_directory", &pattern)
                .action
                == PermissionAction::Allow
                || permissions.approved_rule_count() > 0;
            {
                let mut policy_guard = policy.lock();
                if !policy_guard
                    .external_roots
                    .iter()
                    .any(|root| root == &promote_dir)
                {
                    policy_guard.external_roots.push(promote_dir.clone());
                }
            }
            let resolved = policy.lock().resolve(request.requested).map_err(|e| {
                format!(
                    "external_directory approved for {} but re-resolve failed: {e}",
                    promote_dir.display()
                )
            })?;
            tracing::info!(
                tool = %request.tool,
                requested = %request.requested,
                target_dir = %promote_dir.display(),
                resolved = %resolved.display(),
                is_always,
                "sandbox access approved; promoted directory into external_roots"
            );
            Ok(AccessDecision::Allowed {
                path: resolved,
                always: is_always,
            })
        }
        Err(CheckError::Denied {
            permission,
            pattern: _,
        }) => Err(format!(
            "sandbox access denied by policy: {permission} for path '{}'",
            request.requested
        )),
        Err(CheckError::Rejected { .. }) => Ok(AccessDecision::Rejected {
            message: format!(
                "user rejected external_directory access for '{}'",
                request.requested
            ),
        }),
        Err(CheckError::NotFound { .. }) => Ok(AccessDecision::Rejected {
            message: format!("external_directory ask dropped for '{}'", request.requested),
        }),
    }
}

/// Pick the directory to add to `external_roots` for a given
/// canonicalized target. If the target itself is a directory, add
/// the target. Otherwise (file, or doesn't exist), add the parent.
fn pick_promote_dir(canonical: &Path) -> PathBuf {
    if canonical.is_dir() {
        canonical.to_path_buf()
    } else {
        canonical
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| canonical.to_path_buf())
    }
}

/// Convenience: surface the original `PathContainmentError` if the
/// path isn't an "outside project root" error. Used to distinguish
/// "this path is malformed" (which we shouldn't ask about) from "this
/// path is outside the project root" (which we should).
pub fn should_trigger_ask(err: &PathContainmentError) -> bool {
    matches!(err, PathContainmentError::ExternalAccess { .. })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission_v2::{default_ruleset, UserReply};
    use crate::sandbox::SandboxPolicy;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn pick_promote_dir_for_directory_is_self() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        assert_eq!(pick_promote_dir(dir), dir.to_path_buf());
    }

    #[test]
    fn pick_promote_dir_for_file_is_parent() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("note.txt");
        fs::write(&file, "hi").unwrap();
        let dir = pick_promote_dir(&file);
        assert_eq!(dir, tmp.path().to_path_buf());
    }

    #[test]
    fn pick_promote_dir_for_nonexistent_falls_back_to_parent() {
        let tmp = TempDir::new().unwrap();
        let new_file = tmp.path().join("does_not_exist.txt");
        let dir = pick_promote_dir(&new_file);
        assert_eq!(dir, tmp.path().to_path_buf());
    }

    #[tokio::test]
    async fn trigger_access_resolves_inside_root_without_ask() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("inside.txt");
        fs::write(&file, "hi").unwrap();
        let policy = Arc::new(Mutex::new(SandboxPolicy::new(tmp.path().to_path_buf())));
        let (svc, _rx) = PermissionService::new("test", default_ruleset());
        let decision = trigger_access(
            &policy,
            &svc,
            AccessRequest {
                requested: file.to_str().unwrap(),
                tool: "read",
            },
        )
        .await
        .expect("trigger_access");
        match decision {
            AccessDecision::Allowed { path, .. } => assert_eq!(path, file.canonicalize().unwrap()),
            AccessDecision::Rejected { message } => panic!("unexpected reject: {message}"),
        }
    }

    #[tokio::test]
    async fn trigger_access_rejects_when_user_says_no() {
        let project = TempDir::new().unwrap();
        let external = TempDir::new().unwrap();
        let outside = external.path().join("secret.txt");
        fs::write(&outside, "nope").unwrap();
        let policy = Arc::new(Mutex::new(SandboxPolicy::new(project.path().to_path_buf())));
        let (svc, mut rx) = PermissionService::new("test", default_ruleset());
        let svc_clone = svc.clone();
        tokio::spawn(async move {
            let req = rx.recv().await.expect("request");
            svc_clone.reply(&req.id, UserReply::Reject).ok();
        });
        let decision = trigger_access(
            &policy,
            &svc,
            AccessRequest {
                requested: outside.to_str().unwrap(),
                tool: "read",
            },
        )
        .await
        .expect("trigger_access");
        assert!(matches!(decision, AccessDecision::Rejected { .. }));
        assert!(policy.lock().external_roots.is_empty());
    }

    #[tokio::test]
    async fn trigger_access_promotes_directory_target_itself() {
        let project = TempDir::new().unwrap();
        let external = TempDir::new().unwrap();
        let target_dir = external.path().join("payload");
        fs::create_dir(&target_dir).unwrap();
        let policy = Arc::new(Mutex::new(SandboxPolicy::new(project.path().to_path_buf())));
        let (svc, mut rx) = PermissionService::new("test", default_ruleset());
        let svc_clone = svc.clone();
        tokio::spawn(async move {
            let req = rx.recv().await.expect("request");
            svc_clone.reply(&req.id, UserReply::Once).ok();
        });
        // Pass the directory path itself.
        let decision = trigger_access(
            &policy,
            &svc,
            AccessRequest {
                requested: target_dir.to_str().unwrap(),
                tool: "read",
            },
        )
        .await
        .expect("trigger_access");
        assert!(matches!(decision, AccessDecision::Allowed { .. }));
        // The promoted directory should be the target itself, not
        // its parent. Re-resolving the directory should now succeed.
        let canonical_target = target_dir.canonicalize().unwrap();
        assert!(policy
            .lock()
            .external_roots
            .iter()
            .any(|root| root == &canonical_target));
    }

    #[tokio::test]
    async fn trigger_access_promotes_parent_for_new_file() {
        let project = TempDir::new().unwrap();
        let external = TempDir::new().unwrap();
        let policy = Arc::new(Mutex::new(SandboxPolicy::new(project.path().to_path_buf())));
        let (svc, mut rx) = PermissionService::new("test", default_ruleset());
        let svc_clone = svc.clone();
        tokio::spawn(async move {
            let req = rx.recv().await.expect("request");
            svc_clone.reply(&req.id, UserReply::Once).ok();
        });
        // The file doesn't exist yet, so canonicalize will fail and
        // pick_promote_dir will return the parent.
        let new_file = external.path().join("new.txt");
        let decision = trigger_access(
            &policy,
            &svc,
            AccessRequest {
                requested: new_file.to_str().unwrap(),
                tool: "write",
            },
        )
        .await
        .expect("trigger_access");
        assert!(matches!(decision, AccessDecision::Allowed { .. }));
        let canonical_parent = external.path().canonicalize().unwrap();
        assert!(policy
            .lock()
            .external_roots
            .iter()
            .any(|root| root == &canonical_parent));
    }
}
