#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UiCommandHint {
    pub name: &'static str,
    pub usage: &'static str,
    pub description: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub enum UiCommand {
    Compact,
    Help,
}

pub struct UiCommandService;

impl UiCommandService {
    pub fn new() -> Self {
        Self
    }

    pub fn hints(&self) -> Vec<UiCommandHint> {
        vec![
            UiCommandHint {
                name: "compact",
                usage: "/compact",
                description: "Compact this session's conversation context.",
            },
            UiCommandHint {
                name: "help",
                usage: "/help",
                description: "Show available UI commands.",
            },
        ]
    }

    pub fn parse(&self, input: &str) -> Result<UiCommand, String> {
        let command_line = input.trim();
        let Some(command_line) = command_line.strip_prefix('/') else {
            return Err("UI commands must start with `/`.".to_string());
        };

        let mut parts = command_line.split_whitespace();
        let command = parts
            .next()
            .ok_or_else(|| "Enter a command after `/`.".to_string())?
            .to_ascii_lowercase();
        let args: Vec<&str> = parts.collect();

        match command.as_str() {
            "compact" if args.is_empty() => Ok(UiCommand::Compact),
            "compact" => Err("Usage: /compact".to_string()),
            "help" if args.is_empty() => Ok(UiCommand::Help),
            "help" => Err("Usage: /help".to_string()),
            other => Err(format!("Unknown command: /{other}")),
        }
    }

    pub fn help_text(&self) -> String {
        self.hints()
            .into_iter()
            .map(|hint| format!("{} - {}", hint.usage, hint.description))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Default for UiCommandService {
    fn default() -> Self {
        Self::new()
    }
}
