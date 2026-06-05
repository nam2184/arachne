use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TechStack {
    pub languages: Vec<String>,
    pub frameworks: Vec<String>,
    pub tools: Vec<String>,
}

impl TechStack {
    pub fn new() -> Self {
        Self {
            languages: Vec::new(),
            frameworks: Vec::new(),
            tools: Vec::new(),
        }
    }

    pub fn add_language(&mut self, lang: String) {
        if !self.languages.contains(&lang) {
            self.languages.push(lang);
        }
    }

    pub fn add_framework(&mut self, framework: String) {
        if !self.frameworks.contains(&framework) {
            self.frameworks.push(framework);
        }
    }

    pub fn add_tool(&mut self, tool: String) {
        if !self.tools.contains(&tool) {
            self.tools.push(tool);
        }
    }

    pub fn detect_from_files(&mut self, files: &[String]) {
        for file in files {
            let ext = file.split('.').last().unwrap_or("");
            match ext {
                "rs" => self.add_language("Rust".to_string()),
                "js" | "jsx" | "mjs" => self.add_language("JavaScript".to_string()),
                "ts" | "tsx" | "mts" => self.add_language("TypeScript".to_string()),
                "py" => self.add_language("Python".to_string()),
                "go" => self.add_language("Go".to_string()),
                "java" => self.add_language("Java".to_string()),
                "rb" => self.add_language("Ruby".to_string()),
                "php" => self.add_language("PHP".to_string()),
                "cs" => self.add_language("C#".to_string()),
                "cpp" | "cc" | "cxx" => self.add_language("C++".to_string()),
                "c" => self.add_language("C".to_string()),
                _ => {}
            }
        }
    }
}