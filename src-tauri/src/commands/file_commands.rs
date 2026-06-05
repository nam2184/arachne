use std::path::Path;
use crate::services::file_service::{FileService, FileEntry};
use crate::services::file_service::get_file_tree as build_file_tree;
use crate::services::file_service::FileTreeNode;

#[tauri::command]
pub async fn read_file(path: String) -> Result<String, String> {
    FileService::read_file(Path::new(&path))
}

#[tauri::command]
pub async fn write_file(path: String, content: String) -> Result<(), String> {
    FileService::write_file(Path::new(&path), &content)
}

#[tauri::command]
pub async fn list_directory(path: String) -> Result<Vec<FileEntry>, String> {
    FileService::list_directory(Path::new(&path))
}

#[tauri::command]
pub async fn search_files(path: String, pattern: String) -> Result<Vec<String>, String> {
    FileService::search_files(Path::new(&path), &pattern)
}

#[tauri::command]
pub async fn get_file_tree(path: String, max_depth: Option<usize>) -> Result<FileTreeNode, String> {
    let depth = max_depth.unwrap_or(3);
    build_file_tree(Path::new(&path), depth)
}