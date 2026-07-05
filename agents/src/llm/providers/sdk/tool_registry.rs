//! Per-tool registration for the AI SDK provider.
//!
//! Each tool the runner exposes to the model gets:
//!   - a `JsonSchema`-derived input struct that defines the wire
//!     shape the SDK renders on the request,
//!   - the standard AISDK `Tool::builder().name(...).description(
//!     ...).input_schema(...).execute(...)` registration,
//!   - a real executor closure that delegates to the harness-side
//!     `ToolDispatcherFn`. The dispatcher runs the v2 permission
//!     service, doom-loop detector, and sandboxed `run_tool_*`
//!     paths so every tool call still gets the harness treatment;
//!     the SDK is just doing the wire round-trip on our behalf.
//!
//! This mirrors the docs pattern:
//!
//! ```ignore
//! use aisdk::core::tools::{Tool, ToolExecute};
//! let func = ToolExecute::new(Box::new(|inp: Value| {
//!     // execute
//! }));
//! let schema = schemars::schema_for!(ToolInput);
//! let tool = Tool::builder()
//!     .name("...")
//!     .description("...")
//!     .input_schema(schema)
//!     .execute(func)
//!     .build()
//!     .unwrap();
//! ```

use std::sync::Arc;

use aisdk::core::tools::{Tool, ToolExecute};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::llm::events::ToolDefinition;
use crate::llm::providers::ToolDispatcherFn;
use crate::llm::request::LlmError;

// ---------------------------------------------------------------------------
// Input structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Read a file from disk")]
pub struct ReadInput {
    /// Path to the file to read.
    pub path: String,
    /// Line offset to start reading from (1-indexed).
    #[serde(default)]
    pub offset: Option<u64>,
    /// Maximum number of lines to read.
    #[serde(default)]
    pub limit: Option<u64>,
    /// Optional. Read from a peer session listed in `<peers>`.
    #[serde(default)]
    pub peer_session_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Write content to a file")]
pub struct WriteInput {
    /// Path to the file to write.
    pub path: String,
    /// Content to write.
    pub content: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Replace text in an existing file")]
pub struct EditInput {
    /// Path to the file to edit.
    pub path: String,
    /// Text to find and replace.
    pub old_string: String,
    /// Replacement text.
    pub new_string: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Apply a file-oriented patch")]
pub struct ApplyPatchInput {
    /// Full patch text describing file operations.
    pub patchText: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Run a shell command")]
pub struct ShellInput {
    /// Shell command to execute.
    pub command: String,
    /// Working directory for the command.
    #[serde(default)]
    pub workdir: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Find files by glob pattern")]
pub struct GlobInput {
    /// Root directory to search from.
    pub path: String,
    /// Glob pattern to match files against.
    #[serde(default)]
    pub pattern: Option<String>,
    /// Optional. Search a peer session listed in `<peers>`.
    #[serde(default)]
    pub peer_session_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Search file contents")]
pub struct GrepInput {
    /// Root directory to search from.
    pub path: String,
    /// Text pattern to search for.
    pub pattern: String,
    /// File name pattern to filter by.
    #[serde(default)]
    pub include: Option<String>,
    /// Optional. Search a peer session listed in `<peers>`.
    #[serde(default)]
    pub peer_session_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Fetch a web URL")]
pub struct WebFetchInput {
    /// URL to fetch.
    pub url: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Search the web")]
pub struct WebSearchInput {
    /// Search query.
    pub query: String,
    /// Maximum number of results (1..=20).
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Spawn a sub-agent session")]
pub struct TaskInput {
    /// Sub-agent kind: explore, build, plan, etc.
    pub subagent_type: String,
    /// Short description of what the sub-agent should do.
    pub description: String,
    /// Detailed prompt for the sub-agent.
    pub prompt: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Update the session todo list")]
pub struct TodoInput {
    /// Todo content to set.
    pub content: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(description = "Switch permission mode")]
pub struct PlanInput {
    /// Mode: plan or build.
    pub mode: String,
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Build the AISDK `Tool` for a given tool name with the harness
/// dispatcher as the executor. Returns `Err(LlmError)` if the
/// tool name is unknown to the runner, so callers can surface a
/// useful message instead of silently passing a no-op tool.
pub fn build_sdk_tool(name: &str, dispatcher: Arc<ToolDispatcherFn>) -> Result<Tool, LlmError> {
    let name_owned: String = name.to_string();
    let executor = ToolExecute::new(Box::new(move |input| {
        // The AISDK's `handle_tool_call` wraps `Err(message)` as
        // `ToolResultInfo::output = Err(message)` and feeds it
        // back to the model as a tool error. We surface the
        // dispatcher's raw `Result<String, String>` so the model
        // can self-correct.
        tracing::info!(tool = %name_owned, input = %input, "sdk tool execution started");
        let started = std::time::Instant::now();
        let result = (dispatcher.as_ref())(&name_owned, input);
        match &result {
            Ok(output) => tracing::debug!(
                tool = %name_owned,
                output = %output,
                "sdk tool execution output"
            ),
            Err(error) => tracing::debug!(
                tool = %name_owned,
                error = %error,
                "sdk tool execution error"
            ),
        }
        tracing::info!(
            tool = %name_owned,
            elapsed_ms = started.elapsed().as_millis(),
            success = result.is_ok(),
            "sdk tool execution finished"
        );
        result
    }));

    let tool = match name {
        "apply_patch" => Tool::builder()
            .name("apply_patch")
            .description("Apply a file-oriented patch.")
            .input_schema(schemars::schema_for!(ApplyPatchInput))
            .execute(executor)
            .build()
            .map_err(|error| LlmError::new("sdk_tool", &error.to_string()))?,
        "edit" => Tool::builder()
            .name("edit")
            .description("Replace text in an existing file.")
            .input_schema(schemars::schema_for!(EditInput))
            .execute(executor)
            .build()
            .map_err(|error| LlmError::new("sdk_tool", &error.to_string()))?,
        "glob" => Tool::builder()
            .name("glob")
            .description("Find files by glob pattern.")
            .input_schema(schemars::schema_for!(GlobInput))
            .execute(executor)
            .build()
            .map_err(|error| LlmError::new("sdk_tool", &error.to_string()))?,
        "grep" => Tool::builder()
            .name("grep")
            .description("Search file contents.")
            .input_schema(schemars::schema_for!(GrepInput))
            .execute(executor)
            .build()
            .map_err(|error| LlmError::new("sdk_tool", &error.to_string()))?,
        "read" => Tool::builder()
            .name("read")
            .description("Read a file from disk.")
            .input_schema(schemars::schema_for!(ReadInput))
            .execute(executor)
            .build()
            .map_err(|error| LlmError::new("sdk_tool", &error.to_string()))?,
        "shell" => Tool::builder()
            .name("shell")
            .description("Run a shell command.")
            .input_schema(schemars::schema_for!(ShellInput))
            .execute(executor)
            .build()
            .map_err(|error| LlmError::new("sdk_tool", &error.to_string()))?,
        "task" => Tool::builder()
            .name("task")
            .description("Spawn a sub-agent session.")
            .input_schema(schemars::schema_for!(TaskInput))
            .execute(executor)
            .build()
            .map_err(|error| LlmError::new("sdk_tool", &error.to_string()))?,
        "todo" => Tool::builder()
            .name("todo")
            .description("Update the session todo list.")
            .input_schema(schemars::schema_for!(TodoInput))
            .execute(executor)
            .build()
            .map_err(|error| LlmError::new("sdk_tool", &error.to_string()))?,
        "plan" => Tool::builder()
            .name("plan")
            .description("Switch permission mode.")
            .input_schema(schemars::schema_for!(PlanInput))
            .execute(executor)
            .build()
            .map_err(|error| LlmError::new("sdk_tool", &error.to_string()))?,
        "webfetch" => Tool::builder()
            .name("webfetch")
            .description("Fetch a web URL.")
            .input_schema(schemars::schema_for!(WebFetchInput))
            .execute(executor)
            .build()
            .map_err(|error| LlmError::new("sdk_tool", &error.to_string()))?,
        "websearch" => Tool::builder()
            .name("websearch")
            .description("Search the web.")
            .input_schema(schemars::schema_for!(WebSearchInput))
            .execute(executor)
            .build()
            .map_err(|error| LlmError::new("sdk_tool", &error.to_string()))?,
        "write" => Tool::builder()
            .name("write")
            .description("Write content to a file.")
            .input_schema(schemars::schema_for!(WriteInput))
            .execute(executor)
            .build()
            .map_err(|error| LlmError::new("sdk_tool", &error.to_string()))?,
        other => {
            return Err(LlmError::new(
                "sdk_unknown_tool",
                &format!("unknown tool name for SDK registration: {other}"),
            ));
        }
    };

    Ok(tool)
}

pub fn build_sdk_tool_for_definition(
    definition: &ToolDefinition,
    dispatcher: Arc<ToolDispatcherFn>,
) -> Result<Tool, LlmError> {
    if !crate::mcp::is_mcp_tool_name(&definition.name) {
        return build_sdk_tool(&definition.name, dispatcher);
    }

    let name_owned = definition.name.clone();
    let executor = ToolExecute::new(Box::new(move |input| {
        tracing::info!(tool = %name_owned, input = %input, "sdk MCP tool execution started");
        let started = std::time::Instant::now();
        let result = (dispatcher.as_ref())(&name_owned, input);
        tracing::info!(
            tool = %name_owned,
            elapsed_ms = started.elapsed().as_millis(),
            success = result.is_ok(),
            "sdk MCP tool execution finished"
        );
        result
    }));

    let schema = schemars::Schema::try_from(definition.parameters.clone())
        .unwrap_or_else(|_| schemars::Schema::default());

    Tool::builder()
        .name(definition.name.clone())
        .description(definition.description.clone())
        .input_schema(schema)
        .execute(executor)
        .build()
        .map_err(|error| LlmError::new("sdk_tool", &error.to_string()))
}
