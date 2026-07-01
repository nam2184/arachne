use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod commands;
mod error;
mod services;

use arachne_agents::{
    create_conversation_service, llm::SubagentRegistry, paths, ConversationService,
    ProviderService, SessionService, SnapshotService,
};
use services::agent_service::AgentService;
use services::memory_service::MemoryService;
use services::permission_map::PermissionMap;
use services::project_service::ProjectService;
use services::settings_service::SettingsService;
use services::stack_detector::StackDetector;
use services::ui_command_service::UiCommandService;

pub struct AppState {
    pub project_service: Arc<ProjectService>,
    pub agent_service: Arc<AgentService>,
    pub session_service: Arc<SessionService>,
    pub conversation_service: Arc<ConversationService>,
    pub snapshot_service: Arc<SnapshotService>,
    pub settings_service: Arc<SettingsService>,
    pub memory_service: Arc<MemoryService>,
    pub stack_detector: Arc<StackDetector>,
    pub permission_map: Arc<PermissionMap>,
    pub ui_command_service: Arc<UiCommandService>,
}

fn setup_logging() {
    let default_filter = default_log_filter();
    let filter = tracing_subscriber::EnvFilter::new(
        std::env::var("RUST_LOG").unwrap_or_else(|_| default_filter.to_string()),
    );
    let log_path = log_file_path();

    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(file) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(std::sync::Mutex::new(file)),
                )
                .init();
            tracing::info!(log_file = %log_path.display(), "logging initialized");
        }
        Err(error) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(std::io::stderr),
                )
                .init();
            tracing::warn!(
                log_file = %log_path.display(),
                error = %error,
                "failed to open log file; falling back to stderr"
            );
        }
    }
}

fn log_file_path() -> PathBuf {
    std::env::var_os("ARACHNE_LOG_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(paths::log_file)
}

fn default_app_data_dir() -> PathBuf {
    paths::data_dir()
}

fn default_app_config_dir() -> PathBuf {
    paths::config_dir()
}

fn default_log_filter() -> &'static str {
    if cfg!(feature = "dev-logs") || cfg!(debug_assertions) {
        "arachne=debug,arachne_agents=debug,tauri=info"
    } else if cfg!(feature = "prod-logs") {
        "arachne=info,arachne_agents=info"
    } else {
        "warn"
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = dotenvy::dotenv();
    setup_logging();

    let stack_detector = StackDetector::new();
    let memory_service = MemoryService::new();

    let app_data_dir = default_app_data_dir();
    let app_config_dir = default_app_config_dir();

    let db_path = app_data_dir.join("arachne.sqlite");
    let project_service = ProjectService::new(db_path.clone(), Arc::clone(&stack_detector));
    let session_service = SessionService::new(db_path.clone());
    let conversation_service = create_conversation_service(app_data_dir.join("conversations"));
    let snapshot_service = SnapshotService::new(app_data_dir.join("snapshots"));
    let provider_service = ProviderService::new(db_path.clone());

    // Initialize the database schema once so the sub-agent registry can
    // query it via its own connections (it opens per-call). The
    // ProjectService / SessionService will also call init() on their
    // own connections when they first access the file; init() is
    // idempotent.
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(conn) = arachne_agents::database::Database::new(db_path.clone()) {
        if let Err(e) = conn.init() {
            tracing::warn!("Failed to init shared database: {}", e);
        }
    }

    let settings_service = SettingsService::new(app_config_dir);
    if let Err(e) = settings_service.load() {
        tracing::warn!("Failed to load settings: {}", e);
    }

    let permission_map = Arc::new(PermissionMap::new());
    let permission_map_for_setup = Arc::clone(&permission_map);
    let ui_command_service = Arc::new(UiCommandService::new());
    let subagent_registry = SubagentRegistry::new(db_path.clone());

    let agent_service = AgentService::new(
        Arc::clone(&session_service),
        Arc::clone(&conversation_service),
        Arc::clone(&provider_service),
        Arc::clone(&subagent_registry),
        Arc::clone(&permission_map),
        Arc::clone(&snapshot_service),
    );

    tauri::Builder::default()
        .setup(move |app| {
            permission_map_for_setup.set_app_handle(app.handle().clone());
            Ok(())
        })
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_shell::init())
        .manage(AppState {
            project_service: Arc::clone(&project_service),
            agent_service: Arc::clone(&agent_service),
            session_service: Arc::clone(&session_service),
            conversation_service: Arc::clone(&conversation_service),
            snapshot_service: Arc::clone(&snapshot_service),
            settings_service: Arc::clone(&settings_service),
            memory_service: Arc::clone(&memory_service),
            stack_detector: Arc::clone(&stack_detector),
            permission_map: Arc::clone(&permission_map),
            ui_command_service: Arc::clone(&ui_command_service),
        })
        .manage(permission_map)
        .manage(ui_command_service)
        .manage(agent_service)
        .manage(provider_service)
        .manage(conversation_service)
        .manage(snapshot_service)
        .manage(project_service)
        .manage(session_service)
        .manage(settings_service)
        .invoke_handler(tauri::generate_handler![
            commands::project_commands::create_project,
            commands::project_commands::open_project,
            commands::project_commands::get_project,
            commands::project_commands::list_projects,
            commands::project_commands::close_project,
            commands::project_commands::refresh_project_stack,
            commands::file_commands::read_file,
            commands::file_commands::write_file,
            commands::file_commands::list_directory,
            commands::file_commands::search_files,
            commands::file_commands::get_file_tree,
            commands::agent_commands::send_message,
            commands::agent_commands::update_session_provider,
            commands::session_commands::init_sessions,
            commands::session_commands::create_session,
            commands::session_commands::create_session_chat,
            commands::session_commands::get_session,
            commands::session_commands::get_all_sessions,
            commands::session_commands::update_session_title,
            commands::session_commands::delete_session,
            commands::session_commands::create_session_group,
            commands::session_commands::get_all_session_groups,
            commands::session_commands::delete_session_group,
            commands::session_commands::rename_session_group,
            commands::session_commands::add_session_to_group,
            commands::session_commands::remove_session_from_group,
            commands::conversation_commands::append_message,
            commands::conversation_commands::get_messages,
            commands::conversation_commands::get_ai_conversation,
            commands::conversation_commands::get_ui_conversation,
            commands::conversation_commands::compact_conversation,
            commands::conversation_commands::delete_conversation,
            commands::session_diff_commands::get_session_diff,
            commands::settings_commands::get_settings,
            commands::settings_commands::save_settings,
            commands::provider_commands::get_provider_configs,
            commands::provider_commands::get_provider_config,
            commands::provider_commands::get_provider_auth_states,
            commands::provider_commands::get_provider_auth_state,
            commands::provider_commands::upsert_provider_auth_state,
            commands::provider_commands::start_provider_oauth,
            commands::provider_commands::complete_provider_oauth,
            commands::provider_commands::upsert_provider_config,
            commands::provider_commands::delete_provider_config,
            commands::provider_commands::set_active_provider,
            commands::permission_commands::permission_list_pending,
            commands::permission_commands::permission_reply,
            commands::compaction_commands::compact_now,
            commands::ui_commands::list_ui_commands,
            commands::ui_commands::execute_ui_command,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn main() {
    run();
}
