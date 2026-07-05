use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ProviderProtocol;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProvidersConfig {
    #[serde(default)]
    pub default_provider: Option<String>,
    #[serde(default)]
    pub default_model: Option<String>,
    /// Non-secret provider metadata. API keys/OAuth tokens stay in the auth
    /// store unless we intentionally migrate secret storage later.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderRuntimeConfig>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderRuntimeConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub protocol: Option<ProviderProtocol>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

impl ProvidersConfig {
    pub fn merge(&mut self, next: Self) {
        if next.default_provider.is_some() {
            self.default_provider = next.default_provider;
        }
        if next.default_model.is_some() {
            self.default_model = next.default_model;
        }
        for (name, provider) in next.providers {
            self.providers
                .entry(name)
                .and_modify(|existing| existing.merge(provider.clone()))
                .or_insert(provider);
        }
    }
}

impl ProviderRuntimeConfig {
    pub fn merge(&mut self, next: Self) {
        if next.model.is_some() {
            self.model = next.model;
        }
        if next.base_url.is_some() {
            self.base_url = next.base_url;
        }
        if next.protocol.is_some() {
            self.protocol = next.protocol;
        }
        if next.enabled.is_some() {
            self.enabled = next.enabled;
        }
    }
}
