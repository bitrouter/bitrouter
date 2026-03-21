//! Error types for the A2A gateway.

/// Errors that can occur during A2A gateway operations.
#[derive(Debug, thiserror::Error)]
pub enum A2aGatewayError {
    /// Failed to connect to upstream A2A agent.
    #[error("upstream '{name}' connection failed: {reason}")]
    UpstreamConnect { name: String, reason: String },

    /// Upstream A2A agent call failed.
    #[error("upstream '{name}' call failed: {reason}")]
    UpstreamCall { name: String, reason: String },

    /// Upstream A2A agent closed.
    #[error("upstream '{name}' closed")]
    UpstreamClosed { name: String },

    /// Agent not found.
    #[error("agent not found: {name}")]
    AgentNotFound { name: String },

    /// Invalid configuration.
    #[error("invalid config: {reason}")]
    InvalidConfig { reason: String },

    /// A2A client request error.
    #[error("client error: {0}")]
    Client(String),
}
