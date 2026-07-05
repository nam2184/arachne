use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use super::rule::{PermissionAction, PermissionRule};
use super::ruleset::PermissionRuleset;

/// Identifies a single pending permission request.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RequestId(pub String);

/// Suggested patterns that the user can "always" approve. For example, a bash
/// approval might suggest `["git status", "git status *"]` so future git status
/// calls run without prompting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub id: RequestId,
    pub session_id: String,
    pub permission: String,
    pub patterns: Vec<String>,
    pub tool: String,
    pub metadata: Option<serde_json::Value>,
    pub always: Vec<String>,
}

pub type PermissionRequestReceiver = mpsc::UnboundedReceiver<PermissionRequest>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserReply {
    Once,
    Always,
    Reject,
}

/// Internal pending state for a permission request.
struct PendingEntry {
    request: PermissionRequest,
    reply_tx: oneshot::Sender<UserReply>,
}

/// Per-session permission service. Each session gets its own service so that
/// approved rules are scoped to that session.
pub struct PermissionService {
    session_id: String,
    base_ruleset: RwLock<PermissionRuleset>,
    approved: RwLock<Vec<PermissionRule>>,
    external_roots: RwLock<Vec<PathBuf>>,
    pending: RwLock<HashMap<RequestId, PendingEntry>>,
    /// Channel used by the frontend/Tauri layer to receive new requests.
    request_tx: mpsc::UnboundedSender<PermissionRequest>,
}

impl PermissionService {
    pub fn new(
        session_id: impl Into<String>,
        base_ruleset: PermissionRuleset,
    ) -> (Arc<Self>, PermissionRequestReceiver) {
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let service = Arc::new(Self {
            session_id: session_id.into(),
            base_ruleset: RwLock::new(base_ruleset),
            approved: RwLock::new(Vec::new()),
            external_roots: RwLock::new(Vec::new()),
            pending: RwLock::new(HashMap::new()),
            request_tx,
        });
        (service, request_rx)
    }

    /// Resolve a tool call. Returns `Ok(())` if the action is allowed (or
    /// approved by the user), `Err(PermissionDenied)` if denied by the ruleset,
    /// and `Err(PermissionAsked)` if the user needs to be prompted.
    pub fn check(&self, request: CheckRequest) -> Result<CheckOutcome, CheckError> {
        let CheckRequest {
            permission,
            pattern,
            tool,
            always,
            request_id,
        } = request;

        let rule = {
            let approved_ruleset = PermissionRuleset {
                rules: self.approved.read().clone(),
            };
            // Order matters: base first, approved later so they win (last-match wins).
            let base_ruleset = self.base_ruleset.read();
            PermissionRuleset::evaluate_merged(
                &[&*base_ruleset, &approved_ruleset],
                &permission,
                &pattern,
            )
        };

        match rule.action {
            PermissionAction::Allow => Ok(CheckOutcome::Allowed),
            PermissionAction::Deny => Err(CheckError::Denied {
                permission,
                pattern,
            }),
            PermissionAction::Ask => {
                let id = request_id.unwrap_or_else(|| RequestId(Uuid::new_v4().to_string()));
                let req = PermissionRequest {
                    id: id.clone(),
                    session_id: self.session_id.clone(),
                    permission,
                    patterns: vec![pattern],
                    tool,
                    metadata: None,
                    always,
                };
                self.ask(req)
            }
        }
    }

    fn ask(&self, request: PermissionRequest) -> Result<CheckOutcome, CheckError> {
        let id = request.id.clone();
        let (reply_tx, reply_rx) = oneshot::channel();
        {
            let mut pending = self.pending.write();
            pending.insert(
                id.clone(),
                PendingEntry {
                    request: request.clone(),
                    reply_tx,
                },
            );
        }
        if self.request_tx.send(request).is_err() {
            self.pending.write().remove(&id);
            return Err(CheckError::Rejected { id });
        }
        match reply_rx.blocking_recv() {
            Ok(UserReply::Once) | Ok(UserReply::Always) => Ok(CheckOutcome::Allowed),
            Ok(UserReply::Reject) | Err(_) => Err(CheckError::Rejected { id }),
        }
    }

    /// Async variant of `check`. Useful when the caller is in an async context.
    pub async fn check_async(&self, request: CheckRequest) -> Result<CheckOutcome, CheckError> {
        let CheckRequest {
            permission,
            pattern,
            tool,
            always,
            request_id,
        } = request;

        let rule = {
            let approved_ruleset = PermissionRuleset {
                rules: self.approved.read().clone(),
            };
            // Order matters: base first, approved later so they win (last-match wins).
            let base_ruleset = self.base_ruleset.read();
            PermissionRuleset::evaluate_merged(
                &[&*base_ruleset, &approved_ruleset],
                &permission,
                &pattern,
            )
        };

        match rule.action {
            PermissionAction::Allow => Ok(CheckOutcome::Allowed),
            PermissionAction::Deny => Err(CheckError::Denied {
                permission,
                pattern,
            }),
            PermissionAction::Ask => {
                let id = request_id.unwrap_or_else(|| RequestId(Uuid::new_v4().to_string()));
                let req = PermissionRequest {
                    id,
                    session_id: self.session_id.clone(),
                    permission,
                    patterns: vec![pattern],
                    tool,
                    metadata: None,
                    always,
                };
                self.ask_async(req).await
            }
        }
    }

    async fn ask_async(&self, request: PermissionRequest) -> Result<CheckOutcome, CheckError> {
        let id = request.id.clone();
        let (reply_tx, reply_rx) = oneshot::channel();
        {
            let mut pending = self.pending.write();
            pending.insert(
                id.clone(),
                PendingEntry {
                    request: request.clone(),
                    reply_tx,
                },
            );
        }
        if self.request_tx.send(request).is_err() {
            self.pending.write().remove(&id);
            return Err(CheckError::Rejected { id });
        }
        match reply_rx.await {
            Ok(UserReply::Once) | Ok(UserReply::Always) => Ok(CheckOutcome::Allowed),
            Ok(UserReply::Reject) | Err(_) => Err(CheckError::Rejected { id }),
        }
    }

    /// Reply to a pending permission request. Called by the Tauri layer after
    /// the user clicks a button in the UI.
    pub fn reply(&self, request_id: &RequestId, reply: UserReply) -> Result<(), CheckError> {
        let entry = {
            let mut pending = self.pending.write();
            pending.remove(request_id)
        };
        match entry {
            Some(entry) => {
                // If the reply is Always, persist the suggested patterns as
                // session-scoped allow rules *before* the waiters are notified.
                if matches!(reply, UserReply::Always) {
                    for pattern in &entry.request.always {
                        self.approved.write().push(PermissionRule::allow(
                            entry.request.permission.clone(),
                            pattern.clone(),
                        ));
                    }
                }
                let _ = entry.reply_tx.send(reply);
                if matches!(reply, UserReply::Reject) {
                    // Reject any other pending requests in the same session.
                    let to_reject: Vec<_> = {
                        let pending = self.pending.read();
                        pending
                            .iter()
                            .filter(|(_, e)| e.request.session_id == entry.request.session_id)
                            .map(|(id, _)| id.clone())
                            .collect()
                    };
                    for other_id in to_reject {
                        if let Some(other) = self.pending.write().remove(&other_id) {
                            let _ = other.reply_tx.send(UserReply::Reject);
                        }
                    }
                }
                Ok(())
            }
            None => Err(CheckError::NotFound {
                id: request_id.clone(),
            }),
        }
    }

    /// List all currently pending requests. Used by the frontend to render
    /// prompts for sessions that aren't actively asking.
    pub fn list_pending(&self) -> Vec<PermissionRequest> {
        self.pending
            .read()
            .values()
            .map(|e| e.request.clone())
            .collect()
    }

    /// Number of approved rules accumulated so far.
    pub fn approved_rule_count(&self) -> usize {
        self.approved.read().len()
    }

    pub fn add_external_root(&self, root: impl AsRef<Path>) {
        let root = root.as_ref().to_path_buf();
        let mut external_roots = self.external_roots.write();
        if !external_roots.iter().any(|existing| existing == &root) {
            external_roots.push(root);
        }
    }

    pub fn external_roots(&self) -> Vec<PathBuf> {
        self.external_roots.read().clone()
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn base_ruleset(&self) -> PermissionRuleset {
        self.base_ruleset.read().clone()
    }

    pub fn replace_base_ruleset(&self, ruleset: PermissionRuleset) {
        *self.base_ruleset.write() = ruleset;
    }
}

#[derive(Debug, Clone)]
pub struct CheckRequest {
    pub permission: String,
    pub pattern: String,
    pub tool: String,
    pub always: Vec<String>,
    pub request_id: Option<RequestId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckOutcome {
    Allowed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckError {
    Denied { permission: String, pattern: String },
    Rejected { id: RequestId },
    NotFound { id: RequestId },
}

impl std::fmt::Display for CheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckError::Denied {
                permission,
                pattern,
            } => {
                write!(f, "permission denied: {permission} {pattern}")
            }
            CheckError::Rejected { id } => write!(f, "user rejected request {}", id.0),
            CheckError::NotFound { id } => write!(f, "no pending request with id {}", id.0),
        }
    }
}

impl std::error::Error for CheckError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission_v2::rule::PermissionRule;

    fn make_service(
        rules: Vec<PermissionRule>,
    ) -> (
        Arc<PermissionService>,
        mpsc::UnboundedReceiver<PermissionRequest>,
    ) {
        let ruleset = PermissionRuleset { rules };
        PermissionService::new("test-session", ruleset)
    }

    #[test]
    fn allow_rule_lets_call_through() {
        let (svc, _rx) = make_service(vec![PermissionRule::allow("bash", "*")]);
        let result = svc.check(CheckRequest {
            permission: "bash".to_string(),
            pattern: "git status".to_string(),
            tool: "bash".to_string(),
            always: vec![],
            request_id: None,
        });
        assert!(matches!(result, Ok(CheckOutcome::Allowed)));
    }

    #[test]
    fn deny_rule_blocks_call() {
        let (svc, _rx) = make_service(vec![PermissionRule::deny("bash", "rm *")]);
        let result = svc.check(CheckRequest {
            permission: "bash".to_string(),
            pattern: "rm -rf /".to_string(),
            tool: "bash".to_string(),
            always: vec![],
            request_id: None,
        });
        assert!(matches!(result, Err(CheckError::Denied { .. })));
    }

    #[test]
    fn ask_rule_blocks_until_replied() {
        let (svc, mut rx) = make_service(vec![PermissionRule::ask("bash", "git *")]);
        // Spawn a thread to reply to the request.
        let svc_clone = svc.clone();
        std::thread::spawn(move || {
            // Wait for the request to appear on the channel.
            let req = rx.blocking_recv().expect("request");
            svc_clone.reply(&req.id, UserReply::Once).expect("reply");
        });
        let result = svc.check(CheckRequest {
            permission: "bash".to_string(),
            pattern: "git status".to_string(),
            tool: "bash".to_string(),
            always: vec!["git status".to_string()],
            request_id: None,
        });
        assert!(matches!(result, Ok(CheckOutcome::Allowed)));
    }

    #[test]
    fn always_reply_adds_to_approved() {
        let (svc, mut rx) = make_service(vec![PermissionRule::ask("bash", "git *")]);
        let svc_clone = svc.clone();
        std::thread::spawn(move || {
            println!("[thread] waiting for request");
            let req = rx.blocking_recv().expect("request");
            println!("[thread] got request id={}, replying Always", req.id.0);
            svc_clone.reply(&req.id, UserReply::Always).expect("reply");
            println!("[thread] reply sent");
        });
        println!("[main] calling check (first)");
        let _ = svc
            .check(CheckRequest {
                permission: "bash".to_string(),
                pattern: "git status".to_string(),
                tool: "bash".to_string(),
                always: vec!["git *".to_string()],
                request_id: None,
            })
            .expect("first call allowed");
        println!(
            "[main] first call returned, approved count={}",
            svc.approved_rule_count()
        );
        assert_eq!(svc.approved_rule_count(), 1);
        println!("[main] calling check (second)");
        let result = svc.check(CheckRequest {
            permission: "bash".to_string(),
            pattern: "git status".to_string(),
            tool: "bash".to_string(),
            always: vec![],
            request_id: None,
        });
        println!("[main] second call returned {:?}", result);
        assert!(matches!(result, Ok(CheckOutcome::Allowed)));
    }

    #[test]
    fn reject_blocks_and_clears_other_pending_in_session() {
        let (svc, mut rx) = make_service(vec![PermissionRule::ask("bash", "*")]);
        let svc_clone = svc.clone();
        let session = svc.session_id().to_string();
        // Spawn two callers that will be blocked on ask.
        let s1 = svc.clone();
        let s2 = svc.clone();
        let h1 = std::thread::spawn(move || {
            s1.check(CheckRequest {
                permission: "bash".to_string(),
                pattern: "ls".to_string(),
                tool: "bash".to_string(),
                always: vec![],
                request_id: Some(RequestId("req-1".to_string())),
            })
        });
        let h2 = std::thread::spawn(move || {
            s2.check(CheckRequest {
                permission: "bash".to_string(),
                pattern: "pwd".to_string(),
                tool: "bash".to_string(),
                always: vec![],
                request_id: Some(RequestId("req-2".to_string())),
            })
        });
        // Drain pending requests and reply reject on the first.
        let first = rx.blocking_recv().expect("first req");
        assert_eq!(first.tool, "bash");
        svc.reply(&first.id, UserReply::Reject).expect("reply");
        // Other pending should have been cleared and rejected.
        let r1 = h1.join().unwrap();
        let r2 = h2.join().unwrap();
        assert!(
            r1.is_err() || r2.is_err(),
            "at least one should have been rejected"
        );
        let _ = session;
    }

    #[test]
    fn reply_to_unknown_id_returns_not_found() {
        let (svc, _rx) = make_service(vec![]);
        let result = svc.reply(&RequestId("nope".to_string()), UserReply::Once);
        assert!(matches!(result, Err(CheckError::NotFound { .. })));
    }

    #[test]
    fn replacing_base_ruleset_preserves_session_state() {
        let (svc, _rx) = make_service(vec![PermissionRule::allow("bash", "git *")]);
        svc.add_external_root("/tmp/external");
        svc.approved
            .write()
            .push(PermissionRule::allow("bash", "npm *"));

        svc.replace_base_ruleset(PermissionRuleset {
            rules: vec![PermissionRule::deny("bash", "git push *")],
        });

        assert_eq!(svc.approved_rule_count(), 1);
        assert_eq!(svc.external_roots(), vec![PathBuf::from("/tmp/external")]);
        assert_eq!(
            svc.base_ruleset()
                .evaluate("bash", "git push origin main")
                .action,
            PermissionAction::Deny
        );
        assert!(matches!(
            svc.check(CheckRequest {
                permission: "bash".to_string(),
                pattern: "npm test".to_string(),
                tool: "bash".to_string(),
                always: vec![],
                request_id: None,
            }),
            Ok(CheckOutcome::Allowed)
        ));
    }
}
