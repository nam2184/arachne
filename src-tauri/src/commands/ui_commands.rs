use std::sync::Arc;

use tauri::State;

use crate::services::agent_service::AgentService;
use crate::services::ui_command_service::{UiCommand, UiCommandHint, UiCommandService};

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UiCommandResult {
    pub status: String,
    pub message: String,
    pub conversation_changed: bool,
}

#[tauri::command]
pub fn list_ui_commands(ui_commands: State<'_, Arc<UiCommandService>>) -> Vec<UiCommandHint> {
    ui_commands.hints()
}

#[tauri::command]
pub async fn execute_ui_command(
    session_id: String,
    input: String,
    ui_commands: State<'_, Arc<UiCommandService>>,
    agent_service: State<'_, Arc<AgentService>>,
) -> Result<UiCommandResult, String> {
    match ui_commands.parse(&input)? {
        UiCommand::Compact => match agent_service.compact_now(&session_id).await {
            Ok(arachne_agents::CompactionOutcome::Compacted { .. }) => Ok(UiCommandResult {
                status: "compacted".to_string(),
                message: "Conversation compacted.".to_string(),
                conversation_changed: true,
            }),
            Ok(arachne_agents::CompactionOutcome::NotNeeded) => Ok(UiCommandResult {
                status: "not_needed".to_string(),
                message: "No compaction needed yet.".to_string(),
                conversation_changed: false,
            }),
            Ok(arachne_agents::CompactionOutcome::Failed { reason }) => Err(reason),
            Err(error) => Err(error),
        },
        UiCommand::Help => Ok(UiCommandResult {
            status: "help".to_string(),
            message: ui_commands.help_text(),
            conversation_changed: false,
        }),
    }
}
