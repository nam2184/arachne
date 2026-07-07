use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use lsp_types::{
    CodeAction, CodeActionKind, CompletionItem, CompletionItemKind, Diagnostic,
    DiagnosticRelatedInformation, DiagnosticSeverity, DocumentSymbol, DocumentSymbolResponse,
    GotoDefinitionResponse, Hover, HoverContents, Location, LocationLink, MarkupContent,
    MarkupKind, Position, PositionEncodingKind, Range, SymbolInformation, SymbolKind, TextEdit,
    Uri, WorkspaceEdit,
};

/// Stable response envelope for the `lsp` tool.
///
/// The inner values intentionally use `lsp-types` so the JSON matches the
/// Language Server Protocol instead of a local approximation. Producers can
/// populate the subset that matches the requested action.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspToolOutput {
    pub schema: LspOutputSchema,
    pub action: LspAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<Uri>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbols: Option<DocumentSymbolResponse>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub definitions: Option<GotoDefinitionResponse>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<Location>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hover: Option<Hover>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completions: Vec<CompletionItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub code_actions: Vec<CodeAction>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

impl LspToolOutput {
    pub fn new(action: LspAction) -> Self {
        Self {
            schema: LspOutputSchema::V1,
            action,
            ..Self::default()
        }
    }

    pub fn for_file(action: LspAction, path: impl Into<String>) -> Self {
        Self {
            path: Some(path.into()),
            ..Self::new(action)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspOutputSchema {
    V1,
}

impl Default for LspOutputSchema {
    fn default() -> Self {
        Self::V1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspAction {
    Symbols,
    Diagnostics,
    Workspace,
    Definition,
    References,
    Hover,
    Completion,
    CodeAction,
    Document,
}

impl Default for LspAction {
    fn default() -> Self {
        Self::Document
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn serializes_sparse_symbol_output_as_lsp_json() {
        let mut output = LspToolOutput::for_file(LspAction::Symbols, "src/lib.rs");
        output.language_id = Some("rust".to_string());
        output.uri = Some(Uri::from_str("file:///repo/src/lib.rs").expect("valid file uri"));
        output.symbols = Some(DocumentSymbolResponse::Nested(vec![DocumentSymbol {
            name: "run".to_string(),
            detail: Some("fn run()".to_string()),
            kind: SymbolKind::FUNCTION,
            tags: None,
            #[allow(deprecated)]
            deprecated: None,
            range: Range::new(Position::new(10, 0), Position::new(12, 1)),
            selection_range: Range::new(Position::new(10, 3), Position::new(10, 6)),
            children: None,
        }]));

        let value = serde_json::to_value(output).expect("serializes lsp output");
        assert_eq!(value["schema"], "v1");
        assert_eq!(value["action"], "symbols");
        assert_eq!(value["path"], "src/lib.rs");
        assert_eq!(value["uri"], "file:///repo/src/lib.rs");
        assert_eq!(value["languageId"], "rust");
        assert_eq!(value["symbols"][0]["name"], "run");
        assert_eq!(value["symbols"][0]["kind"], 12);
        assert!(value.get("diagnostics").is_none());
    }
}
