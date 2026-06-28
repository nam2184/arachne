use std::collections::HashMap;

use serde::{Deserialize, Serialize};

pub const DEFAULT_CONTEXT_WINDOW: usize = 128_000;
pub const DEFAULT_MAX_OUTPUT: usize = 8_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelSpec {
    pub id: String,
    pub context_window: usize,
    pub max_output: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ProviderSpecFile {
    #[serde(flatten)]
    providers: HashMap<String, Vec<ModelSpec>>,
}

#[derive(Debug, Clone)]
pub struct ModelRegistry {
    specs: HashMap<(String, String), ModelSpec>,
    fallback: ModelSpec,
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::with_fallback(ModelSpec {
            id: String::new(),
            context_window: DEFAULT_CONTEXT_WINDOW,
            max_output: DEFAULT_MAX_OUTPUT,
        })
    }
}

impl ModelRegistry {
    pub fn with_fallback(fallback: ModelSpec) -> Self {
        Self {
            specs: HashMap::new(),
            fallback,
        }
    }

    pub fn from_embedded_json() -> Self {
        let raw = include_str!("../../config/provider-models.json");
        match serde_json::from_str::<ProviderSpecFile>(raw) {
            Ok(file) => {
                let mut specs = HashMap::new();
                for (provider, models) in file.providers {
                    for model in models {
                        specs.insert((provider.clone(), model.id.clone()), model);
                    }
                }
                Self {
                    specs,
                    fallback: ModelSpec {
                        id: String::new(),
                        context_window: DEFAULT_CONTEXT_WINDOW,
                        max_output: DEFAULT_MAX_OUTPUT,
                    },
                }
            }
            Err(error) => {
                tracing::warn!("Failed to parse provider-models.json: {}", error);
                Self::default()
            }
        }
    }

    pub fn from_specs(specs: Vec<(String, ModelSpec)>, fallback: ModelSpec) -> Self {
        let map: HashMap<(String, String), ModelSpec> = specs
            .into_iter()
            .map(|(provider, spec)| ((provider, spec.id.clone()), spec))
            .collect();
        Self {
            specs: map,
            fallback,
        }
    }

    pub fn lookup(&self, provider: &str, model: &str) -> &ModelSpec {
        self.specs
            .get(&(provider.to_string(), model.to_string()))
            .unwrap_or(&self.fallback)
    }

    pub fn try_lookup(&self, provider: &str, model: &str) -> Option<&ModelSpec> {
        self.specs.get(&(provider.to_string(), model.to_string()))
    }

    pub fn fallback(&self) -> &ModelSpec {
        &self.fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_registry() -> ModelRegistry {
        let fallback = ModelSpec {
            id: "fallback".to_string(),
            context_window: 32_000,
            max_output: 2_000,
        };
        let specs = vec![
            (
                "anthropic".to_string(),
                ModelSpec {
                    id: "claude-opus-4-8".to_string(),
                    context_window: 1_000_000,
                    max_output: 128_000,
                },
            ),
            (
                "anthropic".to_string(),
                ModelSpec {
                    id: "claude-3-5-haiku-20241022".to_string(),
                    context_window: 200_000,
                    max_output: 8_192,
                },
            ),
            (
                "openai".to_string(),
                ModelSpec {
                    id: "gpt-4.1".to_string(),
                    context_window: 1_048_576,
                    max_output: 32_768,
                },
            ),
        ];
        ModelRegistry::from_specs(specs, fallback)
    }

    #[test]
    fn lookup_returns_specific_spec() {
        let registry = sample_registry();
        let spec = registry.lookup("anthropic", "claude-opus-4-8");
        assert_eq!(spec.context_window, 1_000_000);
        assert_eq!(spec.max_output, 128_000);
    }

    #[test]
    fn lookup_falls_back_when_model_unknown() {
        let registry = sample_registry();
        let spec = registry.lookup("anthropic", "unknown-model");
        assert_eq!(spec.id, "fallback");
        assert_eq!(spec.context_window, 32_000);
    }

    #[test]
    fn lookup_falls_back_when_provider_unknown() {
        let registry = sample_registry();
        let spec = registry.lookup("replicate", "llama-3");
        assert_eq!(spec.id, "fallback");
    }

    #[test]
    fn try_lookup_returns_none_for_unknown() {
        let registry = sample_registry();
        assert!(registry
            .try_lookup("anthropic", "claude-opus-4-8")
            .is_some());
        assert!(registry.try_lookup("anthropic", "unknown").is_none());
    }

    #[test]
    fn embedded_json_loads_specs() {
        let registry = ModelRegistry::from_embedded_json();
        let spec = registry.try_lookup("anthropic", "claude-opus-4-8");
        assert!(spec.is_some(), "claude-opus-4-8 should be in embedded JSON");
        let spec = spec.unwrap();
        assert!(spec.context_window >= 200_000);
        assert!(spec.max_output > 0);
    }

    #[test]
    fn embedded_json_has_openai_models() {
        let registry = ModelRegistry::from_embedded_json();
        assert!(registry.try_lookup("openai", "gpt-4.1").is_some());
        assert!(registry.try_lookup("openai", "o3").is_some());
    }
}
