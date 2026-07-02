use arachne_agents::{
    ProviderAuthFieldType, ProviderAuthState, ProviderConfig, ProviderOAuthAuthorization,
    ProviderService,
};
use std::sync::Arc;
use tauri::State;

#[tauri::command]
pub async fn get_provider_configs(
    provider_service: State<'_, Arc<ProviderService>>,
) -> Result<Vec<ProviderConfig>, String> {
    Ok(provider_service.get_configs())
}

#[tauri::command]
pub async fn get_provider_config(
    name: String,
    provider_service: State<'_, Arc<ProviderService>>,
) -> Result<Option<ProviderConfig>, String> {
    Ok(provider_service.get_config(&name))
}

#[tauri::command]
pub async fn get_provider_auth_states(
    provider_service: State<'_, Arc<ProviderService>>,
) -> Result<Vec<ProviderAuthState>, String> {
    provider_service.get_auth_states()
}

#[tauri::command]
pub async fn get_provider_auth_state(
    provider_name: String,
    provider_service: State<'_, Arc<ProviderService>>,
) -> Result<ProviderAuthState, String> {
    provider_service.get_auth_state(&provider_name)
}

#[tauri::command]
pub async fn upsert_provider_auth_state(
    auth: ProviderAuthState,
    provider_service: State<'_, Arc<ProviderService>>,
) -> Result<(), String> {
    provider_service.upsert_auth_state(auth)
}

#[tauri::command]
pub async fn update_provider_auth_settings(
    provider_name: String,
    field_type: ProviderAuthFieldType,
    api_key: Option<String>,
    provider_service: State<'_, Arc<ProviderService>>,
) -> Result<(), String> {
    provider_service.update_auth_settings(&provider_name, field_type, api_key)
}

#[tauri::command]
pub async fn start_provider_oauth(
    provider_name: String,
    provider_service: State<'_, Arc<ProviderService>>,
) -> Result<ProviderOAuthAuthorization, String> {
    provider_service.start_oauth(&provider_name).await
}

#[tauri::command]
pub async fn complete_provider_oauth(
    provider_name: String,
    provider_service: State<'_, Arc<ProviderService>>,
) -> Result<ProviderAuthState, String> {
    provider_service.complete_oauth(&provider_name).await
}

#[tauri::command]
pub async fn upsert_provider_config(
    config: ProviderConfig,
    provider_service: State<'_, Arc<ProviderService>>,
) -> Result<(), String> {
    provider_service.upsert_config(config)
}

#[tauri::command]
pub async fn delete_provider_config(
    name: String,
    provider_service: State<'_, Arc<ProviderService>>,
) -> Result<(), String> {
    provider_service.delete_config(&name)
}

#[tauri::command]
pub async fn set_active_provider(
    name: String,
    provider_service: State<'_, Arc<ProviderService>>,
) -> Result<(), String> {
    let configs = provider_service.get_configs();
    for config in configs {
        let enabled = config.name == name;
        provider_service.set_enabled(&config.name, enabled)?;
    }
    Ok(())
}
