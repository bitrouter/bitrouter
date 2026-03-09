use std::{io, path::PathBuf};

use thiserror::Error;

pub type Result<T> = std::result::Result<T, ConfigError>;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration file not found: {0}")]
    MissingConfig(PathBuf),
    #[error("failed to read configuration: {path}: {source}")]
    ConfigRead { path: PathBuf, source: io::Error },
    #[error("configuration parse error: {0}")]
    ConfigParse(String),
}
