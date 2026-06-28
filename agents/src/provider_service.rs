use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::database::{Database, ProviderConfigRepository};
use crate::llm::providers::{
    aisdk_provider_base_url_env, aisdk_provider_model_env, aisdk_supported_provider_names,
};
use crate::{ProviderConfig, ProviderProtocol};

pub struct ProviderService {
    db_path: PathBuf,
    configs: RwLock<Vec<ProviderConfig>>,
}

impl ProviderService {
    pub fn new(db_path: PathBuf) -> Arc<Self> {
        let service = Arc::new(Self {
            db_path,
            configs: RwLock::new(Vec::new()),
        });
        if let Err(e) = service.load() {
            tracing::warn!("Failed to load provider configs: {}", e);
        }
        service
    }

    pub fn with_defaults() -> Arc<Self> {
        let service = Arc::new(Self {
            db_path: PathBuf::new(),
            configs: RwLock::new(Self::default_configs()),
        });
        service
    }

    fn default_configs() -> Vec<ProviderConfig> {
        let anthropic_model = default_model_for_provider("anthropic");
        let mut configs = vec![
            provider_config(
                "anthropic",
                &anthropic_model,
                ProviderProtocol::Anthropic,
                true,
            ),
            provider_config("openai", "gpt-4o", ProviderProtocol::OpenAI, true),
            provider_config("minimax", "MiniMax-M3", ProviderProtocol::OpenAI, true),
        ];

        for provider_name in aisdk_supported_provider_names() {
            if configs.iter().any(|config| config.name == *provider_name) {
                continue;
            }

            configs.push(provider_config(
                provider_name,
                &default_model_for_provider(provider_name),
                protocol_for_provider(provider_name),
                false,
            ));
        }

        configs
    }

    fn db(&self) -> Result<Database, String> {
        if let Some(parent) = self.db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let db = Database::new(self.db_path.clone()).map_err(|e| e.to_string())?;
        db.init()?;
        Ok(db)
    }

    pub fn load(&self) -> Result<(), String> {
        if self.db_path.as_os_str().is_empty() {
            return Ok(());
        }
        let db = self.db()?;
        let mut configs = ProviderConfigRepository::list(&db)?
            .into_iter()
            .map(normalize_config)
            .collect::<Vec<_>>();
        if configs.is_empty() {
            configs = Self::default_configs();
            for config in &configs {
                ProviderConfigRepository::upsert(&db, config)?;
            }
        } else {
            for mut default_config in Self::default_configs() {
                if configs
                    .iter()
                    .any(|config| config.name == default_config.name)
                {
                    continue;
                }

                default_config.enabled = false;
                ProviderConfigRepository::upsert(&db, &default_config)?;
                configs.push(default_config);
            }
        }
        *self.configs.write() = configs;
        Ok(())
    }

    pub fn save(&self) -> Result<(), String> {
        if self.db_path.as_os_str().is_empty() {
            return Ok(());
        }
        let configs = self.configs.read().clone();
        let db = self.db()?;
        for config in configs {
            ProviderConfigRepository::upsert(&db, &config)?;
        }
        Ok(())
    }

    pub fn get_configs(&self) -> Vec<ProviderConfig> {
        self.configs.read().clone()
    }

    pub fn get_config(&self, name: &str) -> Option<ProviderConfig> {
        self.configs.read().iter().find(|c| c.name == name).cloned()
    }

    pub fn upsert_config(&self, config: ProviderConfig) -> Result<(), String> {
        let config = normalize_config(config);
        {
            let mut configs = self.configs.write();
            if let Some(existing) = configs.iter_mut().find(|c| c.name == config.name) {
                *existing = config;
            } else {
                configs.push(config);
            }
        }
        self.save()
    }

    pub fn delete_config(&self, name: &str) -> Result<(), String> {
        {
            let mut configs = self.configs.write();
            configs.retain(|c| c.name != name);
        }
        if !self.db_path.as_os_str().is_empty() {
            let db = self.db()?;
            ProviderConfigRepository::delete(&db, name)?;
        }
        Ok(())
    }

    pub fn get_enabled(&self) -> Option<ProviderConfig> {
        self.configs.read().iter().find(|c| c.enabled).cloned()
    }

    pub fn set_enabled(&self, name: &str, enabled: bool) -> Result<(), String> {
        {
            let mut configs = self.configs.write();
            if let Some(config) = configs.iter_mut().find(|c| c.name == name) {
                config.enabled = enabled;
            }
        }
        self.save()
    }
}

fn provider_config(
    name: &str,
    model: &str,
    protocol: ProviderProtocol,
    enabled: bool,
) -> ProviderConfig {
    let mut config = ProviderConfig::new(name.to_string(), model.to_string(), protocol);
    config.enabled = enabled;
    config.base_url = env_non_empty(&aisdk_provider_base_url_env(name));
    config
}

fn normalize_config(mut config: ProviderConfig) -> ProviderConfig {
    config.base_url = config.base_url.and_then(non_empty);
    config.api_key = config.api_key.and_then(non_empty);
    if config.model.trim().is_empty() {
        config.model = default_model_for_provider(&config.name);
    }
    config
}

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().and_then(non_empty)
}

fn non_empty(value: String) -> Option<String> {
    let trimmed = value.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn default_model_for_provider(provider_name: &str) -> String {
    if provider_name == "anthropic" {
        return std::env::var(aisdk_provider_model_env(provider_name))
            .ok()
            .and_then(non_empty)
            .unwrap_or_else(|| "claude-opus-4-8".to_string());
    }

    std::env::var(aisdk_provider_model_env(provider_name))
        .ok()
        .and_then(non_empty)
        .unwrap_or_else(|| "default".to_string())
}

fn protocol_for_provider(provider_name: &str) -> ProviderProtocol {
    if provider_name == "anthropic" {
        ProviderProtocol::Anthropic
    } else {
        ProviderProtocol::OpenAI
    }
}

impl Default for ProviderService {
    fn default() -> Self {
        Self {
            db_path: PathBuf::new(),
            configs: RwLock::new(Vec::new()),
        }
    }
}
