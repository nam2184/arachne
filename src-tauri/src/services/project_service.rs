use std::path::{Path, PathBuf};
use std::sync::Arc;
use crate::domain::Project;
use crate::services::stack_detector::StackDetector;

pub struct ProjectService {
    projects: parking_lot::RwLock<std::collections::HashMap<String, Project>>,
    stack_detector: Arc<StackDetector>,
}

impl ProjectService {
    pub fn new(stack_detector: Arc<StackDetector>) -> Arc<Self> {
        Arc::new(Self {
            projects: parking_lot::RwLock::new(std::collections::HashMap::new()),
            stack_detector,
        })
    }

    pub fn open_project(&self, path: &Path) -> Result<Project, String> {
        if !path.exists() {
            return Err(format!("Path does not exist: {}", path.display()));
        }
        if !path.is_dir() {
            return Err(format!("Path is not a directory: {}", path.display()));
        }

        let name = path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Unknown")
            .to_string();

        let tech_stack = self.stack_detector.detect(path);

        let mut project = Project::new(path.to_string_lossy().to_string(), name);
        project.tech_stack = tech_stack.languages;

        let id = project.id.clone();
        self.projects.write().insert(id.clone(), project.clone());

        Ok(project)
    }

    pub fn get_project(&self, id: &str) -> Option<Project> {
        self.projects.read().get(id).cloned()
    }

    pub fn list_projects(&self) -> Vec<Project> {
        self.projects.read().values().cloned().collect()
    }

    pub fn close_project(&self, id: &str) -> bool {
        self.projects.write().remove(id).is_some()
    }

    pub fn refresh_stack(&self, id: &str) -> Result<Vec<String>, String> {
        let mut projects = self.projects.write();
        if let Some(project) = projects.get_mut(id) {
            let path = PathBuf::from(&project.path);
            if path.exists() {
                let stack = self.stack_detector.detect(&path);
                project.tech_stack = stack.languages;
                return Ok(project.tech_stack.clone());
            }
        }
        Err("Project not found".to_string())
    }
}