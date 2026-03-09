use std::path::{Path, PathBuf};

/// Paths derived from the config file location (not serialized).
#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub config_file: PathBuf,
    pub runtime_dir: PathBuf,
    pub log_dir: PathBuf,
}

impl RuntimePaths {
    pub fn from_config_path(config_file: impl Into<PathBuf>) -> Self {
        let config_file = config_file.into();
        let base = config_file.parent().unwrap_or_else(|| Path::new("."));
        Self {
            runtime_dir: base.join("run"),
            log_dir: base.join("logs"),
            config_file,
        }
    }
}
