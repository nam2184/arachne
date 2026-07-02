use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::database::{Database, ProviderAuthStateRepository, ProviderConfigRepository};
use crate::llm::providers::{
    aisdk_provider_base_url_env, aisdk_provider_model_env, aisdk_supported_provider_names,
};
use crate::{
    ProviderAuthFieldType, ProviderAuthState, ProviderConfig, ProviderOAuthAuthorization,
    ProviderProtocol,
};

use crate::provider_oauth::ProviderOAuthCoordinator;

pub struct ProviderService {
    db_path: PathBuf,
    configs: RwLock<Vec<ProviderConfig>>,
    oauth: ProviderOAuthCoordinator,
}

impl ProviderService {
    pub fn new(db_path: PathBuf) -> Arc<Self> {
        let service = Arc::new(Self {
            db_path,
            configs: RwLock::new(Vec::new()),
            oauth: ProviderOAuthCoordinator::default(),
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
            oauth: ProviderOAuthCoordinator::default(),
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
            provider_config("openai", "gpt-5.5", ProviderProtocol::OpenAI, true),
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
        self.seed_auth_states_from_legacy_api_keys(&db, &configs)?;
        let auth_states = ProviderAuthStateRepository::list(&db)?
            .into_iter()
            .map(normalize_auth_state)
            .collect::<Vec<_>>();
        for config in &mut configs {
            if let Some(auth) = auth_states
                .iter()
                .find(|auth| auth.provider_name == config.name)
            {
                config.auth_field_type = auth.field_type.clone();
                config.api_key = auth.selected_token().and_then(non_empty);
                config.auth_account_id = auth.account_id.clone().and_then(non_empty);
                tracing::debug!(
                    provider = %config.name,
                    auth_type = %auth.field_type.as_str(),
                    has_selected_token = config.api_key.is_some(),
                    has_account_id = config.auth_account_id.is_some(),
                    selected_token_source = auth_token_source(auth),
                    "loaded provider auth token into runtime config"
                );
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

    pub fn get_auth_states(&self) -> Result<Vec<ProviderAuthState>, String> {
        let db = self.db()?;
        Ok(ProviderAuthStateRepository::list(&db)?
            .into_iter()
            .map(normalize_auth_state)
            .collect())
    }

    pub fn get_auth_state(&self, provider_name: &str) -> Result<ProviderAuthState, String> {
        let db = self.db()?;
        if let Some(auth) = ProviderAuthStateRepository::find_by_provider(&db, provider_name)? {
            return Ok(normalize_auth_state(auth));
        }

        let mut auth = ProviderAuthState::new(provider_name.to_string());
        auth.api_key = ProviderConfigRepository::find_by_name(&db, provider_name)?
            .and_then(|config| config.api_key)
            .and_then(non_empty);
        Ok(auth)
    }

    pub fn upsert_auth_state(&self, auth: ProviderAuthState) -> Result<(), String> {
        let auth = normalize_auth_state(auth);
        let db = self.db()?;
        ProviderAuthStateRepository::upsert(&db, &auth)?;
        {
            let mut configs = self.configs.write();
            if let Some(config) = configs
                .iter_mut()
                .find(|config| config.name == auth.provider_name)
            {
                config.auth_field_type = auth.field_type.clone();
                config.api_key = auth.selected_token().and_then(non_empty);
                config.auth_account_id = auth.account_id.clone().and_then(non_empty);
                tracing::debug!(
                    provider = %config.name,
                    auth_type = %auth.field_type.as_str(),
                    has_selected_token = config.api_key.is_some(),
                    has_account_id = config.auth_account_id.is_some(),
                    selected_token_source = auth_token_source(&auth),
                    "updated provider auth token in runtime config"
                );
            }
        }
        Ok(())
    }

    pub fn update_auth_settings(
        &self,
        provider_name: &str,
        field_type: ProviderAuthFieldType,
        api_key: Option<String>,
    ) -> Result<(), String> {
        let mut auth = self.get_auth_state(provider_name)?;
        auth.field_type = field_type;
        auth.api_key = api_key.and_then(non_empty);
        self.upsert_auth_state(auth)
    }

    pub async fn start_oauth(
        &self,
        provider_name: &str,
    ) -> Result<ProviderOAuthAuthorization, String> {
        self.oauth.start(provider_name).await
    }

    pub async fn complete_oauth(&self, provider_name: &str) -> Result<ProviderAuthState, String> {
        let tokens = self.oauth.complete(provider_name).await?;
        let mut auth = self.get_auth_state(provider_name)?;
        auth.field_type = ProviderAuthFieldType::OAuth;
        auth.access_token = Some(tokens.access_token);
        auth.refresh_token = tokens.refresh_token;
        auth.account_id = tokens.account_id;
        tracing::debug!(
            provider = %provider_name,
            has_access_token = auth.access_token.is_some(),
            has_refresh_token = auth.refresh_token.is_some(),
            has_account_id = auth.account_id.is_some(),
            "completed provider oauth and received token set"
        );
        self.upsert_auth_state(auth.clone())?;
        Ok(auth)
    }

    pub fn pending_oauth(&self, provider_name: &str) -> Option<ProviderOAuthAuthorization> {
        self.oauth.pending_authorization(provider_name)
    }

    pub fn upsert_config(&self, config: ProviderConfig) -> Result<(), String> {
        let mut config = normalize_config(config);
        if !self.db_path.as_os_str().is_empty() {
            let db = self.db()?;
            if let Some(auth) = ProviderAuthStateRepository::find_by_provider(&db, &config.name)? {
                let auth = normalize_auth_state(auth);
                config.auth_field_type = auth.field_type.clone();
                config.api_key = auth.selected_token().and_then(non_empty);
                config.auth_account_id = auth.account_id.clone().and_then(non_empty);
            }
        }
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
            ProviderAuthStateRepository::delete(&db, name)?;
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

    fn seed_auth_states_from_legacy_api_keys(
        &self,
        db: &Database,
        configs: &[ProviderConfig],
    ) -> Result<(), String> {
        for config in configs {
            if config.api_key.is_none() {
                continue;
            }
            if ProviderAuthStateRepository::find_by_provider(db, &config.name)?.is_some() {
                continue;
            }
            let mut auth = ProviderAuthState::new(config.name.clone());
            auth.api_key = config.api_key.clone();
            ProviderAuthStateRepository::upsert(db, &auth)?;
        }
        Ok(())
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
    config.auth_account_id = config.auth_account_id.and_then(non_empty);
    if config.model.trim().is_empty() {
        config.model = default_model_for_provider(&config.name);
    }
    config
}

fn normalize_auth_state(mut auth: ProviderAuthState) -> ProviderAuthState {
    auth.provider_name = auth.provider_name.trim().to_string();
    auth.access_token = auth.access_token.and_then(non_empty);
    auth.refresh_token = auth.refresh_token.and_then(non_empty);
    auth.account_id = auth.account_id.and_then(non_empty);
    if auth.account_id.is_none() && auth.provider_name.eq_ignore_ascii_case("openai") {
        auth.account_id = auth
            .access_token
            .as_deref()
            .and_then(crate::provider_oauth::extract_openai_account_id_from_jwt);
    }
    auth.api_key = auth.api_key.and_then(non_empty);
    auth
}

fn auth_token_source(auth: &ProviderAuthState) -> &'static str {
    match auth.field_type {
        ProviderAuthFieldType::ApiKey => "api_key",
        ProviderAuthFieldType::OAuth => "oauth_access_token",
    }
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
            oauth: ProviderOAuthCoordinator::default(),
        }
    }
}
