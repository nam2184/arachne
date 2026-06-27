use arachne_agents::{AgentSession, ConversationService, SessionGroup, SessionService};
use std::sync::Arc;
use tauri::State;

#[derive(serde::Serialize)]
pub struct SessionInitPayload {
    sessions: Vec<AgentSession>,
    groups: Vec<SessionGroup>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedSessionChatPayload {
    root_session_id: String,
    chat_session_id: String,
}

#[tauri::command]
pub async fn init_sessions(
    session_service: State<'_, Arc<SessionService>>,
) -> Result<SessionInitPayload, String> {
    Ok(SessionInitPayload {
        sessions: session_service.get_all_sessions()?,
        groups: session_service.get_all_groups()?,
    })
}

#[tauri::command]
pub async fn create_session(
    project_id: String,
    directory: String,
    provider: String,
    model: String,
    session_service: State<'_, Arc<SessionService>>,
    conversation_service: State<'_, Arc<ConversationService>>,
) -> Result<String, String> {
    let (id, created) =
        session_service.create_top_level_session(project_id, directory, provider, model)?;
    if created {
        if let Err(error) = conversation_service.create_conversation(&id) {
            let _ = session_service.delete_session(&id);
            return Err(error);
        }
    }
    Ok(id)
}

#[tauri::command]
pub async fn create_session_chat(
    session_id: String,
    session_service: State<'_, Arc<SessionService>>,
    conversation_service: State<'_, Arc<ConversationService>>,
) -> Result<CreatedSessionChatPayload, String> {
    let created = session_service.create_session_chat(&session_id)?;
    if let Some(root_id) = created.created_root_session_id.as_deref() {
        if let Err(error) = conversation_service.create_conversation(root_id) {
            let _ = session_service.delete_session(root_id);
            return Err(error);
        }
    }
    if let Err(error) = conversation_service.create_conversation(&created.chat_session_id) {
        let _ = session_service.delete_session(&created.chat_session_id);
        return Err(error);
    }
    Ok(CreatedSessionChatPayload {
        root_session_id: created.root_session_id,
        chat_session_id: created.chat_session_id,
    })
}

#[tauri::command]
pub async fn get_session(
    id: String,
    session_service: State<'_, Arc<SessionService>>,
) -> Result<Option<AgentSession>, String> {
    session_service.get_session(&id)
}

#[tauri::command]
pub async fn get_all_sessions(
    session_service: State<'_, Arc<SessionService>>,
) -> Result<Vec<AgentSession>, String> {
    session_service.get_all_sessions()
}

#[tauri::command]
pub async fn update_session_title(
    session_id: String,
    title: String,
    session_service: State<'_, Arc<SessionService>>,
) -> Result<(), String> {
    let title = title.trim();
    let title = if title.is_empty() { None } else { Some(title) };
    session_service.update_session_title(&session_id, title)
}

#[tauri::command]
pub async fn delete_session(
    id: String,
    session_service: State<'_, Arc<SessionService>>,
    conversation_service: State<'_, Arc<ConversationService>>,
) -> Result<(), String> {
    let session = session_service.get_session(&id)?;
    if session
        .as_ref()
        .is_some_and(|session| session.parent_session_id.is_none())
    {
        for chat in session_service.session_chats(&id)? {
            let _ = conversation_service.delete_conversation(&chat.id);
            session_service.delete_session(&chat.id)?;
        }
    }
    conversation_service.delete_conversation(&id)?;
    session_service.delete_session(&id)
}

#[tauri::command]
pub async fn create_session_group(
    session_ids: Vec<String>,
    session_service: State<'_, Arc<SessionService>>,
) -> Result<String, String> {
    session_service.create_group(session_ids)
}

#[tauri::command]
pub async fn get_all_session_groups(
    session_service: State<'_, Arc<SessionService>>,
) -> Result<Vec<SessionGroup>, String> {
    session_service.get_all_groups()
}

#[tauri::command]
pub async fn delete_session_group(
    id: String,
    session_service: State<'_, Arc<SessionService>>,
) -> Result<(), String> {
    session_service.delete_group(&id)
}

#[tauri::command]
pub async fn rename_session_group(
    id: String,
    name: Option<String>,
    session_service: State<'_, Arc<SessionService>>,
) -> Result<(), String> {
    session_service.rename_group(&id, name)
}

#[tauri::command]
pub async fn add_session_to_group(
    session_id: String,
    group_id: String,
    session_service: State<'_, Arc<SessionService>>,
) -> Result<(), String> {
    session_service.add_session_to_group(&session_id, &group_id)
}

#[tauri::command]
pub async fn remove_session_from_group(
    session_id: String,
    session_service: State<'_, Arc<SessionService>>,
) -> Result<(), String> {
    session_service.remove_session_from_group(&session_id)
}
