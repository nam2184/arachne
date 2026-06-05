use std::path::PathBuf;
use std::sync::Arc;
use parking_lot::RwLock;
use crate::domain::Provider;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    pub providers: Vec<ProviderConfig>,
    pub active_provider: String,
    pub active_model: String,
    pub theme: String,
    pub editor_font_size: u32,
    pub editor_tab_size: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub model: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub enabled: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            providers: vec![
                ProviderConfig {
                    name: "anthropic".to_string(),
                    model: "claude-3-5-sonnet-20241022".to_string(),
                    api_key: None,
                    base_url: None,
                    enabled: true,
                },
                ProviderConfig {
                    name: "openai".to_string(),
                    model: "gpt-4o".to_string(),
                    api_key: None,
                    base_url: None,
                    enabled: true,
                },
            ],
            active_provider: "anthropic".to_string(),
            active_model: "claude-3-5-sonnet-20241022".to_string(),
            theme: "dark".to_string(),
            editor_font_size: 14,
            editor_tab_size: 2,
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
        let content = serde_json::to_string_pretty(&settings)
            .map_err(|e| e.to_string())?;

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

    pub fn update_provider(&self, name: String, updates: ProviderConfig) {
        let mut settings = self.settings.write();
        if let Some(provider) = settings.providers.iter_mut().find(|p| p.name == name) {
            *provider = updates;
        }
    }

    pub fn set_active_provider(&self, name: String, model: String) {
        let mut settings = self.settings.write();
        settings.active_provider = name;
        settings.active_model = model;
    }

    pub fn get_provider(&self, name: &str) -> Option<Provider> {
        let settings = self.settings.read();
        settings.providers.iter()
            .find(|p| p.name == name)
            .map(|p| Provider {
                name: p.name.clone(),
                model: p.model.clone(),
                api_key: p.api_key.clone(),
                base_url: p.base_url.clone(),
            })
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