//! Model-domain failures, independent of gateway HTTP status policy.

/// Result of a model-domain operation.
pub type Result<T> = std::result::Result<T, ModelError>;

/// A failure to satisfy the model request contract.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ModelError {
    /// A known conversion or replay-authority rule refused this boundary.
    #[error("model conversion incompatible")]
    Incompatible {
        /// Content-free diagnostics retained for the caller's policy/observers.
        report: crate::conversion::ConversionReport,
    },
    /// Invalid model configuration or request semantics.
    #[error("invalid model request: {message}")]
    InvalidRequest {
        /// Detail needed to correct the request or metadata.
        message: String,
    },
    /// A successful provider response did not satisfy its wire contract.
    #[error("invalid model response: {message}")]
    InvalidResponse {
        /// Provider diagnostic for trusted callers.
        message: String,
    },
    /// A provider reported a failure, including its native status.
    #[error("model provider error ({status}): {message}")]
    Provider {
        /// Native provider status, not the caller-facing HTTP status.
        status: u16,
        /// Provider diagnostic for trusted callers.
        message: String,
    },
    /// A provider declined the request under its content policy.
    #[error("model provider policy violation: {message}")]
    PolicyViolation {
        /// Provider diagnostic for trusted callers.
        message: String,
    },
    /// The supplied credential cannot be represented by the transport.
    #[error("invalid model credential: {message}")]
    InvalidCredential {
        /// Transport validation detail, excluding the credential itself.
        message: String,
    },
    /// HTTP client or protocol dispatch could not be configured.
    #[error("model invocation configuration error: {message}")]
    Configuration {
        /// Configuration diagnostic.
        message: String,
    },
    /// Model I/O failed without a provider response.
    #[error("model transport error: {message}")]
    Transport {
        /// Transport diagnostic.
        message: String,
    },
    /// Provider bytes could not be decoded as JSON or SSE.
    #[error("model response decode error: {message}")]
    Decode {
        /// Decode diagnostic.
        message: String,
    },
    /// The provider returned a non-success HTTP response.
    #[error("model HTTP error ({status}): {body}")]
    HttpResponse {
        /// Actual provider HTTP status.
        status: u16,
        /// Credential-filtered provider body; public presentation is caller policy.
        body: String,
        /// Parsed provider Retry-After delay in seconds.
        retry_after: Option<u64>,
    },
    /// Credential commit failed or the selected account changed during refresh.
    #[error("{failure}")]
    CredentialStorage {
        /// Bounded store failure facts with no backend-controlled secret detail.
        failure: crate::auth::store::StoreError,
    },
    /// Model I/O exceeded its selected timeout.
    #[error("model invocation timed out")]
    Timeout,
    /// The caller cancelled model I/O.
    #[error("model invocation cancelled")]
    Cancelled,
}

impl ModelError {
    /// Construct an authentication/transport configuration failure.
    pub fn configuration(message: impl Into<String>) -> Self {
        Self::Configuration {
            message: message.into(),
        }
    }

    /// Construct a model request validation failure.
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::InvalidRequest {
            message: message.into(),
        }
    }

    /// Construct a credential representation failure.
    pub fn invalid_credential(message: impl Into<String>) -> Self {
        Self::InvalidCredential {
            message: message.into(),
        }
    }
}
