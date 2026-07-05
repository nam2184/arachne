pub mod agent;
pub mod mcp;
pub mod providers;
pub mod ui;
pub mod websearch;

use std::path::Path;

use serde::{Deserialize, Serialize};

pub use agent::{
    merge_permission, AgentConfig, AgentRoleConfig, AgentRoleToolsConfig, RuntimeCompactionConfig,
};
pub use mcp::{McpConfig, McpServerConfig, McpTransport};
pub use providers::{ProviderRuntimeConfig, ProvidersConfig};
pub use ui::{AppConfig, UiConfig};
pub use websearch::RuntimeWebSearchConfig;

use crate::paths;
use crate::permission_v2::config::PermissionConfigValue;
use crate::permission_v2::PermissionRuleset;

pub const CONFIG_PRECEDENCE: &[&str] = &[
    "defaults",
    "global/user config.json",
    "project .arachne/config.json",
    "app settings UI overrides",
];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeConfigFile {
    #[serde(default, alias = "app")]
    pub ui: UiConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub providers: ProvidersConfig,
    #[serde(default)]
    pub permission: Option<PermissionConfigValue>,
    #[serde(default)]
    pub compaction: RuntimeCompactionConfig,
    #[serde(default)]
    pub websearch: RuntimeWebSearchConfig,
    #[serde(default)]
    pub mcp: McpConfig,
}

pub type RuntimeConfig = RuntimeConfigFile;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeConfigTraceSummary {
    pub ui_fields: Vec<&'static str>,
    pub agent_default_role: Option<String>,
    pub agent_roles: Vec<String>,
    pub agent_role_permissions: Vec<String>,
    pub provider_fields: Vec<&'static str>,
    pub provider_names: Vec<String>,
    pub provider_base_url_count: usize,
    pub permission_configured: bool,
    pub compaction_fields: Vec<&'static str>,
    pub websearch_fields: Vec<&'static str>,
    pub mcp_servers: Vec<String>,
    pub mcp_enabled_servers: Vec<String>,
    pub mcp_disabled_servers: Vec<String>,
    pub mcp_stdio_count: usize,
    pub mcp_streamable_http_count: usize,
    pub mcp_sse_count: usize,
    pub mcp_polling_http_count: usize,
}

impl RuntimeConfigFile {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw =
            std::fs::read_to_string(path).map_err(|error| format!("read {path:?}: {error}"))?;
        serde_json::from_str(&raw).map_err(|error| format!("parse {path:?}: {error}"))
    }

    pub fn load_global() -> Result<Self, String> {
        Self::load(paths::config_file())
    }

    pub fn load_project(project_root: impl AsRef<Path>) -> Result<Self, String> {
        Self::load(paths::project_config_file(project_root))
    }

    pub fn load_default(project_root: impl AsRef<Path>) -> Result<Self, String> {
        let mut config = Self::load_global()?;
        config.merge(Self::load_project(project_root)?);
        Ok(config)
    }

    pub fn merge(&mut self, next: Self) {
        self.ui.merge(next.ui);
        self.agent.merge(next.agent);
        self.providers.merge(next.providers);
        merge_permission(&mut self.permission, next.permission);
        self.compaction.merge(next.compaction);
        self.websearch.merge(next.websearch);
        self.mcp.merge(next.mcp);
    }

    pub fn permission_ruleset(&self) -> PermissionRuleset {
        crate::permission_v2::ruleset_from_runtime_config(self)
    }

    pub fn permission_ruleset_for_role(&self, role_name: &str) -> PermissionRuleset {
        crate::permission_v2::ruleset_from_runtime_config_for_role(self, role_name)
    }

    pub fn trace_summary(&self) -> RuntimeConfigTraceSummary {
        let mut ui_fields = Vec::new();
        push_if_some(&mut ui_fields, "theme", &self.ui.theme);
        push_if_some(
            &mut ui_fields,
            "editor_font_size",
            &self.ui.editor_font_size,
        );
        push_if_some(&mut ui_fields, "editor_tab_size", &self.ui.editor_tab_size);
        push_if_some(&mut ui_fields, "node_skin", &self.ui.node_skin);
        push_if_some(&mut ui_fields, "workspace_mode", &self.ui.workspace_mode);
        push_if_some(
            &mut ui_fields,
            "code_block_theme",
            &self.ui.code_block_theme,
        );
        push_if_some(&mut ui_fields, "cursor_theme", &self.ui.cursor_theme);

        let mut provider_fields = Vec::new();
        push_if_some(
            &mut provider_fields,
            "default_provider",
            &self.providers.default_provider,
        );
        push_if_some(
            &mut provider_fields,
            "default_model",
            &self.providers.default_model,
        );

        let mut compaction_fields = Vec::new();
        push_if_some(&mut compaction_fields, "auto", &self.compaction.auto);
        push_if_some(
            &mut compaction_fields,
            "buffer_tokens",
            &self.compaction.buffer_tokens,
        );
        push_if_some(
            &mut compaction_fields,
            "keep_recent_tokens",
            &self.compaction.keep_recent_tokens,
        );

        let mut websearch_fields = Vec::new();
        push_if_some(
            &mut websearch_fields,
            "searxng_base_url",
            &self.websearch.searxng_base_url,
        );
        push_if_some(
            &mut websearch_fields,
            "max_results",
            &self.websearch.max_results,
        );

        let agent_roles: Vec<String> = self.agent.roles.keys().cloned().collect();
        let agent_role_permissions: Vec<String> = self
            .agent
            .roles
            .iter()
            .filter_map(|(name, role)| role.permission.as_ref().map(|_| name.clone()))
            .collect();
        let provider_names: Vec<String> = self.providers.providers.keys().cloned().collect();
        let provider_base_url_count = self
            .providers
            .providers
            .values()
            .filter(|provider| provider.base_url.is_some())
            .count();

        let mcp_servers: Vec<String> = self.mcp.servers.keys().cloned().collect();
        let mcp_enabled_servers: Vec<String> = self
            .mcp
            .servers
            .iter()
            .filter_map(|(name, server)| server.enabled.then(|| name.clone()))
            .collect();
        let mcp_disabled_servers: Vec<String> = self
            .mcp
            .servers
            .iter()
            .filter_map(|(name, server)| (!server.enabled).then(|| name.clone()))
            .collect();
        let mcp_stdio_count = self
            .mcp
            .servers
            .values()
            .filter(|server| server.transport == McpTransport::Stdio)
            .count();
        let mcp_streamable_http_count = self
            .mcp
            .servers
            .values()
            .filter(|server| server.transport == McpTransport::StreamableHttp)
            .count();
        let mcp_sse_count = self
            .mcp
            .servers
            .values()
            .filter(|server| server.transport == McpTransport::Sse)
            .count();
        let mcp_polling_http_count = self
            .mcp
            .servers
            .values()
            .filter(|server| server.transport == McpTransport::PollingHttp)
            .count();

        RuntimeConfigTraceSummary {
            ui_fields,
            agent_default_role: self.agent.default_role.clone(),
            agent_roles,
            agent_role_permissions,
            provider_fields,
            provider_names,
            provider_base_url_count,
            permission_configured: self.permission.is_some(),
            compaction_fields,
            websearch_fields,
            mcp_servers,
            mcp_enabled_servers,
            mcp_disabled_servers,
            mcp_stdio_count,
            mcp_streamable_http_count,
            mcp_sse_count,
            mcp_polling_http_count,
        }
    }
}

fn push_if_some<T>(fields: &mut Vec<&'static str>, name: &'static str, value: &Option<T>) {
    if value.is_some() {
        fields.push(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mcp_stdio_server() {
        let raw = r#"{
          "mcp": {
            "servers": {
              "fs": {
                "transport": "stdio",
                "command": "npx",
                "args": ["-y", "server"],
                "env": { "A": "B" }
              }
            }
          }
        }"#;

        let config: RuntimeConfigFile = serde_json::from_str(raw).unwrap();
        let server = config.mcp.servers.get("fs").unwrap();
        assert!(server.enabled);
        assert_eq!(server.transport, McpTransport::Stdio);
        assert_eq!(server.command.as_deref(), Some("npx"));
        assert_eq!(server.args, vec!["-y", "server"]);
        assert_eq!(server.env.get("A").map(String::as_str), Some("B"));
    }

    #[test]
    fn parses_explicit_mcp_transports_and_legacy_http_alias() {
        let raw = r#"{
          "mcp": {
            "servers": {
              "streamable": { "transport": "streamable_http", "url": "https://example.test/mcp" },
              "legacy_alias": { "transport": "http", "url": "https://example.test/mcp" },
              "sse": { "transport": "sse", "url": "https://example.test/sse" },
              "polling": { "transport": "polling_http", "url": "https://example.test/rpc" }
            }
          }
        }"#;

        let config: RuntimeConfigFile = serde_json::from_str(raw).unwrap();

        assert_eq!(
            config.mcp.servers["streamable"].transport,
            McpTransport::StreamableHttp
        );
        assert_eq!(
            config.mcp.servers["legacy_alias"].transport,
            McpTransport::StreamableHttp
        );
        assert_eq!(config.mcp.servers["sse"].transport, McpTransport::Sse);
        assert_eq!(
            config.mcp.servers["polling"].transport,
            McpTransport::PollingHttp
        );
    }

    #[test]
    fn parses_user_facing_config_sections() {
        let raw = r#"{
          "ui": {
            "theme": "light",
            "cursor_theme": "windows-black",
            "workspace_mode": "agent"
          },
          "agent": {
            "default_role": "builder",
            "roles": {
              "builder": {
                "description": "Default coding agent",
                "model": "gpt-5.5",
                "permission": { "bash": { "git *": "allow" } }
              },
              "reviewer": {
                "tools": { "readonly": true },
                "permission": { "*": "ask", "read": "allow" }
              }
            }
          },
          "providers": {
            "default_provider": "openai",
            "default_model": "gpt-5.5",
            "providers": {
              "openai": {
                "model": "gpt-5.5",
                "base_url": "https://api.openai.com/v1",
                "protocol": "openai",
                "enabled": true
              }
            }
          },
          "permission": { "webfetch": "ask" },
          "compaction": { "auto": false, "buffer_tokens": 12000 }
        }"#;

        let config: RuntimeConfigFile = serde_json::from_str(raw).unwrap();

        assert_eq!(config.ui.theme.as_deref(), Some("light"));
        assert_eq!(config.ui.cursor_theme.as_deref(), Some("windows-black"));
        assert_eq!(config.agent.default_role.as_deref(), Some("builder"));
        assert!(config.agent.roles.contains_key("reviewer"));
        assert_eq!(config.providers.default_provider.as_deref(), Some("openai"));
        assert_eq!(config.compaction.auto, Some(false));
        assert_eq!(config.compaction.buffer_tokens, Some(12000));
        assert!(config.permission.is_some());
    }

    #[test]
    fn app_alias_still_parses_as_ui_config() {
        let raw = r#"{ "app": { "theme": "light" } }"#;
        let config: RuntimeConfigFile = serde_json::from_str(raw).unwrap();
        assert_eq!(config.ui.theme.as_deref(), Some("light"));
    }

    #[test]
    fn merge_project_overrides_global_websearch_and_mcp() {
        let mut global = RuntimeConfigFile {
            websearch: RuntimeWebSearchConfig {
                searxng_base_url: Some("https://global.example".to_string()),
                max_results: Some(5),
            },
            ..RuntimeConfigFile::default()
        };
        global
            .mcp
            .servers
            .insert("global".to_string(), McpServerConfig::default());

        let mut project = RuntimeConfigFile {
            websearch: RuntimeWebSearchConfig {
                searxng_base_url: Some("https://project.example".to_string()),
                max_results: None,
            },
            ..RuntimeConfigFile::default()
        };
        project
            .mcp
            .servers
            .insert("project".to_string(), McpServerConfig::default());

        global.merge(project);

        assert_eq!(
            global.websearch.searxng_base_url.as_deref(),
            Some("https://project.example")
        );
        assert_eq!(global.websearch.max_results, Some(5));
        assert!(global.mcp.servers.contains_key("global"));
        assert!(global.mcp.servers.contains_key("project"));
    }

    #[test]
    fn merge_layers_user_facing_config_by_priority() {
        let mut global = RuntimeConfigFile::default();
        global.ui.theme = Some("dark".to_string());
        global.ui.cursor_theme = Some("react-flow".to_string());
        global.providers.default_provider = Some("anthropic".to_string());
        global.compaction.auto = Some(true);

        let mut project = RuntimeConfigFile::default();
        project.ui.cursor_theme = Some("windows-black".to_string());
        project.providers.default_model = Some("project-model".to_string());
        project.compaction.auto = Some(false);

        global.merge(project);

        assert_eq!(global.ui.theme.as_deref(), Some("dark"));
        assert_eq!(global.ui.cursor_theme.as_deref(), Some("windows-black"));
        assert_eq!(
            global.providers.default_provider.as_deref(),
            Some("anthropic")
        );
        assert_eq!(
            global.providers.default_model.as_deref(),
            Some("project-model")
        );
        assert_eq!(global.compaction.auto, Some(false));
    }

    #[test]
    fn merge_agent_roles_by_name() {
        let mut global: RuntimeConfigFile = serde_json::from_str(
            r#"{ "agent": { "roles": { "reviewer": { "description": "Review", "tools": { "readonly": true } } } } }"#,
        )
        .unwrap();
        let project: RuntimeConfigFile = serde_json::from_str(
            r#"{ "agent": { "roles": { "reviewer": { "model": "review-model", "permission": { "read": "allow" } } } } }"#,
        )
        .unwrap();

        global.merge(project);
        let reviewer = global.agent.roles.get("reviewer").unwrap();

        assert_eq!(reviewer.description.as_deref(), Some("Review"));
        assert_eq!(reviewer.model.as_deref(), Some("review-model"));
        assert_eq!(reviewer.tools.readonly, Some(true));
        assert!(reviewer.permission.is_some());
    }

    #[test]
    fn merge_permission_maps_in_priority_order() {
        let mut global: RuntimeConfigFile = serde_json::from_str(
            r#"{ "permission": { "bash": { "*": "ask", "git *": "allow" } } }"#,
        )
        .unwrap();
        let project: RuntimeConfigFile = serde_json::from_str(
            r#"{ "permission": { "bash": { "git push *": "deny" }, "webfetch": "ask" } }"#,
        )
        .unwrap();

        global.merge(project);
        let ruleset = global.permission_ruleset();

        assert_eq!(
            ruleset.evaluate("bash", "git status").action,
            crate::permission_v2::PermissionAction::Allow
        );
        assert_eq!(
            ruleset.evaluate("bash", "git push origin main").action,
            crate::permission_v2::PermissionAction::Deny
        );
        assert_eq!(
            ruleset.evaluate("webfetch", "https://example.com").action,
            crate::permission_v2::PermissionAction::Ask
        );
    }

    #[test]
    fn role_permission_overrides_global_permission() {
        let config: RuntimeConfigFile = serde_json::from_str(
            r#"{
              "permission": { "bash": "deny" },
              "agent": {
                "roles": {
                  "builder": { "permission": { "bash": { "git *": "allow" } } }
                }
              }
            }"#,
        )
        .unwrap();

        let ruleset = config.permission_ruleset_for_role("builder");

        assert_eq!(
            ruleset.evaluate("bash", "git status").action,
            crate::permission_v2::PermissionAction::Allow
        );
        assert_eq!(
            ruleset.evaluate("bash", "rm -rf /tmp/nope").action,
            crate::permission_v2::PermissionAction::Deny
        );
    }
}
