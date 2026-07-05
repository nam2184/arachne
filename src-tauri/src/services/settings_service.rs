use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arachne_agents::{McpConfig, McpServerConfig, RuntimeConfig, RuntimeWebSearchConfig, UiConfig};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_editor_font_size")]
    pub editor_font_size: u32,
    #[serde(default = "default_editor_tab_size")]
    pub editor_tab_size: u32,
    #[serde(default = "default_node_skin")]
    pub node_skin: String,
    #[serde(default = "default_workspace_mode")]
    pub workspace_mode: String,
    #[serde(default = "default_code_block_theme")]
    pub code_block_theme: String,
    #[serde(default = "default_cursor_theme")]
    pub cursor_theme: String,
    #[serde(default)]
    pub searxng_base_url: Option<String>,
    #[serde(default = "default_websearch_max_results")]
    pub websearch_max_results: u32,
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
}

fn default_theme() -> String {
    "dark".to_string()
}
fn default_editor_font_size() -> u32 {
    14
}
fn default_editor_tab_size() -> u32 {
    2
}
fn default_node_skin() -> String {
    "default".to_string()
}
fn default_workspace_mode() -> String {
    "canvas".to_string()
}
fn default_code_block_theme() -> String {
    "github".to_string()
}
fn default_cursor_theme() -> String {
    "react-flow".to_string()
}
fn default_websearch_max_results() -> u32 {
    5
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            theme: "dark".to_string(),
            editor_font_size: 14,
            editor_tab_size: 2,
            node_skin: "default".to_string(),
            workspace_mode: "canvas".to_string(),
            code_block_theme: default_code_block_theme(),
            cursor_theme: default_cursor_theme(),
            searxng_base_url: None,
            websearch_max_results: default_websearch_max_results(),
            mcp_servers: BTreeMap::new(),
        }
    }
}

pub struct SettingsService {
    settings: RwLock<AppSettings>,
    config_path: PathBuf,
}

impl SettingsService {
    pub fn new(config_dir: PathBuf) -> Arc<Self> {
        let config_path = config_dir.join("settings.json");
        Arc::new(Self {
            settings: RwLock::new(AppSettings::default()),
            config_path,
        })
    }

    pub fn load(&self) -> Result<(), String> {
        if !self.config_path.exists() {
            return Ok(());
        }

        let content = std::fs::read_to_string(&self.config_path)
            .map_err(|e| format!("Failed to read settings: {}", e))?;

        let settings: AppSettings = serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse settings: {}", e))?;

        *self.settings.write() = settings;
        Ok(())
    }

    pub fn save(&self) -> Result<(), String> {
        let settings = self.settings.read().clone();
        let content = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;

        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create config directory: {}", e))?;
        }

        std::fs::write(&self.config_path, content)
            .map_err(|e| format!("Failed to write settings: {}", e))
    }

    pub fn get_settings(&self) -> AppSettings {
        self.settings.read().clone()
    }

    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    pub fn update_settings(&self, updates: AppSettings) {
        *self.settings.write() = updates;
    }

    pub fn runtime_config(&self) -> RuntimeConfig {
        let settings = self.settings.read();
        RuntimeConfig {
            ui: UiConfig {
                theme: Some(settings.theme.clone()),
                editor_font_size: Some(settings.editor_font_size),
                editor_tab_size: Some(settings.editor_tab_size),
                node_skin: Some(settings.node_skin.clone()),
                workspace_mode: Some(settings.workspace_mode.clone()),
                code_block_theme: Some(settings.code_block_theme.clone()),
                cursor_theme: Some(settings.cursor_theme.clone()),
            },
            websearch: RuntimeWebSearchConfig {
                searxng_base_url: settings
                    .searxng_base_url
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string),
                max_results: Some(settings.websearch_max_results.clamp(1, 20) as usize),
            },
            mcp: McpConfig {
                servers: settings.mcp_servers.clone(),
            },
            ..RuntimeConfig::default()
        }
    }
}

impl Default for SettingsService {
    fn default() -> Self {
        Self {
            settings: RwLock::new(AppSettings::default()),
            config_path: PathBuf::new(),
        }
    }
}
