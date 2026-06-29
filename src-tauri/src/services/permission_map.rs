use arachne_agents::permission_v2::{default_ruleset, PermissionService};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter};

/// Per-session permission service registry. Each session gets its own
/// `PermissionService` so that approved rules, pending requests, and
/// doom-loop state are scoped to that session.
pub struct PermissionMap {
    services: Mutex<HashMap<String, Arc<PermissionService>>>,
    app_handle: Mutex<Option<AppHandle>>,
}

impl PermissionMap {
    pub fn new() -> Self {
        Self {
            services: Mutex::new(HashMap::new()),
            app_handle: Mutex::new(None),
        }
    }

    pub fn set_app_handle(&self, app_handle: AppHandle) {
        *self.app_handle.lock().unwrap() = Some(app_handle);
    }

    /// Get the service for `session_id`, creating one with the default
    /// ruleset if it doesn't exist.
    pub fn get_or_create(&self, session_id: &str) -> Arc<PermissionService> {
        let mut services = self.services.lock().unwrap();
        services
            .entry(session_id.to_string())
            .or_insert_with(|| {
                let ruleset = default_ruleset();
                let (service, mut rx) = PermissionService::new(session_id, ruleset);
                let app_handle = self.app_handle.lock().unwrap().clone();
                let session_id = session_id.to_string();
                tauri::async_runtime::spawn(async move {
                    while let Some(request) = rx.recv().await {
                        tracing::debug!(
                            session_id = %request.session_id,
                            permission = %request.permission,
                            tool = %request.tool,
                            "permission request pending"
                        );
                        if let Some(app_handle) = &app_handle {
                            let _ = app_handle.emit("permission-changed", &request.session_id);
                        } else {
                            tracing::warn!(
                                session_id = %session_id,
                                "permission request created before app handle was registered"
                            );
                        }
                    }
                });
                service
            })
            .clone()
    }

    /// Get the service for `session_id` if it exists, without creating.
    pub fn get(&self, session_id: &str) -> Option<Arc<PermissionService>> {
        self.services.lock().unwrap().get(session_id).cloned()
    }

    /// Remove the service for `session_id`.
    pub fn remove(&self, session_id: &str) -> Option<Arc<PermissionService>> {
        self.services.lock().unwrap().remove(session_id)
    }
}
