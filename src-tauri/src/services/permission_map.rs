use arachne_agents::permission_v2::{
    default_ruleset, ruleset_from_runtime_config, service_from_runtime_config,
    PermissionRequestReceiver, PermissionRuleset, PermissionService,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter};

/// Per-session permission service registry.
///
/// The service is keyed by session because approvals, pending prompts, and
/// external roots are session-local runtime state. The base ruleset is not
/// treated as immutable: every runner build can refresh it from the latest
/// merged runtime config, including project `.arachne/config.json` changes.
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
                let (service, rx) = PermissionService::new(session_id, default_ruleset());
                self.forward_permission_requests(session_id, rx);
                service
            })
            .clone()
    }

    /// Get the service for `session_id`, creating it with the provided ruleset
    /// if needed. Existing services keep session-local state but replace their
    /// base ruleset so config changes are applied on the next runner build.
    pub fn get_or_create_with_ruleset(
        &self,
        session_id: &str,
        ruleset: PermissionRuleset,
    ) -> Arc<PermissionService> {
        let mut services = self.services.lock().unwrap();
        if let Some(service) = services.get(session_id) {
            service.replace_base_ruleset(ruleset);
            return Arc::clone(service);
        }

        let (service, rx) = PermissionService::new(session_id, ruleset);
        self.forward_permission_requests(session_id, rx);
        services.insert(session_id.to_string(), Arc::clone(&service));
        service
    }

    /// Get the service for `session_id`, creating it from merged runtime config
    /// if needed. Existing services keep session-local state but replace their
    /// base ruleset so per-project config changes are applied on the next
    /// runner build.
    pub fn get_or_create_with_runtime_config(
        &self,
        session_id: &str,
        runtime_config: &arachne_agents::RuntimeConfig,
    ) -> Arc<PermissionService> {
        let ruleset = ruleset_from_runtime_config(runtime_config);
        let mut services = self.services.lock().unwrap();
        if let Some(service) = services.get(session_id) {
            service.replace_base_ruleset(ruleset);
            return Arc::clone(service);
        }

        let (service, rx) = service_from_runtime_config(session_id.to_string(), runtime_config);
        self.forward_permission_requests(session_id, rx);
        services.insert(session_id.to_string(), Arc::clone(&service));
        service
    }

    fn forward_permission_requests(&self, session_id: &str, mut rx: PermissionRequestReceiver) {
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
