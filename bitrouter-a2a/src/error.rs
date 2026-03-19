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

    /// Task not found.
    #[error("task not found: {id}")]
    TaskNotFound { id: String },

    /// Optimistic concurrency version conflict.
    #[error("version conflict")]
    VersionConflict,

    /// Agent execution error.
    #[error("execution error: {0}")]
    Execution(String),
}
