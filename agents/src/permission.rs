use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::ToolCall;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    Plan,
    Build,
}

impl Default for PermissionMode {
    fn default() -> Self {
        // Plan by default — the user has to explicitly switch to build
        // mode to allow mutating tools. This is the safer starting
        // point and matches the chat UI's default toggle position.
        Self::Plan
    }
}

impl fmt::Display for PermissionMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PermissionMode::Plan => write!(formatter, "plan"),
            PermissionMode::Build => write!(formatter, "build"),
        }
    }
}

impl FromStr for PermissionMode {
    type Err = PermissionParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "plan" | "read" | "readonly" | "read_only" => Ok(Self::Plan),
            "build" | "edit" | "write" => Ok(Self::Build),
            _ => Err(PermissionParseError {
                value: value.to_string(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionParseError {
    pub value: String,
}

impl fmt::Display for PermissionParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unknown permission mode: {}", self.value)
    }
}

impl std::error::Error for PermissionParseError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionAction {
    Read,
    Search,
    Context,
    Edit,
    Execute,
    Session,
    Network,
    UserPrompt,
    ExternalDirectory,
    Mode,
    Invalid,
}

impl PermissionAction {
    pub fn allows_in_plan(self) -> bool {
        matches!(
            self,
            PermissionAction::Read
                | PermissionAction::Search
                | PermissionAction::Context
                | PermissionAction::Mode
                | PermissionAction::Invalid
        )
    }
}

impl fmt::Display for PermissionAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PermissionAction::Read => write!(formatter, "read"),
            PermissionAction::Search => write!(formatter, "search"),
            PermissionAction::Context => write!(formatter, "context"),
            PermissionAction::Edit => write!(formatter, "edit"),
            PermissionAction::Execute => write!(formatter, "execute"),
            PermissionAction::Session => write!(formatter, "session"),
            PermissionAction::Network => write!(formatter, "network"),
            PermissionAction::UserPrompt => write!(formatter, "user_prompt"),
            PermissionAction::ExternalDirectory => write!(formatter, "external_directory"),
            PermissionAction::Mode => write!(formatter, "mode"),
            PermissionAction::Invalid => write!(formatter, "invalid"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    pub tool: String,
    pub action: PermissionAction,
    pub resources: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Deny { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionError {
    pub mode: PermissionMode,
    pub request: PermissionRequest,
    pub reason: String,
}

impl fmt::Display for PermissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "permission denied: {} requires {} mode or explicit approval (current mode: {})",
            self.request.tool, self.request.action, self.mode
        )
    }
}

impl std::error::Error for PermissionError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PermissionService {
    mode: PermissionMode,
}

impl PermissionService {
    pub fn new(mode: PermissionMode) -> Self {
        Self { mode }
    }

    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    pub fn assert_tool_call(&self, call: &ToolCall) -> Result<PermissionRequest, PermissionError> {
        let request = request_for_tool_call(call);
        match self.evaluate(&request) {
            PermissionDecision::Allow => Ok(request),
            PermissionDecision::Deny { reason } => Err(PermissionError {
                mode: self.mode,
                request,
                reason,
            }),
        }
    }

    pub fn evaluate(&self, request: &PermissionRequest) -> PermissionDecision {
        if self.mode == PermissionMode::Build || request.action.allows_in_plan() {
            return PermissionDecision::Allow;
        }

        PermissionDecision::Deny {
            reason: format!(
                "{} is blocked in plan mode because it is not read-only",
                request.action
            ),
        }
    }
}

pub fn request_for_tool_call(call: &ToolCall) -> PermissionRequest {
    PermissionRequest {
        tool: call.name.clone(),
        action: action_for_tool(&call.name),
        resources: resources_for_tool_call(call),
    }
}

pub fn action_for_tool(tool: &str) -> PermissionAction {
    match tool {
        "read" | "read_file" => PermissionAction::Read,
        "glob" | "grep" | "search_files" => PermissionAction::Search,
        "lsp" => PermissionAction::Context,
        "tree_sitter" => PermissionAction::Context,
        "ghidra" => PermissionAction::Context,
        "write" | "write_file" | "edit" | "apply_patch" => PermissionAction::Edit,
        "shell" | "bash" => PermissionAction::Execute,
        "task" | "skill" | "todo" | "todowrite" => PermissionAction::Session,
        "webfetch" | "websearch" => PermissionAction::Network,
        "question" => PermissionAction::UserPrompt,
        "external_directory" => PermissionAction::ExternalDirectory,
        "plan" => PermissionAction::Mode,
        "invalid" => PermissionAction::Invalid,
        _ => PermissionAction::Invalid,
    }
}

fn resources_for_tool_call(call: &ToolCall) -> Vec<String> {
    let keys = match action_for_tool(&call.name) {
        PermissionAction::Read | PermissionAction::Search | PermissionAction::Edit => {
            &["path", "filePath", "file_path", "pattern"][..]
        }
        PermissionAction::Execute => &["command"][..],
        PermissionAction::Network => &["url", "query"][..],
        PermissionAction::ExternalDirectory => &["path", "directory"][..],
        _ => &[][..],
    };

    keys.iter()
        .filter_map(|key| call.arguments.get(*key).and_then(|value| value.as_str()))
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::str::FromStr;

    use serde_json::json;

    use crate::permission::{PermissionAction, PermissionMode, PermissionService};
    use crate::ToolCall;

    #[test]
    fn parses_permission_modes() {
        assert_eq!(
            PermissionMode::from_str("plan").unwrap(),
            PermissionMode::Plan
        );
        assert_eq!(
            PermissionMode::from_str("build").unwrap(),
            PermissionMode::Build
        );
        assert_eq!(
            PermissionMode::from_str("read_only").unwrap(),
            PermissionMode::Plan
        );
    }

    #[test]
    fn plan_mode_allows_read_only_tools() {
        let service = PermissionService::new(PermissionMode::Plan);
        assert!(service
            .assert_tool_call(&call("read", &[("path", "src/lib.rs")]))
            .is_ok());
        assert!(service
            .assert_tool_call(&call("grep", &[("pattern", "TODO")]))
            .is_ok());
    }

    #[test]
    fn plan_mode_denies_mutating_tools() {
        let service = PermissionService::new(PermissionMode::Plan);
        let error = service
            .assert_tool_call(&call("apply_patch", &[]))
            .unwrap_err();
        assert_eq!(error.request.action, PermissionAction::Edit);
    }

    #[test]
    fn build_mode_allows_mutating_tools() {
        let service = PermissionService::new(PermissionMode::Build);
        assert!(service
            .assert_tool_call(&call("edit", &[("path", "src/lib.rs")]))
            .is_ok());
    }

    fn call(name: &str, args: &[(&str, &str)]) -> ToolCall {
        ToolCall {
            name: name.to_string(),
            arguments: args
                .iter()
                .map(|(key, value)| (key.to_string(), json!(value)))
                .collect::<HashMap<_, _>>(),
        }
    }
}
