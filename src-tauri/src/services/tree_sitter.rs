use std::path::Path;
use std::sync::Arc;
use tree_sitter::{Language, Parser, Tree};
use tree_sitter_cli::LANGUAGE;
use serde::{Deserialize, Serialize};
use parking_lot::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParseConfig {
    pub max_file_size: usize,
    pub timeout_ms: u64,
    pub enabled_languages: Vec<String>,
}

impl Default for ParseConfig {
    fn default() -> Self {
        Self {
            max_file_size: 1024 * 1024,
            timeout_ms: 5000,
            enabled_languages: vec![
                "rust".to_string(),
                "typescript".to_string(),
                "javascript".to_string(),
                "python".to_string(),
                "go".to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParseResult {
    pub tree: Option<String>,
    pub root_node: Option<String>,
    pub language: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyntaxNode {
    pub kind: String,
    pub text: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub children: Vec<SyntaxNode>,
}

pub struct TreeSitterService {
    parser: RwLock<Parser>,
    config: RwLock<ParseConfig>,
    language_cache: RwLock<std::collections::HashMap<String, Language>>,
}

impl TreeSitterService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn configure(&self, config: ParseConfig) {
        let mut cfg = self.config.write();
        *cfg = config;
    }

    pub fn get_language(lang: &str) -> Option<Language> {
        match lang.to_lowercase().as_str() {
            "rust" => Some(tree_sitter_rust::LANGUAGE.into()),
            "typescript" | "tsx" => Some(tree_sitter_typescript::LANGUAGE.into()),
            "javascript" | "jsx" | "mjs" => Some(tree_sitter_javascript::LANGUAGE.into()),
            "python" => Some(tree_sitter_python::LANGUAGE.into()),
            "go" => Some(tree_sitter_go::LANGUAGE.into()),
            "c" | "cpp" => Some(tree_sitter_c::LANGUAGE.into()),
            "java" => Some(tree_sitter_java::LANGUAGE.into()),
            "ruby" => Some(tree_sitter_ruby::LANGUAGE.into()),
            "php" => Some(tree_sitter_php::LANGUAGE.into()),
            "html" => Some(tree_sitter_html::LANGUAGE.into()),
            "css" => Some(tree_sitter_css::LANGUAGE.into()),
            "json" => Some(tree_sitter_json::LANGUAGE.into()),
            "toml" => Some(tree_sitter_toml::LANGUAGE.into()),
            _ => None,
        }
    }

    pub fn parse_file(&self, path: &Path, content: &str) -> Result<ParseResult, String> {
        let ext = path.extension()?.to_str().unwrap_or("");
        let lang_name = self.extension_to_language(ext);

        let language = Self::get_language(&lang_name).ok_or_else(|| {
            format!("Unsupported language for extension: {}", ext)
        })?;

        let config = self.config.read();
        if content.len() > config.max_file_size {
            return Err(format!("File too large: {} bytes", content.len()));
        }
        drop(config);

        let mut parser = self.parser.write();
        parser.set_language(&language).map_err(|e| e.to_string())?;

        let tree = parser
            .parse(content, None)
            .ok_or_else(|| "Failed to parse file".to_string())?;

        let root_node = tree.root_node();
        let syntax_tree = self.node_to_syntax_node(&root_node, content);

        Ok(ParseResult {
            tree: Some(syntax_tree.to_string()),
            root_node: Some(root_node.kind().to_string()),
            language: Some(lang_name),
            error: None,
        })
    }

    pub fn parse_content(&self, content: &str, language: &str) -> Result<ParseResult, String> {
        let lang = Self::get_language(language).ok_or_else(|| {
            format!("Unsupported language: {}", language)
        })?;

        let mut parser = self.parser.write();
        parser.set_language(&lang).map_err(|e| e.to_string())?;

        let tree = parser
            .parse(content, None)
            .ok_or_else(|| "Failed to parse content".to_string())?;

        let root_node = tree.root_node();
        let syntax_tree = self.node_to_syntax_node(&root_node, content);

        Ok(ParseResult {
            tree: Some(syntax_tree.to_string()),
            root_node: Some(root_node.kind().to_string()),
            language: Some(language.to_string()),
            error: None,
        })
    }

    fn node_to_syntax_node(&self, node: &tree_sitter::Node, content: &str) -> SyntaxNode {
        let mut children = Vec::new();
        let mut cursor = node.walk();

        for child in node.children(&mut cursor) {
            children.push(self.node_to_syntax_node(&child, content));
        }

        SyntaxNode {
            kind: node.kind().to_string(),
            text: node.utf8_text(content.as_bytes()).unwrap_or_default().to_string(),
            start_byte: node.start_byte(),
            end_byte: node.end_byte(),
            children,
        }
    }

    pub fn query_functions(&self, content: &str, language: &str) -> Result<Vec<String>, String> {
        let lang = Self::get_language(language).ok_or_else(|| {
            format!("Unsupported language: {}", language)
        })?;

        let mut parser = self.parser.write();
        parser.set_language(&lang).map_err(|e| e.to_string())?;

        let tree = parser
            .parse(content, None)
            .ok_or_else(|| "Failed to parse content".to_string())?;

        let mut functions = Vec::new();
        let mut cursor = tree.walk();

        for node in tree.root_node().children(&mut cursor) {
            if node.kind().contains("function") || node.kind().contains("method") {
                if let Some(name) = self.extract_function_name(&node, content) {
                    functions.push(name);
                }
            }
        }

        Ok(functions)
    }

    fn extract_function_name(&self, node: &tree_sitter::Node, content: &str) -> Option<String> {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            let kind = child.kind();
            if kind == "identifier" || kind == "property_identifier" {
                return Some(child.utf8_text(content.as_bytes()).unwrap_or_default().to_string());
            }
        }
        None
    }

    fn extension_to_language(&self, ext: &str) -> String {
        match ext.to_lowercase().as_str() {
            "rs" => "rust".to_string(),
            "ts" | "tsx" | "mts" => "typescript".to_string(),
            "js" | "jsx" | "mjs" => "javascript".to_string(),
            "py" => "python".to_string(),
            "go" => "go".to_string(),
            "java" => "java".to_string(),
            "rb" => "ruby".to_string(),
            "php" => "php".to_string(),
            "cs" => "c_sharp".to_string(),
            "c" => "c".to_string(),
            "cpp" | "cc" | "cxx" | "h" | "hpp" => "cpp".to_string(),
            "html" | "htm" => "html".to_string(),
            "css" => "css".to_string(),
            "json" => "json".to_string(),
            "toml" => "toml".to_string(),
            "yaml" | "yml" => "yaml".to_string(),
            "md" => "markdown".to_string(),
            "sql" => "sql".to_string(),
            "sh" | "bash" => "bash".to_string(),
            _ => ext.to_string(),
        }
    }
}

impl Default for TreeSitterService {
    fn default() -> Self {
        Self {
            parser: RwLock::new(Parser::new()),
            config: RwLock::new(ParseConfig::default()),
            language_cache: RwLock::new(std::collections::HashMap::new()),
        }
    }
}
