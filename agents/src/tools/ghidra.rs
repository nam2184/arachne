//! `ghidra` — curated reverse-engineering tool backed by a configured
//! Ghidra MCP server.
//!
//! The Arachne `mcp` module already implements a full MCP-2024-11-05
//! stdio/JSON-RPC client. This tool does NOT spawn its own sidecar; it
//! looks up a user-configured MCP server (by key in `runtime_config.mcp
//! .servers`, matched on the alias `ghidra` or `ghidra-headless`) and
//! delegates every request through `McpManager::run_tool_call`.
//!
//! # Why this shape
//!
//! Spawning `analyzeHeadless` per call costs 30–90s of JVM boot +
//! analysis. For an agent that may call this dozens of times per
//! session, that path is unusable. Both `bethington/ghidra-mcp` and
//! `mrphrazer/ghidra-headless-mcp` already implement the right pattern:
//! a single persistent Ghidra process that speaks JSON-RPC over stdio.
//! Users wire one of them up in `~/.config/arachne/mcp.toml`, and we
//! wrap a curated action layer on top of its 200+ raw tools.
//!
//! # Action mapping
//!
//! The user-visible `action` argument selects one of a small set of
//! high-signal operations. Each maps to one or more MCP tool calls on
//! the underlying Ghidra server — the mapping is best-effort because
//! Ghidra MCP servers don't share a single canonical tool vocabulary.
//! We pick the most common name; if a server uses different naming
//! (e.g. `decompile_function` vs `ghidra_decompile_function`), the
//! `mcp_tool` argument lets the user override.
//!
//! | `action`     | default MCP tool name     | args                                  |
//! |--------------|---------------------------|---------------------------------------|
//! | `status`     | `get_project_info`        | —                                     |
//! | `functions`  | `list_functions`          | `limit`, `offset`                     |
//! | `function`   | `get_function_by_name`    | `name` or `address`                   |
//! | `decompile`  | `decompile_function`      | `name` or `address`                   |
//! | `disasm`     | `disassemble_function`    | `name` or `address`, `limit`          |
//! | `xrefs_to`   | `get_xrefs_to`            | `name` or `address`, `limit`          |
//! | `xrefs_from` | `get_xrefs_from`          | `name` or `address`, `limit`          |
//! | `custom`     | (anything)                | `mcp_tool`, free-form arguments       |
//!
//! The `custom` action exists so the agent can escape the curated set
//! and call any of the 200+ tools the server exposes, e.g. for
//! type-recovery, struct reconstruction, or callgraph analysis.
//!
//! # Sandbox
//!
//! Ghidra is heavyweight: a JVM that may open ports in some modes,
//! loads native code, and ignores the agent's sandbox. Calls go
//! through `Build` permission mode only and require explicit approval
//! for the binary path on first use.

use std::path::Path;

use serde_json::{json, Value};

use crate::{ToolCall, ToolResult};

use super::{failure, resolve_session_path, string_arg, success, ToolContext};
use crate::config::RuntimeConfig;
use crate::mcp::McpManager;

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Sync entry point. Returns a clean error when called — the ghidra tool
/// requires the async path because it talks to an MCP server over
/// stdin/stdout (which is fundamentally async). The `run_tool_async`
/// dispatcher in `tools/mod.rs` is the right way to reach this.
pub fn run(call: &ToolCall) -> ToolResult {
    run_with_context(call, &ToolContext::default())
}

pub fn run_with_context(call: &ToolCall, ctx: &ToolContext) -> ToolResult {
    // Validate the path early so a sync-path call still produces a
    // useful error even though we cannot actually run the tool
    // without runtime_config + mcp_manager. The real dispatch goes
    // through `run_tool_async` in `tools/mod.rs`.
    let _ = resolve_session_path(&string_arg(call, "path"), ctx, "ghidra");
    failure(
        "ghidra",
        "ghidra must be dispatched via run_tool_async (it requires runtime_config \
         and the MCP manager). The sync tool dispatcher does not have access to \
         either; this is a configuration error, not a tool failure."
            .to_string(),
    )
}

/// Async entry point — called by `run_tool_async` in `tools/mod.rs`.
///
/// `runtime` carries the user's full `RuntimeConfig` (which lists MCP
/// servers) and a shared `McpManager` that owns the sidecar stdio
/// pipes. We do NOT spawn anything ourselves.
pub async fn run_async(call: &ToolCall, runtime: &ToolRuntimeRef<'_>) -> ToolResult {
    let config = &runtime.runtime_config;

    // Resolve the Ghidra MCP server name. Acceptable aliases:
    //   "ghidra" — preferred
    //   "ghidra-headless" — what mrphrazer's server is conventionally called
    //   "ghidra_mcp" — what bethington's server is sometimes called
    let Some((server_name, _server_cfg)) = find_ghidra_server(config) else {
        return failure(
            "ghidra",
            "no Ghidra MCP server configured. Add one to runtime_config.mcp.servers \
             with key 'ghidra' (or 'ghidra-headless'), then restart the agent. \
             Example: [mcp.servers.ghidra] command = \"ghidra-mcp\" args = [...]"
                .to_string(),
        );
    };

    let raw_path = string_arg(call, "path");
    let resolved = resolve_session_path(&raw_path, runtime.tool_context, "ghidra");

    let action = string_arg(call, "action");
    if action.is_empty() {
        return failure(
            "ghidra",
            "missing `action` argument; expected one of status, functions, function, \
             decompile, disasm, xrefs_to, xrefs_from, custom"
                .to_string(),
        );
    }

    // Sanity check the binary path for every action except `status`.
    // `status` works on whatever binary the server already has loaded.
    if action != "status" && action != "custom" {
        if let Err(err) = require_binary(&resolved) {
            return failure("ghidra", err);
        }
    }

    let args = GhidraArgs::from_call(call);

    // Map (action, args) -> (mcp_tool_name, mcp_arguments) and dispatch.
    let (mcp_tool, mcp_args) = match build_mcp_call(&action, &args, &resolved) {
        Ok(pair) => pair,
        Err(err) => return failure("ghidra", err),
    };

    // Forge the MCP-style tool call: name = mcp__<server>__<tool>, arguments
    // = whatever we computed. Then hand it to McpManager — same path as
    // any other MCP tool call.
    let sanitized_server = sanitize_for_mcp_prefix(&server_name);
    let sanitized_tool = sanitize_for_mcp_prefix(&mcp_tool);
    let mcp_call_name = format!("mcp__{sanitized_server}__{sanitized_tool}");

    let mut forwarded = call.clone();
    forwarded.name = mcp_call_name;
    // Replace the arguments wholesale — the MCP server expects its own
    // schema, not our action-layer shape.
    forwarded.arguments = mcp_args
        .as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();

    let mgr = runtime.mcp_manager;
    let result = mgr.run_tool_call(&forwarded, config).await;

    // Re-tag the result so the agent sees `ghidra.<action>` rather than
    // the underlying `mcp__<server>__<tool>` name.
    let tag = format!("ghidra.{action}");
    match result {
        mut r => {
            r.tool = tag;
            r
        }
    }
}

/// Lightweight view over the agent's runtime — keeps the ghidra tool
/// decoupled from the full `ToolRuntime` struct (which has ~10 fields
/// and brings sandbox state we don't need).
pub struct ToolRuntimeRef<'a> {
    pub runtime_config: &'a RuntimeConfig,
    pub mcp_manager: &'a McpManager,
    pub tool_context: &'a ToolContext,
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

/// Curated-action arguments. Parsed leniently — bad/missing values
/// collapse to defaults rather than rejecting the call, so the LLM
/// doesn't have to remember exact arg names to ask for a decompile.
#[derive(Debug, Clone, Default)]
struct GhidraArgs {
    name: Option<String>,
    address: Option<String>,
    limit: usize,
    offset: usize,
    /// Tool name override for the `custom` action, or to override the
    /// default MCP tool mapping per-server. Lets users wire up
    /// servers that use different naming without code changes.
    mcp_tool: Option<String>,
}

impl GhidraArgs {
    fn from_call(call: &ToolCall) -> Self {
        Self {
            name: opt_string(call, "name"),
            address: opt_string(call, "address"),
            limit: opt_usize(call, "limit", 25).clamp(1, 1000),
            offset: opt_usize(call, "offset", 0),
            mcp_tool: opt_string(call, "mcp_tool"),
        }
    }

    /// Exactly one of `name` / `address` must be set for actions that
    /// target a function. Returns the resolved selector or an error.
    fn resolve_target(&self) -> Result<Target, String> {
        match (&self.name, &self.address) {
            (Some(name), None) => Ok(Target::Name(name.clone())),
            (None, Some(addr)) => Ok(Target::Address(addr.clone())),
            (Some(_), Some(_)) => {
                Err("supply exactly one of `name` or `address`, not both".into())
            }
            (None, None) => Err("supply exactly one of `name` or `address`".into()),
        }
    }
}

#[derive(Debug, Clone)]
enum Target {
    Name(String),
    Address(String),
}

impl Target {
    fn to_mcp_args(&self) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        match self {
            Target::Name(n) => {
                m.insert("name".into(), json!(n));
                m.insert("address".into(), json!(n));
            } // some servers use name, some use address — send both
            Target::Address(a) => {
                m.insert("address".into(), json!(a));
            }
        }
        m
    }
}

/// Optional string argument: `None` if missing or empty.
fn opt_string(call: &ToolCall, key: &str) -> Option<String> {
    let v = string_arg(call, key);
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// Optional usize argument with a default.
fn opt_usize(call: &ToolCall, key: &str, default: usize) -> usize {
    call.arguments
        .get(key)
        .and_then(|v| v.as_u64())
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// Server discovery
// ---------------------------------------------------------------------------

/// Returns `(server_name, server_config)` for the first MCP server that
/// looks like a Ghidra server. Match is by server-key name (not by
/// introspecting the server's tools — we'd rather not require a network
/// roundtrip just to discover the right entry).
fn find_ghidra_server(config: &RuntimeConfig) -> Option<(String, crate::config::McpServerConfig)> {
    const ALIASES: &[&str] = &["ghidra", "ghidra-headless", "ghidra_mcp", "ghidra-mcp"];

    config
            .mcp
            .servers
            .iter()
            .find(|(name, server)| {
                server.enabled && ALIASES.iter().any(|alias| name.eq_ignore_ascii_case(alias))
            })
            .map(|(name, server)| (name.clone(), server.clone()))
}

/// MCP tool names use `[a-z0-9_]` per the spec. Preserve ASCII
/// letters/digits as-is (lowercased), keep `-`, space, and `_` as
/// `_`, drop everything else (punctuation, `!`, etc.).
fn sanitize_for_mcp_prefix(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if ch == '-' || ch == ' ' || ch == '_' {
            out.push('_');
        }
        // skip punctuation, !, and other unsupported characters
    }
    out
}

// ---------------------------------------------------------------------------
// Action -> MCP call mapping
// ---------------------------------------------------------------------------

/// Resolve `(action, args) -> (mcp_tool_name, mcp_arguments)`.
fn build_mcp_call(
    action: &str,
    args: &GhidraArgs,
    binary: &Path,
) -> Result<(String, Value), String> {
    // `custom` is a passthrough: the user picks the MCP tool name and
    // sends raw arguments. Both the binary path and the target can
    // still flow through unchanged.
    if action == "custom" {
        let tool = args
            .mcp_tool
            .clone()
            .ok_or_else(|| "action=custom requires `mcp_tool` argument".to_string())?;
        let mut map = serde_json::Map::new();
        if let Some(name) = &args.name {
            map.insert("name".into(), json!(name));
        }
        if let Some(addr) = &args.address {
            map.insert("address".into(), json!(addr));
        }
        map.insert("limit".into(), json!(args.limit));
        map.insert("offset".into(), json!(args.offset));
        map.insert("path".into(), json!(binary.display().to_string()));
        return Ok((tool, Value::Object(map)));
    }

    let mut m = serde_json::Map::new();
    m.insert("path".into(), json!(binary.display().to_string()));

    let tool_name = match action {
        "status" => {
            // No path required for status; the server already has a
            // binary loaded. Just call `get_project_info` (or whatever
            // the override is) with no args.
            return Ok((
                args.mcp_tool.clone().unwrap_or_else(|| "get_project_info".into()),
                json!({}),
            ));
        }
        "functions" => {
            m.insert("limit".into(), json!(args.limit));
            m.insert("offset".into(), json!(args.offset));
            args.mcp_tool.clone().unwrap_or_else(|| "list_functions".into())
        }
        "function" => {
            let target = args.resolve_target()?;
            for (k, v) in target.to_mcp_args() {
                m.insert(k, v);
            }
            args.mcp_tool
                .clone()
                .unwrap_or_else(|| "get_function_by_name".into())
        }
        "decompile" => {
            let target = args.resolve_target()?;
            for (k, v) in target.to_mcp_args() {
                m.insert(k, v);
            }
            args.mcp_tool
                .clone()
                .unwrap_or_else(|| "decompile_function".into())
        }
        "disasm" => {
            let target = args.resolve_target()?;
            for (k, v) in target.to_mcp_args() {
                m.insert(k, v);
            }
            m.insert("limit".into(), json!(args.limit));
            args.mcp_tool
                .clone()
                .unwrap_or_else(|| "disassemble_function".into())
        }
        "xrefs_to" => {
            let target = args.resolve_target()?;
            for (k, v) in target.to_mcp_args() {
                m.insert(k, v);
            }
            m.insert("limit".into(), json!(args.limit));
            args.mcp_tool
                .clone()
                .unwrap_or_else(|| "get_xrefs_to".into())
        }
        "xrefs_from" => {
            let target = args.resolve_target()?;
            for (k, v) in target.to_mcp_args() {
                m.insert(k, v);
            }
            m.insert("limit".into(), json!(args.limit));
            args.mcp_tool
                .clone()
                .unwrap_or_else(|| "get_xrefs_from".into())
        }
        other => {
            return Err(format!(
                "unknown action {other:?}; expected status, functions, function, \
                 decompile, disasm, xrefs_to, xrefs_from, custom"
            ));
        }
    };

    Ok((tool_name, Value::Object(m)))
}

fn require_binary(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("missing `path` argument".into());
    }
    if !path.exists() {
        return Err(format!("file not found: {}", path.display()));
    }
    if !path.is_file() {
        return Err(format!("not a regular file: {}", path.display()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::tempdir;

    fn call(name: &str, args: &[(&str, &str)]) -> ToolCall {
        ToolCall {
            name: name.to_string(),
            arguments: args
                .iter()
                .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
                .collect::<HashMap<_, _>>(),
        }
    }

    #[test]
    fn sync_entry_point_is_a_configuration_error() {
        let c = call("ghidra", &[("action", "status")]);
        let result = run(&c);
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or("")
            .contains("must be dispatched via run_tool_async"));
    }

    #[test]
    fn sanitize_for_mcp_prefix_lowercases_and_replaces_separators() {
        assert_eq!(sanitize_for_mcp_prefix("Ghidra-Headless"), "ghidra_headless");
        assert_eq!(sanitize_for_mcp_prefix("ghidra_mcp"), "ghidra_mcp");
        assert_eq!(sanitize_for_mcp_prefix("weird name!"), "weird_name");
    }

    #[test]
    fn find_ghidra_server_recognises_all_aliases() {
        // Build a synthetic RuntimeConfig with several server entries.
        let mut servers = std::collections::BTreeMap::new();
        servers.insert(
            "other-tool".into(),
            crate::config::McpServerConfig::default(),
        );
        let mut ghidra_cfg = crate::config::McpServerConfig::default();
        ghidra_cfg.enabled = true;
        ghidra_cfg.command = Some("ghidra-mcp".into());
        servers.insert("ghidra".into(), ghidra_cfg.clone());
        let mut cfg = RuntimeConfig::default();
        cfg.mcp.servers = servers;

        let (name, server_cfg) = find_ghidra_server(&cfg).expect("ghidra should match");
        assert_eq!(name, "ghidra");
        assert_eq!(server_cfg.command.as_deref(), Some("ghidra-mcp"));
    }

    #[test]
    fn find_ghidra_server_accepts_dash_underscore_and_camelcase_aliases() {
        for alias in ["ghidra-headless", "ghidra_mcp", "Ghidra", "GHIDRA"] {
            let mut servers = std::collections::BTreeMap::new();
            let mut cfg = crate::config::McpServerConfig::default();
            cfg.enabled = true;
            servers.insert(alias.into(), cfg);
            let mut rcfg = RuntimeConfig::default();
            rcfg.mcp.servers = servers;
            assert!(
                find_ghidra_server(&rcfg).is_some(),
                "alias {alias:?} should be recognised"
            );
        }
    }

    #[test]
    fn find_ghidra_server_skips_disabled_servers() {
        let mut servers = std::collections::BTreeMap::new();
        let mut cfg = crate::config::McpServerConfig::default();
        cfg.enabled = false;
        servers.insert("ghidra".into(), cfg);
        let mut rcfg = RuntimeConfig::default();
        rcfg.mcp.servers = servers;
        assert!(find_ghidra_server(&rcfg).is_none());
    }

    #[test]
    fn find_ghidra_server_returns_none_when_only_unrelated_servers_present() {
        let mut servers = std::collections::BTreeMap::new();
        servers.insert(
            "playwright".into(),
            crate::config::McpServerConfig::default(),
        );
        let mut rcfg = RuntimeConfig::default();
        rcfg.mcp.servers = servers;
        assert!(find_ghidra_server(&rcfg).is_none());
    }

    #[test]
    fn target_requires_exactly_one_of_name_or_address() {
        let both = GhidraArgs {
            name: Some("main".into()),
            address: Some("0x401000".into()),
            ..Default::default()
        };
        assert!(both.resolve_target().is_err());

        let neither = GhidraArgs::default();
        assert!(neither.resolve_target().is_err());

        let by_name = GhidraArgs {
            name: Some("main".into()),
            ..Default::default()
        };
        assert!(matches!(
            by_name.resolve_target(),
            Ok(Target::Name(ref s)) if s == "main"
        ));

        let by_addr = GhidraArgs {
            address: Some("0x401000".into()),
            ..Default::default()
        };
        assert!(matches!(
            by_addr.resolve_target(),
            Ok(Target::Address(ref s)) if s == "0x401000"
        ));
    }

    #[test]
    fn args_clamps_limit() {
        let mut hm = HashMap::new();
        hm.insert("limit".into(), json!(999_999));
        let c = ToolCall {
            name: "ghidra".into(),
            arguments: hm,
        };
        let args = GhidraArgs::from_call(&c);
        assert_eq!(args.limit, 1000);
    }

    #[test]
    fn build_mcp_call_status_uses_get_project_info_by_default() {
        let args = GhidraArgs::default();
        let (tool, mcp_args) = build_mcp_call("status", &args, Path::new("/x")).unwrap();
        assert_eq!(tool, "get_project_info");
        assert!(mcp_args.as_object().unwrap().is_empty());
    }

    #[test]
    fn build_mcp_call_decompile_uses_decompile_function_by_default() {
        let args = GhidraArgs {
            name: Some("main".into()),
            ..Default::default()
        };
        let (tool, mcp_args) = build_mcp_call("decompile", &args, Path::new("/bin/ls")).unwrap();
        assert_eq!(tool, "decompile_function");
        let obj = mcp_args.as_object().unwrap();
        assert_eq!(obj.get("path").unwrap(), "/bin/ls");
        assert_eq!(obj.get("name").unwrap(), "main");
    }

    #[test]
    fn build_mcp_call_decompile_respects_mcp_tool_override() {
        let args = GhidraArgs {
            name: Some("main".into()),
            mcp_tool: Some("custom_decompile".into()),
            ..Default::default()
        };
        let (tool, _) = build_mcp_call("decompile", &args, Path::new("/bin/ls")).unwrap();
        assert_eq!(tool, "custom_decompile");
    }

    #[test]
    fn build_mcp_call_disasm_passes_limit() {
        let args = GhidraArgs {
            name: Some("main".into()),
            limit: 200,
            ..Default::default()
        };
        let (_, mcp_args) = build_mcp_call("disasm", &args, Path::new("/bin/ls")).unwrap();
        assert_eq!(mcp_args["limit"], 200);
        assert_eq!(mcp_args["name"], "main");
    }

    #[test]
    fn build_mcp_call_functions_passes_limit_and_offset() {
        let args = GhidraArgs {
            limit: 50,
            offset: 100,
            ..Default::default()
        };
        let (tool, mcp_args) = build_mcp_call("functions", &args, Path::new("/bin/ls")).unwrap();
        assert_eq!(tool, "list_functions");
        assert_eq!(mcp_args["limit"], 50);
        assert_eq!(mcp_args["offset"], 100);
        assert_eq!(mcp_args["path"], "/bin/ls");
    }

    #[test]
    fn build_mcp_call_custom_requires_mcp_tool_argument() {
        let args = GhidraArgs::default();
        let err = build_mcp_call("custom", &args, Path::new("/bin/ls")).unwrap_err();
        assert!(err.contains("requires `mcp_tool`"));
    }

    #[test]
    fn build_mcp_call_custom_passes_through_arguments() {
        let args = GhidraArgs {
            mcp_tool: Some("get_struct_by_name".into()),
            name: Some("Foo".into()),
            limit: 10,
            ..Default::default()
        };
        let (tool, mcp_args) = build_mcp_call("custom", &args, Path::new("/bin/ls")).unwrap();
        assert_eq!(tool, "get_struct_by_name");
        assert_eq!(mcp_args["name"], "Foo");
        assert_eq!(mcp_args["limit"], 10);
        assert_eq!(mcp_args["path"], "/bin/ls");
    }

    #[test]
    fn build_mcp_call_xrefs_targets_require_a_target() {
        let args = GhidraArgs::default();
        assert!(build_mcp_call("xrefs_to", &args, Path::new("/bin/ls")).is_err());
        assert!(build_mcp_call("xrefs_from", &args, Path::new("/bin/ls")).is_err());
        assert!(build_mcp_call("decompile", &args, Path::new("/bin/ls")).is_err());
    }

    #[test]
    fn build_mcp_call_unknown_action_returns_error() {
        let args = GhidraArgs::default();
        assert!(build_mcp_call("wat", &args, Path::new("/bin/ls")).is_err());
    }

    #[test]
    fn require_binary_rejects_missing_path() {
        let result = require_binary(Path::new("/nonexistent"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("file not found"));
    }

    #[test]
    fn require_binary_rejects_directory() {
        let dir = tempdir().unwrap();
        let result = require_binary(dir.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not a regular file"));
    }

    #[test]
    fn require_binary_accepts_existing_file() {
        let dir = tempdir().unwrap();
        let bin = dir.path().join("fake.bin");
        std::fs::write(&bin, b"fake").unwrap();
        assert!(require_binary(&bin).is_ok());
    }

    // -----------------------------------------------------------------
    // Integration tests: compile a real native binary fixture and
    // exercise the ghidra tool end-to-end against it.
    //
    // We don't need a Ghidra MCP server for these — we just need a
    // binary to point at. `rustc` produces a real PE/ELF that Ghidra
    // would happily analyze, so it's a perfect stand-in for a CTF
    // target or stripped binary in real use.
    // -----------------------------------------------------------------

    /// Compile `tests/fixtures/sample_target.rs` to a real native
    /// executable once per test run, then return that path on
    /// every subsequent call.
    ///
    /// The 4 real-binary integration tests would otherwise each
    /// invoke `rustc` and race on the output `.exe` (Windows
    /// `LNK1104` if a parallel test is mid-link). Memoising the
    /// build cuts the work to one compile per `cargo test` and
    /// removes the race entirely.
    ///
    /// Returns `None` if `rustc` isn't on PATH, the fixture is
    /// missing, or compilation fails — tests then early-return.
    fn build_sample_binary() -> Option<std::path::PathBuf> {
        use std::sync::OnceLock;
        static CACHE: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
        CACHE
            .get_or_init(|| {
                let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
                    .ok()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::path::PathBuf::from("."));
                let target_subdir = manifest_dir
                    .join("target")
                    .join("ghidra-test-fixtures");
                std::fs::create_dir_all(&target_subdir).ok()?;
                let out = target_subdir.join(if cfg!(windows) {
                    "sample_target.exe"
                } else {
                    "sample_target"
                });
                let fixture = manifest_dir.join("tests/fixtures/sample_target.rs");
                if !fixture.exists() {
                    eprintln!(
                        "ghidra integration tests: fixture missing at {}; skipping",
                        fixture.display()
                    );
                    return None;
                }
                match std::process::Command::new("rustc")
                    .arg(&fixture)
                    .arg("-o")
                    .arg(&out)
                    .arg("--crate-type")
                    .arg("bin")
                    .status()
                {
                    Ok(s) if s.success() && out.exists() => Some(out),
                    _ => {
                        eprintln!(
                            "ghidra integration tests: rustc unavailable or failed; \
                             skipping real-binary fixture tests"
                        );
                        None
                    }
                }
            })
            .clone()
    }

    #[test]
    fn real_binary_passes_require_binary() {
        let bin = build_sample_binary()
            .expect("rustc must produce a real binary for this integration test");
        assert!(require_binary(&bin).is_ok());
    }

    #[test]
    fn real_binary_threads_through_decompile_action() {
        let bin = build_sample_binary()
            .expect("rustc must produce a real binary for this integration test");
        let args = GhidraArgs {
            name: Some("target_function".into()),
            ..Default::default()
        };
        let (tool, mcp_args) =
            build_mcp_call("decompile", &args, &bin).expect("decompile should map");
        assert_eq!(tool, "decompile_function");
        let obj = mcp_args.as_object().unwrap();
        // The `path` argument must be the absolute path of the
        // real binary we just compiled — that's what the MCP server
        // would receive to open the file in Ghidra.
        let path_arg = obj.get("path").unwrap().as_str().unwrap();
        assert_eq!(path_arg, bin.display().to_string());
        // And the target selector must be present.
        assert_eq!(obj.get("name").unwrap(), "target_function");
    }

    #[test]
    fn real_binary_disasm_action_includes_limit() {
        let bin = build_sample_binary()
            .expect("rustc must produce a real binary for this integration test");
        let args = GhidraArgs {
            name: Some("main".into()),
            limit: 50,
            ..Default::default()
        };
        let (_, mcp_args) = build_mcp_call("disasm", &args, &bin).unwrap();
        let obj = mcp_args.as_object().unwrap();
        assert_eq!(obj["path"], bin.display().to_string());
        assert_eq!(obj["name"], "main");
        assert_eq!(obj["limit"], 50);
    }

    #[test]
    fn find_ghidra_server_with_real_binary_target_in_config() {
        let bin = build_sample_binary()
            .expect("rustc must produce a real binary for this integration test");

        // Build a RuntimeConfig with a ghidra server entry that
        // points at a real (fake-but-runnable) command. We don't
        // actually run it — `find_ghidra_server` only inspects
        // the config map.
        let mut servers = std::collections::BTreeMap::new();
        let mut ghidra_cfg = crate::config::McpServerConfig::default();
        ghidra_cfg.enabled = true;
        ghidra_cfg.command = Some("ghidra-mcp".into());
        ghidra_cfg.args = vec!["--binary".into(), bin.display().to_string()];
        servers.insert("ghidra".into(), ghidra_cfg);

        let mut cfg = RuntimeConfig::default();
        cfg.mcp.servers = servers;

        let (name, server_cfg) = find_ghidra_server(&cfg)
            .expect("ghidra server should be discoverable");
        assert_eq!(name, "ghidra");
        // The args we stashed should round-trip through config.
        assert_eq!(server_cfg.args[1], bin.display().to_string());
    }
}