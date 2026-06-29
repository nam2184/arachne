use crate::services::permission_map::PermissionMap;
use arachne_agents::permission_v2::{PermissionRequest, RequestId, UserReply};
use std::sync::Arc;
use tauri::{Emitter, State};

/// Response for listing pending permission requests.
#[derive(serde::Serialize)]
pub struct PendingPermissionsResponse {
    pub requests: Vec<PermissionRequest>,
}

/// Request body for the `permission_reply` command.
#[derive(serde::Deserialize)]
pub struct ReplyPermissionRequest {
    #[serde(alias = "sessionId")]
    pub session_id: String,
    #[serde(alias = "requestId")]
    pub request_id: String,
    pub reply: UserReply,
}

/// List all pending permission requests for a session. Frontend polls this
/// when the `permission-changed` event fires (or on a fixed interval while
/// the chat is active).
#[tauri::command]
pub async fn permission_list_pending(
    session_id: String,
    state: State<'_, Arc<PermissionMap>>,
) -> Result<PendingPermissionsResponse, String> {
    let service = state
        .get(&session_id)
        .ok_or_else(|| "no permission service for session".to_string())?;
    let requests = service.list_pending();
    Ok(PendingPermissionsResponse { requests })
}

/// Reply to a pending permission request. Called by the UI after the user
/// clicks Yes or No.
#[tauri::command]
pub async fn permission_reply(
    request: ReplyPermissionRequest,
    state: State<'_, Arc<PermissionMap>>,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    let service = state
        .get(&request.session_id)
        .ok_or_else(|| "no permission service for session".to_string())?;
    service
        .reply(&RequestId(request.request_id), request.reply)
        .map_err(|e| e.to_string())?;

    // Notify UI that pending list may have changed.
    let _ = app_handle.emit("permission-changed", &request.session_id);
    Ok(())
}
