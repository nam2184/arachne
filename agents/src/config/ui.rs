use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiConfig {
    #[serde(default)]
    pub theme: Option<String>,
    #[serde(default)]
    pub editor_font_size: Option<u32>,
    #[serde(default)]
    pub editor_tab_size: Option<u32>,
    #[serde(default)]
    pub node_skin: Option<String>,
    #[serde(default)]
    pub workspace_mode: Option<String>,
    #[serde(default)]
    pub code_block_theme: Option<String>,
    #[serde(default)]
    pub cursor_theme: Option<String>,
}

// Backwards-compatible type name for callers that still think of this as app
// settings. The config document key is `ui`.
pub type AppConfig = UiConfig;

impl UiConfig {
    pub fn merge(&mut self, next: Self) {
        if next.theme.is_some() {
            self.theme = next.theme;
        }
        if next.editor_font_size.is_some() {
            self.editor_font_size = next.editor_font_size;
        }
        if next.editor_tab_size.is_some() {
            self.editor_tab_size = next.editor_tab_size;
        }
        if next.node_skin.is_some() {
            self.node_skin = next.node_skin;
        }
        if next.workspace_mode.is_some() {
            self.workspace_mode = next.workspace_mode;
        }
        if next.code_block_theme.is_some() {
            self.code_block_theme = next.code_block_theme;
        }
        if next.cursor_theme.is_some() {
            self.cursor_theme = next.cursor_theme;
        }
    }
}
