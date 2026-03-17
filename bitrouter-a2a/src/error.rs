//! Error types for A2A operations.

/// Errors that can occur during A2A agent card operations.
#[derive(Debug, thiserror::Error)]
pub enum A2aError {
    /// Agent card not found.
    #[error("agent not found: {name}")]
    NotFound { name: String },

    /// Agent card already exists.
    #[error("agent already exists: {name}")]
    AlreadyExists { name: String },

    /// Invalid agent name.
    #[error("invalid agent name \"{name}\": {reason}")]
    InvalidName { name: String, reason: String },

    /// Storage I/O error.
    #[error("storage error: {0}")]
    Storage(String),

    /// A2A client request error.
    #[error("client error: {0}")]
    Client(String),
}
