use std::io;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, RuntimeError>;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("unsupported operation: {0}")]
    Unsupported(&'static str),
    #[error("daemon error: {0}")]
    Daemon(String),
    #[error(transparent)]
    Config(#[from] bitrouter_config::ConfigError),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}
