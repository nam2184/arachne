use std::sync::Arc;
use crate::domain::{Message, MessageRole};
use crate::services::tree_sitter::TreeSitterService;

pub struct ContextIndexer {
    tree_sitter: Arc<TreeSitterService>,
    index: parking_lot::RwLock<std::collections::HashMap<String, CodeContext>>,
}

#[derive(Debug, Clone)]
struct CodeContext {
    functions: Vec<FunctionInfo>,
    structures: Vec<StructureInfo>,
    imports: Vec<String>,
}

#[derive(Debug, Clone)]
struct FunctionInfo {
    name: String,
    file: String,
    line: u32,
    signature: String,
}

#[derive(Debug, Clone)]
struct StructureInfo {
    name: String,
    file: String,
    line: u32,
    kind: String,
}

impl ContextIndexer {
    pub fn new(tree_sitter: Arc<TreeSitterService>) -> Arc<Self> {
        Arc::new(Self {
            tree_sitter,
            index: parking_lot::RwLock::new(std::collections::HashMap::new()),
        })
    }

    pub fn index_project(&self, project_id: String, files: Vec<(String, String)>) {
        let mut code_context = CodeContext {
            functions: Vec::new(),
            structures: Vec::new(),
            imports: Vec::new(),
        };

        for (file_path, content) in files {
            let ext = file_path.split('.').last().unwrap_or("");
            let language = ext.to_lowercase();

            if let Ok(result) = self.tree_sitter.parse_content(&content, &language) {
                if let Some(tree) = result.tree {
                    if let Ok(functions) = self.tree_sitter.query_functions(&content, &language) {
                        for func in functions {
                            code_context.functions.push(FunctionInfo {
                                name: func,
                                file: file_path.clone(),
                                line: 0,
                                signature: String::new(),
                            });
                        }
                    }
                }
            }
        }

        self.index.write().insert(project_id, code_context);
    }

    pub fn search_functions(&self, project_id: &str, query: &str) -> Vec<FunctionInfo> {
        let index = self.index.read();
        let context = index.get(project_id).map(|c| c.clone()).unwrap_or_default();

        context.functions.iter()
            .filter(|f| f.name.to_lowercase().contains(&query.to_lowercase()))
            .cloned()
            .collect()
    }

    pub fn get_context_summary(&self, project_id: &str) -> String {
        let index = self.index.read();
        let context = index.get(project_id).map(|c| c.clone()).unwrap_or_default();

        format!(
            "{} functions, {} structures",
            context.functions.len(),
            context.structures.len()
        )
    }
}

impl Default for ContextIndexer {
    fn default() -> Self {
        Self {
            tree_sitter: Arc::new(TreeSitterService::new()),
            index: parking_lot::RwLock::new(std::collections::HashMap::new()),
        }
    }
}