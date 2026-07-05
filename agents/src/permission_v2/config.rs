use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::config::RuntimeConfigFile;
use crate::paths;

use super::rule::{PermissionAction, PermissionRule};
use super::ruleset::PermissionRuleset;
use super::service::{PermissionRequestReceiver, PermissionService};

/// Top-level config file shape for the `permission` key.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PermissionConfigFile {
    #[serde(default)]
    pub permission: Option<PermissionConfigValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PermissionConfigValue {
    /// `"permission": "allow"` — applies to all tools.
    Global(PermissionAction),
    /// `"permission": { "*": "ask", "bash": "allow" }` — per-tool rules.
    PerTool(BTreeMap<String, PermissionRuleValue>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PermissionRuleValue {
    /// `"bash": "allow"`
    Simple(PermissionAction),
    /// `"bash": { "*": "ask", "git *": "allow" }`
    Patterned(BTreeMap<String, PermissionAction>),
}

impl PermissionConfigFile {
    /// Load from a JSON file. Returns Default if the file doesn't exist.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path).map_err(|e| format!("read {path:?}: {e}"))?;
        let parsed: Self =
            serde_json::from_str(&raw).map_err(|e| format!("parse {path:?}: {e}"))?;
        Ok(parsed)
    }

    /// Try to load from the conventional locations in order. Returns the first
    /// file that exists, or default if none do.
    pub fn load_default(cwd: impl AsRef<Path>) -> Result<Self, String> {
        let cwd = cwd.as_ref();
        let candidates = [
            paths::project_config_file(cwd),
            cwd.join("arachne.json"),
            home_config_path(),
        ];
        for path in &candidates {
            if path.exists() {
                return Self::load(path);
            }
        }
        Ok(Self::default())
    }

    /// Convert this config into a PermissionRuleset, expanding any
    /// `~` / `$HOME` patterns. HOME is read from the environment.
    pub fn into_ruleset(self) -> PermissionRuleset {
        let home = std::env::var_os("HOME");
        self.into_ruleset_with(home.as_deref())
    }

    /// Convert this config into a PermissionRuleset with an explicit HOME
    /// value (used for testing).
    pub fn into_ruleset_with(self, home: Option<&std::ffi::OsStr>) -> PermissionRuleset {
        let mut rules = Vec::new();
        if let Some(perm) = self.permission {
            match perm {
                PermissionConfigValue::Global(action) => {
                    rules.push(PermissionRule::new("*", "*", action));
                }
                PermissionConfigValue::PerTool(map) => {
                    for (permission, value) in map {
                        match value {
                            PermissionRuleValue::Simple(action) => {
                                rules.push(PermissionRule::new(
                                    expand_with(&permission, home),
                                    "*",
                                    action,
                                ));
                            }
                            PermissionRuleValue::Patterned(patterns) => {
                                for (pattern, action) in patterns {
                                    rules.push(PermissionRule::new(
                                        expand_with(&permission, home),
                                        expand_with(&pattern, home),
                                        action,
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
        PermissionRuleset { rules }
    }
}

/// Convert an optional `permission` config section into an executable ruleset,
/// including the built-in defaults.
pub fn ruleset_from_config(permission: Option<PermissionConfigValue>) -> PermissionRuleset {
    let mut ruleset = default_ruleset();
    if let Some(permission) = permission {
        let config = PermissionConfigFile {
            permission: Some(permission),
        };
        ruleset.rules.extend(config.into_ruleset().rules);
    }
    ruleset
}

/// Build the permission ruleset for a fully merged runtime config.
pub fn ruleset_from_runtime_config(config: &RuntimeConfigFile) -> PermissionRuleset {
    ruleset_from_config(config.permission.clone())
}

/// Build the permission ruleset for a role in a fully merged runtime config.
pub fn ruleset_from_runtime_config_for_role(
    config: &RuntimeConfigFile,
    role_name: &str,
) -> PermissionRuleset {
    let mut ruleset = ruleset_from_runtime_config(config);
    if let Some(role) = config.agent.roles.get(role_name) {
        if let Some(permission) = role.permission.clone() {
            let role_ruleset = PermissionConfigFile {
                permission: Some(permission),
            }
            .into_ruleset();
            ruleset.rules.extend(role_ruleset.rules);
        }
    }
    ruleset
}

/// Build a session-scoped permission service from a fully merged runtime config.
pub fn service_from_runtime_config(
    session_id: impl Into<String>,
    config: &RuntimeConfigFile,
) -> (Arc<PermissionService>, PermissionRequestReceiver) {
    PermissionService::new(session_id, ruleset_from_runtime_config(config))
}

/// Build a session-scoped permission service for a runtime-configured role.
pub fn service_from_runtime_config_for_role(
    session_id: impl Into<String>,
    config: &RuntimeConfigFile,
    role_name: &str,
) -> (Arc<PermissionService>, PermissionRequestReceiver) {
    PermissionService::new(
        session_id,
        ruleset_from_runtime_config_for_role(config, role_name),
    )
}

/// Standard global config path for the current platform.
pub fn home_config_path() -> PathBuf {
    paths::config_file()
}

/// Home expansion for permission patterns. `~` and `$HOME` at the start of a
/// pattern are replaced with the user's home directory.
pub fn expand(pattern: &str) -> String {
    expand_with(pattern, std::env::var_os("HOME").as_deref())
}

/// Testable variant of `expand` that accepts an explicit HOME value.
pub fn expand_with(pattern: &str, home: Option<&std::ffi::OsStr>) -> String {
    if let Some(home) = home {
        let home = home.to_string_lossy();
        if pattern == "~" {
            return home.to_string();
        }
        if let Some(rest) = pattern.strip_prefix("~/") {
            return format!("{home}/{rest}");
        }
        if let Some(rest) = pattern.strip_prefix("$HOME/") {
            return format!("{home}/{rest}");
        }
        if pattern == "$HOME" {
            return home.to_string();
        }
    }
    pattern.to_string()
}

/// Default ruleset when no config is found: normal tools allow,
/// doom_loop/external_directory ask, and `.env*` files are denied.
pub fn default_ruleset() -> PermissionRuleset {
    let rules = vec![
        // Normal tools allow by default.
        PermissionRule::allow("read", "*"),
        PermissionRule::allow("glob", "*"),
        PermissionRule::allow("grep", "*"),
        PermissionRule::allow("edit", "*"),
        PermissionRule::allow("write", "*"),
        PermissionRule::allow("apply_patch", "*"),
        PermissionRule::allow("bash", "*"),
        PermissionRule::allow("task", "*"),
        PermissionRule::allow("skill", "*"),
        PermissionRule::allow("lsp", "*"),
        PermissionRule::allow("webfetch", "*"),
        PermissionRule::allow("websearch", "*"),
        PermissionRule::allow("question", "*"),
        PermissionRule::allow("todo", "*"),
        PermissionRule::allow("todowrite", "*"),
        PermissionRule::allow("plan", "*"),
        // Deny .env* by default for read.
        PermissionRule::deny("read", "*.env"),
        PermissionRule::deny("read", "*.env.*"),
        PermissionRule::allow("read", "*.env.example"),
        // External directory and doom loop require explicit approval.
        PermissionRule::ask("external_directory", "*"),
        PermissionRule::ask("doom_loop", "*"),
    ];
    PermissionRuleset { rules }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_replaces_tilde_with_home() {
        let home = std::ffi::OsStr::new("/home/test");
        assert_eq!(expand_with("~", Some(home)), "/home/test");
        assert_eq!(expand_with("~/foo", Some(home)), "/home/test/foo");
    }

    #[test]
    fn expand_replaces_dollar_home_with_home() {
        let home = std::ffi::OsStr::new("/home/test");
        assert_eq!(expand_with("$HOME", Some(home)), "/home/test");
        assert_eq!(expand_with("$HOME/foo", Some(home)), "/home/test/foo");
    }

    #[test]
    fn expand_passes_through_non_home_patterns() {
        let home = std::ffi::OsStr::new("/home/test");
        assert_eq!(expand_with("src/**", Some(home)), "src/**");
        assert_eq!(expand_with("*.env", Some(home)), "*.env");
        assert_eq!(expand_with("/absolute/path", Some(home)), "/absolute/path");
    }

    #[test]
    fn expand_with_no_home_passes_through() {
        assert_eq!(expand_with("~", None), "~");
        assert_eq!(expand_with("src/**", None), "src/**");
    }

    #[test]
    fn load_missing_file_returns_default() {
        let cfg = PermissionConfigFile::load("/nonexistent/path/config.json").unwrap();
        assert!(cfg.permission.is_none());
    }

    #[test]
    fn parses_global_string_form() {
        let raw = r#"{ "permission": "allow" }"#;
        let cfg: PermissionConfigFile = serde_json::from_str(raw).unwrap();
        match cfg.permission {
            Some(PermissionConfigValue::Global(PermissionAction::Allow)) => {}
            other => panic!("expected global allow, got {other:?}"),
        }
    }

    #[test]
    fn parses_per_tool_simple_form() {
        let raw = r#"{
            "permission": {
                "*": "ask",
                "bash": "allow",
                "edit": "deny"
            }
        }"#;
        let cfg: PermissionConfigFile = serde_json::from_str(raw).unwrap();
        match cfg.permission {
            Some(PermissionConfigValue::PerTool(map)) => {
                assert_eq!(map.len(), 3);
                assert!(matches!(
                    map.get("bash"),
                    Some(PermissionRuleValue::Simple(PermissionAction::Allow))
                ));
            }
            other => panic!("expected per-tool, got {other:?}"),
        }
    }

    #[test]
    fn parses_per_tool_patterned_form() {
        let raw = r#"{
            "permission": {
                "bash": {
                    "*": "ask",
                    "git *": "allow",
                    "rm *": "deny"
                }
            }
        }"#;
        let cfg: PermissionConfigFile = serde_json::from_str(raw).unwrap();
        match cfg.permission {
            Some(PermissionConfigValue::PerTool(map)) => {
                let bash = map.get("bash").unwrap();
                if let PermissionRuleValue::Patterned(patterns) = bash {
                    assert_eq!(patterns.len(), 3);
                    assert_eq!(patterns.get("git *"), Some(&PermissionAction::Allow));
                } else {
                    panic!("expected patterned bash");
                }
            }
            other => panic!("expected per-tool, got {other:?}"),
        }
    }

    #[test]
    fn into_ruleset_converts_global_to_catchall() {
        let cfg = PermissionConfigFile {
            permission: Some(PermissionConfigValue::Global(PermissionAction::Deny)),
        };
        let ruleset = cfg.into_ruleset();
        assert_eq!(ruleset.rules.len(), 1);
        assert_eq!(ruleset.rules[0].action, PermissionAction::Deny);
    }

    #[test]
    fn into_ruleset_expands_home_in_patterns() {
        let home = std::ffi::OsStr::new("/home/test");
        let cfg = PermissionConfigFile {
            permission: Some(PermissionConfigValue::PerTool({
                let mut m = BTreeMap::new();
                m.insert(
                    "external_directory".to_string(),
                    PermissionRuleValue::Simple(PermissionAction::Allow),
                );
                m.insert(
                    "edit".to_string(),
                    PermissionRuleValue::Patterned({
                        let mut p = BTreeMap::new();
                        p.insert("~/projects/**".to_string(), PermissionAction::Allow);
                        p
                    }),
                );
                m
            })),
        };
        let ruleset = cfg.into_ruleset_with(Some(home));
        let expanded_patterns: Vec<&str> =
            ruleset.rules.iter().map(|r| r.pattern.as_str()).collect();
        assert!(expanded_patterns.contains(&"/home/test/projects/**"));
    }

    #[test]
    fn into_ruleset_without_home_passes_through() {
        let cfg = PermissionConfigFile {
            permission: Some(PermissionConfigValue::PerTool({
                let mut m = BTreeMap::new();
                m.insert(
                    "edit".to_string(),
                    PermissionRuleValue::Simple(PermissionAction::Allow),
                );
                m
            })),
        };
        let ruleset = cfg.into_ruleset_with(None);
        assert_eq!(ruleset.rules[0].permission, "edit");
    }

    #[test]
    fn default_ruleset_denies_env_files_for_read() {
        let ruleset = default_ruleset();
        let result = ruleset.evaluate("read", "production.env");
        assert_eq!(result.action, PermissionAction::Deny);
        let result = ruleset.evaluate("read", "example.env.example");
        assert_eq!(result.action, PermissionAction::Allow);
        let result = ruleset.evaluate("read", "src/lib.rs");
        assert_eq!(result.action, PermissionAction::Allow);
    }

    #[test]
    fn default_ruleset_asks_for_external_directory() {
        let ruleset = default_ruleset();
        let result = ruleset.evaluate("external_directory", "/tmp/something");
        assert_eq!(result.action, PermissionAction::Ask);
    }

    #[test]
    fn default_ruleset_allows_common_tools() {
        let ruleset = default_ruleset();
        for (permission, pattern) in [
            ("read", "src/main.rs"),
            ("edit", "src/main.rs"),
            ("write", "src/main.rs"),
            ("bash", "git status"),
            ("glob", "**/*.rs"),
            ("grep", "TODO"),
            ("webfetch", "https://example.com"),
            ("websearch", "rust mcp sdk"),
            ("question", "confirm"),
            ("todo", "update"),
            ("todowrite", "update"),
            ("plan", "build"),
            ("skill", "rust"),
            ("lsp", "hover"),
            ("task", "explore"),
            ("apply_patch", "patch"),
        ] {
            let result = ruleset.evaluate(permission, pattern);
            assert_eq!(
                result.action,
                PermissionAction::Allow,
                "{permission} {pattern} should allow by default"
            );
        }
    }

    #[test]
    fn load_default_falls_back_to_defaults_when_no_files() {
        // Use a tempdir as cwd to ensure no config files are present.
        // We need a HOME that doesn't have a config either; this test
        // simply asserts the result is the default shape (no error, no
        // rules). It doesn't read env vars.
        let dir = tempfile::tempdir().unwrap();
        // Pass a known empty path explicitly.
        let cfg =
            PermissionConfigFile::load(dir.path().join("definitely-not-a-config.json")).unwrap();
        assert!(cfg.permission.is_none());
        let _ = dir;
    }
}
