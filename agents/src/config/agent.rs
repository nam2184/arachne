use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::permission_v2::config::{PermissionConfigValue, PermissionRuleValue};
use crate::permission_v2::{default_ruleset, PermissionRuleset};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub default_role: Option<String>,
    #[serde(default)]
    pub roles: BTreeMap<String, AgentRoleConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentRoleConfig {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub permission: Option<PermissionConfigValue>,
    #[serde(default)]
    pub tools: AgentRoleToolsConfig,
    #[serde(default)]
    pub compaction: RuntimeCompactionConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRoleToolsConfig {
    #[serde(default)]
    pub readonly: Option<bool>,
    #[serde(default)]
    pub disabled: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCompactionConfig {
    #[serde(default)]
    pub auto: Option<bool>,
    #[serde(default)]
    pub buffer_tokens: Option<usize>,
    #[serde(default)]
    pub keep_recent_tokens: Option<usize>,
}

impl AgentConfig {
    pub fn merge(&mut self, next: Self) {
        if next.default_role.is_some() {
            self.default_role = next.default_role;
        }
        for (name, role) in next.roles {
            self.roles
                .entry(name)
                .and_modify(|existing| existing.merge(role.clone()))
                .or_insert(role);
        }
    }

    pub fn permission_ruleset_for_role(&self, role_name: &str) -> PermissionRuleset {
        let mut ruleset = default_ruleset();
        if let Some(role) = self.roles.get(role_name) {
            if let Some(permission) = role.permission.clone() {
                let config = crate::permission_v2::config::PermissionConfigFile {
                    permission: Some(permission),
                };
                ruleset.rules.extend(config.into_ruleset().rules);
            }
        }
        ruleset
    }
}

impl AgentRoleConfig {
    pub fn merge(&mut self, next: Self) {
        if next.description.is_some() {
            self.description = next.description;
        }
        if next.system_prompt.is_some() {
            self.system_prompt = next.system_prompt;
        }
        if next.provider.is_some() {
            self.provider = next.provider;
        }
        if next.model.is_some() {
            self.model = next.model;
        }
        merge_permission(&mut self.permission, next.permission);
        self.tools.merge(next.tools);
        self.compaction.merge(next.compaction);
    }
}

pub fn merge_permission(
    current: &mut Option<PermissionConfigValue>,
    next: Option<PermissionConfigValue>,
) {
    let Some(next) = next else {
        return;
    };
    match (current.as_mut(), next) {
        (Some(PermissionConfigValue::PerTool(current)), PermissionConfigValue::PerTool(next)) => {
            for (permission, value) in next {
                match (current.get_mut(&permission), value) {
                    (
                        Some(PermissionRuleValue::Patterned(current_patterns)),
                        PermissionRuleValue::Patterned(next_patterns),
                    ) => current_patterns.extend(next_patterns),
                    (_, value) => {
                        current.insert(permission, value);
                    }
                }
            }
        }
        (_, next) => *current = Some(next),
    }
}

impl AgentRoleToolsConfig {
    pub fn merge(&mut self, next: Self) {
        if next.readonly.is_some() {
            self.readonly = next.readonly;
        }
        if !next.disabled.is_empty() {
            self.disabled = next.disabled;
        }
    }
}

impl RuntimeCompactionConfig {
    pub fn merge(&mut self, next: Self) {
        if next.auto.is_some() {
            self.auto = next.auto;
        }
        if next.buffer_tokens.is_some() {
            self.buffer_tokens = next.buffer_tokens;
        }
        if next.keep_recent_tokens.is_some() {
            self.keep_recent_tokens = next.keep_recent_tokens;
        }
    }
}
