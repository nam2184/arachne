use std::path::Path;
use std::sync::Arc;

pub struct FileService;

impl FileService {
    pub fn read_file(path: &Path) -> Result<String, String> {
        std::fs::read_to_string(path).map_err(|e| format!("Failed to read file: {}", e))
    }

    pub fn write_file(path: &Path, content: &str) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create directory: {}", e))?;
        }
        std::fs::write(path, content).map_err(|e| format!("Failed to write file: {}", e))
    }

    pub fn list_directory(path: &Path) -> Result<Vec<FileEntry>, String> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(path).map_err(|e| format!("Failed to read directory: {}", e))? {
            let entry = entry.map_err(|e| e.to_string())?;
            let metadata = entry.metadata().map_err(|e| e.to_string())?;
            entries.push(FileEntry {
                name: entry.file_name().to_string_lossy().to_string(),
                path: entry.path().to_string_lossy().to_string(),
                is_dir: metadata.is_dir(),
                size: metadata.len(),
            });
        }
        entries.sort_by(|a, b| {
            match (a.is_dir, b.is_dir) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            }
        });
        Ok(entries)
    }

    pub fn search_files(path: &Path, pattern: &str) -> Result<Vec<String>, String> {
        let mut matches = Vec::new();
        let pattern_lower = pattern.to_lowercase();
        walkdir::WalkDir::new(path)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .for_each(|entry| {
                if let Some(name) = entry.file_name().to_str() {
                    if name.to_lowercase().contains(&pattern_lower) {
                        matches.push(entry.path().to_string_lossy().to_string());
                    }
                }
            });
        Ok(matches)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

pub fn get_file_tree(path: &Path, max_depth: usize) -> Result<FileTreeNode, String> {
    build_tree_node(path, 0, max_depth)
}

fn build_tree_node(path: &Path, current_depth: usize, max_depth: usize) -> Result<FileTreeNode, String> {
    let name = path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();

    let mut children = Vec::new();
    if path.is_dir() && current_depth < max_depth {
        if let Ok(entries) = std::fs::read_dir(path) {
            let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
            entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
            for entry in entries {
                if let Ok(child) = build_tree_node(&entry.path(), current_depth + 1, max_depth) {
                    children.push(child);
                }
            }
        }
    }

    Ok(FileTreeNode {
        name,
        path: path.to_string_lossy().to_string(),
        children: if path.is_dir() { Some(children) } else { None },
    })
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FileTreeNode {
    pub name: String,
    pub path: String,
    pub children: Option<Vec<FileTreeNode>>,
}