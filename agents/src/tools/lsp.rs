use std::{collections::BTreeMap, path::Path};

use crate::{ToolCall, ToolResult};

use super::{
    failure, resolve_session_path, string_arg, success_with_metadata, usize_arg, ToolContext,
};

use output::{
    Diagnostic as LspDiagnostic, DiagnosticSeverity as LspDiagnosticSeverity, DocumentSymbol,
    DocumentSymbolResponse, LspAction, LspToolOutput, Position, Range, SymbolKind as LspSymbolKind,
    Uri,
};
use tree_sitter_language_pack::{
    detect_language_from_content, detect_language_from_path, process,
    Diagnostic as ParserDiagnostic, DiagnosticSeverity as ParserDiagnosticSeverity, ProcessConfig,
    ProcessResult, Span, StructureItem, StructureKind, SymbolInfo, SymbolKind as ParserSymbolKind,
};

pub mod output;

const WORKSPACE_DEFAULT_LIMIT: usize = 25;
const WORKSPACE_MAX_LIMIT: usize = 100;
const WORKSPACE_DEFAULT_DEPTH: usize = 6;
const WORKSPACE_MAX_DEPTH: usize = 12;
const MAX_FILE_BYTES: u64 = 512 * 1024;

pub fn run(call: &ToolCall) -> ToolResult {
    run_with_context(call, &ToolContext::default())
}

pub fn run_with_context(call: &ToolCall, ctx: &ToolContext) -> ToolResult {
    let action = string_arg(call, "action");
    match action.trim() {
        "" | "document" | "parse_file" => run_file_action(call, ctx, LspAction::Document),
        "diagnostics" => run_file_action(call, ctx, LspAction::Diagnostics),
        "symbols" => run_file_action(call, ctx, LspAction::Symbols),
        "workspace" => run_workspace(call, ctx),
        other => failure(
            "lsp",
            format!(
                "unsupported lsp action `{other}`; use document, diagnostics, symbols, or workspace"
            ),
        ),
    }
}

fn run_file_action(call: &ToolCall, ctx: &ToolContext, action: LspAction) -> ToolResult {
    let requested = string_arg(call, "path");
    if requested.trim().is_empty() {
        return failure("lsp", "path is required for file actions".to_string());
    }

    let path = resolve_session_path(&requested, ctx, "lsp");
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) => return failure("lsp", error.to_string()),
    };
    let language = match language_for(call, &path, &source) {
        Some(language) => language,
        None => {
            return failure(
                "lsp",
                format!(
                    "could not detect a tree-sitter language for {}",
                    path.display()
                ),
            )
        }
    };

    let processed = match analyze_source(&source, &language) {
        Ok(processed) => processed,
        Err(error) => return failure("lsp", error.to_string()),
    };

    let mut output = LspToolOutput::for_file(action, path.display().to_string());
    output.uri = file_uri(&path);
    output.language_id = Some(language.clone());
    output.metadata = base_metadata(&processed, &source);
    output.metadata.insert(
        "notice".to_string(),
        serde_json::json!("Static tree-sitter analysis only; diagnostics are parse-level syntax signals, not compiler or language-server diagnostics."),
    );

    if matches!(action, LspAction::Document | LspAction::Diagnostics) {
        output.diagnostics = processed
            .diagnostics
            .iter()
            .map(to_lsp_diagnostic)
            .collect();
    }
    if matches!(action, LspAction::Document | LspAction::Symbols) {
        output.symbols = Some(DocumentSymbolResponse::Nested(document_symbols(&processed)));
    }

    finish(output)
}

fn run_workspace(call: &ToolCall, ctx: &ToolContext) -> ToolResult {
    let requested = string_arg(call, "path");
    let requested = if requested.trim().is_empty() {
        ".".to_string()
    } else {
        requested
    };
    let root = resolve_session_path(&requested, ctx, "lsp");
    let limit = usize_arg(call, "limit")
        .unwrap_or(WORKSPACE_DEFAULT_LIMIT)
        .clamp(1, WORKSPACE_MAX_LIMIT);
    let max_depth = usize_arg(call, "max_depth")
        .unwrap_or(WORKSPACE_DEFAULT_DEPTH)
        .clamp(1, WORKSPACE_MAX_DEPTH);

    let mut output = LspToolOutput::new(LspAction::Workspace);
    output.path = Some(root.display().to_string());
    output.metadata.insert(
        "notice".to_string(),
        serde_json::json!("Bounded static tree-sitter workspace overview; use compiler/tests/real LSP for authoritative diagnostics."),
    );
    output
        .metadata
        .insert("limit".to_string(), serde_json::json!(limit));
    output
        .metadata
        .insert("maxDepth".to_string(), serde_json::json!(max_depth));

    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(&root)
        .follow_links(false)
        .max_depth(max_depth)
        .into_iter()
        .filter_entry(|entry| entry.depth() == 0 || !is_ignored_workspace_path(entry.path()))
        .flatten()
    {
        if !entry.file_type().is_file() {
            continue;
        }
        if entry
            .metadata()
            .map(|m| m.len() > MAX_FILE_BYTES)
            .unwrap_or(true)
        {
            continue;
        }
        if detect_language_from_path(&entry.path().to_string_lossy()).is_some() {
            files.push(entry.path().to_path_buf());
        }
        if files.len() >= limit {
            break;
        }
    }

    let mut languages: BTreeMap<String, usize> = BTreeMap::new();
    let mut file_summaries = Vec::new();
    let mut diagnostics_by_file = Vec::new();

    for path in &files {
        let source = match std::fs::read_to_string(path) {
            Ok(source) => source,
            Err(error) => {
                file_summaries.push(serde_json::json!({
                    "path": path.display().to_string(),
                    "error": error.to_string(),
                }));
                continue;
            }
        };
        let Some(language) = detect_language_from_path(&path.to_string_lossy())
            .or_else(|| detect_language_from_content(&source))
            .map(ToString::to_string)
        else {
            continue;
        };
        *languages.entry(language.clone()).or_default() += 1;
        let processed = match analyze_source(&source, &language) {
            Ok(processed) => processed,
            Err(error) => {
                file_summaries.push(serde_json::json!({
                    "path": path.display().to_string(),
                    "languageId": language,
                    "error": error.to_string(),
                }));
                continue;
            }
        };
        let diagnostics = processed
            .diagnostics
            .iter()
            .map(to_lsp_diagnostic)
            .collect::<Vec<_>>();
        output.diagnostics.extend(diagnostics.iter().cloned());
        if !diagnostics.is_empty() {
            diagnostics_by_file.push(serde_json::json!({
                "path": path.display().to_string(),
                "uri": file_uri(path).map(|uri| uri.to_string()),
                "diagnostics": diagnostics,
            }));
        }
        file_summaries.push(serde_json::json!({
            "path": path.display().to_string(),
            "uri": file_uri(path).map(|uri| uri.to_string()),
            "languageId": language,
            "lines": processed.metrics.total_lines,
            "symbols": processed.symbols.len().max(processed.structure.len()),
            "diagnostics": processed.diagnostics.len(),
        }));
    }

    output
        .metadata
        .insert("filesAnalyzed".to_string(), serde_json::json!(files.len()));
    output.metadata.insert(
        "truncated".to_string(),
        serde_json::json!(files.len() >= limit),
    );
    output
        .metadata
        .insert("languages".to_string(), serde_json::json!(languages));
    output
        .metadata
        .insert("files".to_string(), serde_json::json!(file_summaries));
    output.metadata.insert(
        "diagnosticsByFile".to_string(),
        serde_json::json!(diagnostics_by_file),
    );

    finish(output)
}

fn analyze_source(
    source: &str,
    language: &str,
) -> Result<ProcessResult, tree_sitter_language_pack::Error> {
    let mut config = ProcessConfig::new(language).all();
    config.data_extraction = false;
    process(source, &config)
}

fn language_for(call: &ToolCall, path: &Path, source: &str) -> Option<String> {
    let explicit = string_arg(call, "language_id");
    let explicit = if explicit.trim().is_empty() {
        string_arg(call, "languageId")
    } else {
        explicit
    };
    if !explicit.trim().is_empty() {
        return Some(explicit);
    }
    detect_language_from_path(&path.to_string_lossy())
        .or_else(|| detect_language_from_content(source))
        .map(ToString::to_string)
}

fn document_symbols(processed: &ProcessResult) -> Vec<DocumentSymbol> {
    if !processed.structure.is_empty() {
        return processed
            .structure
            .iter()
            .filter_map(structure_to_document_symbol)
            .collect();
    }

    processed
        .symbols
        .iter()
        .map(symbol_to_document_symbol)
        .collect()
}

fn structure_to_document_symbol(item: &StructureItem) -> Option<DocumentSymbol> {
    let name = item.name.clone()?;
    let children = item
        .children
        .iter()
        .filter_map(structure_to_document_symbol)
        .collect::<Vec<_>>();
    Some(DocumentSymbol {
        name,
        detail: item.signature.clone().or_else(|| item.visibility.clone()),
        kind: structure_kind(&item.kind),
        tags: None,
        #[allow(deprecated)]
        deprecated: None,
        range: to_lsp_range(&item.span),
        selection_range: to_lsp_range(&item.span),
        children: if children.is_empty() {
            None
        } else {
            Some(children)
        },
    })
}

fn symbol_to_document_symbol(symbol: &SymbolInfo) -> DocumentSymbol {
    DocumentSymbol {
        name: symbol.name.clone(),
        detail: symbol.type_annotation.clone(),
        kind: symbol_kind(&symbol.kind),
        tags: None,
        #[allow(deprecated)]
        deprecated: None,
        range: to_lsp_range(&symbol.span),
        selection_range: to_lsp_range(&symbol.span),
        children: None,
    }
}

fn to_lsp_diagnostic(diagnostic: &ParserDiagnostic) -> LspDiagnostic {
    LspDiagnostic::new(
        to_lsp_range(&diagnostic.span),
        Some(match diagnostic.severity {
            ParserDiagnosticSeverity::Error => LspDiagnosticSeverity::ERROR,
            ParserDiagnosticSeverity::Warning => LspDiagnosticSeverity::WARNING,
            ParserDiagnosticSeverity::Info => LspDiagnosticSeverity::INFORMATION,
        }),
        None,
        Some("tree-sitter-language-pack".to_string()),
        diagnostic.message.clone(),
        None,
        None,
    )
}

fn to_lsp_range(span: &Span) -> Range {
    Range::new(
        Position::new(to_u32(span.start_line), to_u32(span.start_column)),
        Position::new(to_u32(span.end_line), to_u32(span.end_column)),
    )
}

fn to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn structure_kind(kind: &StructureKind) -> LspSymbolKind {
    match kind {
        StructureKind::Function => LspSymbolKind::FUNCTION,
        StructureKind::Method => LspSymbolKind::METHOD,
        StructureKind::Class => LspSymbolKind::CLASS,
        StructureKind::Struct => LspSymbolKind::STRUCT,
        StructureKind::Interface => LspSymbolKind::INTERFACE,
        StructureKind::Enum => LspSymbolKind::ENUM,
        StructureKind::Module | StructureKind::Namespace => LspSymbolKind::MODULE,
        StructureKind::Trait => LspSymbolKind::INTERFACE,
        StructureKind::Impl | StructureKind::Other(_) => LspSymbolKind::OBJECT,
    }
}

fn symbol_kind(kind: &ParserSymbolKind) -> LspSymbolKind {
    match kind {
        ParserSymbolKind::Variable => LspSymbolKind::VARIABLE,
        ParserSymbolKind::Constant => LspSymbolKind::CONSTANT,
        ParserSymbolKind::Function => LspSymbolKind::FUNCTION,
        ParserSymbolKind::Class => LspSymbolKind::CLASS,
        ParserSymbolKind::Type => LspSymbolKind::TYPE_PARAMETER,
        ParserSymbolKind::Interface => LspSymbolKind::INTERFACE,
        ParserSymbolKind::Enum => LspSymbolKind::ENUM,
        ParserSymbolKind::Module => LspSymbolKind::MODULE,
        ParserSymbolKind::Other(_) => LspSymbolKind::OBJECT,
    }
}

fn base_metadata(processed: &ProcessResult, source: &str) -> BTreeMap<String, serde_json::Value> {
    BTreeMap::from([
        (
            "totalLines".to_string(),
            serde_json::json!(processed.metrics.total_lines),
        ),
        ("totalBytes".to_string(), serde_json::json!(source.len())),
        (
            "nodeCount".to_string(),
            serde_json::json!(processed.metrics.node_count),
        ),
        (
            "errorCount".to_string(),
            serde_json::json!(processed.metrics.error_count),
        ),
        (
            "imports".to_string(),
            serde_json::json!(processed.imports.len()),
        ),
        (
            "exports".to_string(),
            serde_json::json!(processed.exports.len()),
        ),
    ])
}

fn file_uri(path: &Path) -> Option<Uri> {
    let absolute = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut path = absolute.to_string_lossy().replace('\\', "/");
    if !path.starts_with('/') {
        path = format!("/{path}");
    }
    format!("file://{}", percent_encode_uri_path(&path))
        .parse()
        .ok()
}

fn percent_encode_uri_path(path: &str) -> String {
    path.bytes()
        .flat_map(|byte| match byte {
            b' ' => "%20".bytes().collect::<Vec<_>>(),
            b'#' => "%23".bytes().collect::<Vec<_>>(),
            b'%' => "%25".bytes().collect::<Vec<_>>(),
            b'?' => "%3F".bytes().collect::<Vec<_>>(),
            _ => vec![byte],
        })
        .map(char::from)
        .collect()
}

fn is_ignored_workspace_path(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    matches!(
        name,
        ".git"
            | "node_modules"
            | "dist"
            | "build"
            | "target"
            | "coverage"
            | ".next"
            | ".nuxt"
            | ".cache"
            | "vendor"
            | "generated"
            | ".venv"
            | "venv"
    )
}

fn finish(output: LspToolOutput) -> ToolResult {
    match serde_json::to_value(&output) {
        Ok(metadata) => success_with_metadata(
            "lsp",
            serde_json::to_string_pretty(&metadata).unwrap_or_else(|_| metadata.to_string()),
            metadata,
        ),
        Err(error) => failure("lsp", error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_conversion_uses_lsp_shape() {
        let diagnostic = ParserDiagnostic {
            message: "syntax error".to_string(),
            severity: ParserDiagnosticSeverity::Error,
            span: Span {
                start_line: 2,
                start_column: 4,
                end_line: 2,
                end_column: 8,
                ..Span::default()
            },
        };

        let lsp = to_lsp_diagnostic(&diagnostic);
        assert_eq!(lsp.range.start.line, 2);
        assert_eq!(lsp.range.start.character, 4);
        assert_eq!(lsp.severity, Some(LspDiagnosticSeverity::ERROR));
        assert_eq!(lsp.source.as_deref(), Some("tree-sitter-language-pack"));
    }

    #[test]
    fn file_uri_escapes_windows_paths() {
        let uri = file_uri(Path::new(r"C:\tmp\has space#x.rs")).expect("uri");
        assert_eq!(uri.to_string(), "file:///C:/tmp/has%20space%23x.rs");
    }
}
