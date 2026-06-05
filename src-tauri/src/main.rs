use std::sync::Arc;
use tauri::Manager;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod commands;
mod db;
mod domain;
mod error;
mod services;

use services::agent_runtime::{create_agent_runtime, AgentRuntime};
use services::memory_service::MemoryService;
use services::project_service::ProjectService;
use services::settings_service::SettingsService;
use services::stack_detector::StackDetector;
use services::tree_sitter::TreeSitterService;
use services::watcher_service::WatcherService;
use services::context_indexer::ContextIndexer;

pub struct AppState {
    pub project_service: Arc<ProjectService>,
    pub agent_runtime: Arc<AgentRuntime>,
    pub settings_service: Arc<SettingsService>,
    pub memory_service: Arc<MemoryService>,
    pub stack_detector: Arc<StackDetector>,
    pub tree_sitter: Arc<TreeSitterService>,
    pub watcher_service: Arc<WatcherService>,
    pub context_indexer: Arc<ContextIndexer>,
}

fn setup_logging() {
    let default_filter = default_log_filter();

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            std::env::var("RUST_LOG").unwrap_or_else(|_| default_filter.to_string()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();
}

fn default_log_filter() -> &'static str {
    if cfg!(feature = "dev-logs") || cfg!(debug_assertions) {
        "openman=debug,tauri=info"
    } else if cfg!(feature = "prod-logs") {
        "openman=info"
    } else {
        "warn"
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    setup_logging();

    let tree_sitter = TreeSitterService::new();
    let stack_detector = StackDetector::new();
    let memory_service = MemoryService::new();
    let watcher_service = WatcherService::new();
    let context_indexer = ContextIndexer::new(Arc::clone(&tree_sitter));
    let agent_runtime = create_agent_runtime(Arc::clone(&tree_sitter));

    let project_service = ProjectService::new(Arc::clone(&stack_detector));

    let settings_service = Arc::new(SettingsService::new(
        directories::ProjectDirs::from("ai", "openman", "openman")
            .map(|d| d.config_dir().to_path_buf())
            .unwrap_or_else(|| std::env::current_dir().unwrap().join("config")),
    ));

    if let Err(e) = settings_service.load() {
        tracing::warn!("Failed to load settings: {}", e);
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_shell::init())
        .manage(AppState {
            project_service,
            agent_runtime,
            settings_service,
            memory_service,
            stack_detector,
            tree_sitter,
            watcher_service,
            context_indexer,
        })
        .invoke_handler(tauri::generate_handler![
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
            commands::agent_commands::create_agent_session,
            commands::agent_commands::send_message,
            commands::agent_commands::update_agent_context,
            commands::agent_commands::add_memory_fact,
            commands::agent_commands::parse_code_context,
            commands::settings_commands::get_settings,
            commands::settings_commands::update_provider,
            commands::settings_commands::set_active_provider,
            commands::settings_commands::save_settings,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
