use arachne_agents::{
    ProviderAuthFieldType, ProviderAuthState, ProviderConfig, ProviderOAuthAuthorization,
    ProviderOAuthProfile, ProviderService,
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
    profile_label: Option<String>,
    provider_service: State<'_, Arc<ProviderService>>,
) -> Result<ProviderOAuthProfile, String> {
    provider_service
        .complete_oauth(&provider_name, profile_label)
        .await
}

#[tauri::command]
pub async fn list_provider_oauth_profiles(
    provider_name: String,
    provider_service: State<'_, Arc<ProviderService>>,
) -> Result<Vec<ProviderOAuthProfile>, String> {
    provider_service.list_oauth_profiles(&provider_name)
}

#[tauri::command]
pub async fn set_active_provider_oauth_profile(
    provider_name: String,
    profile_id: String,
    provider_service: State<'_, Arc<ProviderService>>,
) -> Result<ProviderOAuthProfile, String> {
    provider_service.set_active_oauth_profile(&provider_name, &profile_id)
}

#[tauri::command]
pub async fn rename_provider_oauth_profile(
    profile_id: String,
    label: String,
    provider_service: State<'_, Arc<ProviderService>>,
) -> Result<(), String> {
    provider_service.rename_oauth_profile(&profile_id, &label)
}

#[tauri::command]
pub async fn delete_provider_oauth_profile(
    profile_id: String,
    provider_service: State<'_, Arc<ProviderService>>,
) -> Result<(), String> {
    provider_service.delete_oauth_profile(&profile_id)
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
