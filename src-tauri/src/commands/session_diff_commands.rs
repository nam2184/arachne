use std::sync::Arc;

use arachne_agents::{ConversationService, SessionFileDiff, SessionService};
use tauri::State;

#[tauri::command]
pub async fn get_session_diff(
    session_id: String,
    message_id: Option<String>,
    session_service: State<'_, Arc<SessionService>>,
    conversation_service: State<'_, Arc<ConversationService>>,
) -> Result<Vec<SessionFileDiff>, String> {
    if session_service.get_session(&session_id)?.is_none() {
        return Err(format!("Session not found: {session_id}"));
    }
    conversation_service.get_session_diff(&session_id, message_id.as_deref())
}
