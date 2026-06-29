use std::path::{Path, PathBuf};

const QUALIFIER: &str = "ai";
const ORGANIZATION: &str = "arachne";
const APPLICATION: &str = "arachne";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArachneDirs {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
}

pub fn app_dirs() -> ArachneDirs {
    if let Some(dirs) = directories::ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION) {
        return ArachneDirs {
            config_dir: dirs.config_dir().to_path_buf(),
            data_dir: dirs.data_dir().to_path_buf(),
            cache_dir: dirs.cache_dir().to_path_buf(),
        };
    }

    let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    ArachneDirs {
        config_dir: base.join("config"),
        data_dir: base.join("data"),
        cache_dir: base.join("cache"),
    }
}

pub fn config_dir() -> PathBuf {
    app_dirs().config_dir
}

pub fn data_dir() -> PathBuf {
    app_dirs().data_dir
}

pub fn cache_dir() -> PathBuf {
    app_dirs().cache_dir
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.json")
}

pub fn settings_file() -> PathBuf {
    config_dir().join("settings.json")
}

pub fn log_file() -> PathBuf {
    data_dir().join("logs").join("arachne.log")
}

pub fn project_config_file(project_root: impl AsRef<Path>) -> PathBuf {
    project_root.as_ref().join(".arachne").join("config.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_config_file_uses_dot_arachne() {
        assert_eq!(
            project_config_file(Path::new("/repo")),
            PathBuf::from("/repo").join(".arachne").join("config.json")
        );
    }
}
