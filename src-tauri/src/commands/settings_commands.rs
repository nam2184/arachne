use tauri::State;
use crate::services::settings_service::{SettingsService, AppSettings, ProviderConfig};
use std::sync::Arc;

#[tauri::command]
pub async fn get_settings(
    settings_service: State<'_, Arc<SettingsService>>,
) -> Result<AppSettings, String> {
    Ok(settings_service.get_settings())
}

#[tauri::command]
pub async fn update_provider(
    name: String,
    config: ProviderConfig,
    settings_service: State<'_, Arc<SettingsService>>,
) -> Result<(), String> {
    settings_service.update_provider(name, config);
    settings_service.save()
}

#[tauri::command]
pub async fn set_active_provider(
    name: String,
    model: String,
    settings_service: State<'_, Arc<SettingsService>>,
) -> Result<(), String> {
    settings_service.set_active_provider(name, model);
    settings_service.save()
}

#[tauri::command]
pub async fn save_settings(
    settings_service: State<'_, Arc<SettingsService>>,
) -> Result<(), String> {
    settings_service.save()
}