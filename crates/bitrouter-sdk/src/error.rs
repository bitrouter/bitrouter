//! Crate-wide error type. Shared across all protocol modules.

use std::fmt;

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, BitrouterError>;

/// The unified BitRouter error type.
///
/// Variants carry an HTTP status so handlers can render OpenAI/Anthropic-style
/// error envelopes without a separate mapping table. `Clone` because the
/// pipeline hands the same error to several consumers (observe hooks,
/// settlement recorders, the caller).
#[derive(Debug, Clone, thiserror::Error)]
pub enum BitrouterError {
    /// 400 — malformed request (bad JSON, unknown enum variant, …).
    #[error("bad request: {message}")]
    BadRequest {
        /// Human-readable detail.
        message: String,
    },

    /// 401 — missing or invalid credentials.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// 402 — payment required (MPP challenge / insufficient balance).
    #[error("payment required: {0}")]
    PaymentRequired(String),

    /// 403 — policy violation.
    #[error("forbidden: {0}")]
    Forbidden(String),

    /// 404 — no route found for the requested model.
    #[error("not found: {0}")]
    NotFound(String),

    /// 429 — rate limited.
    #[error("rate limited")]
    RateLimited {
        /// Seconds the caller should wait before retrying.
        retry_after: Option<u64>,
    },

    /// 502 — upstream provider returned an error.
    #[error("upstream error ({status}): {message}")]
    Upstream {
        /// Upstream HTTP status.
        status: u16,
        /// Upstream error body / detail.
        message: String,
    },

    /// 504 — upstream timed out.
    #[error("upstream timeout")]
    UpstreamTimeout,

    /// 500 — internal error.
    #[error("internal error: {0}")]
    Internal(String),
}

impl BitrouterError {
    /// The HTTP status code this error renders as.
    pub fn status(&self) -> u16 {
        match self {
            Self::BadRequest { .. } => 400,
            Self::Unauthorized(_) => 401,
            Self::PaymentRequired(_) => 402,
            Self::Forbidden(_) => 403,
            Self::NotFound(_) => 404,
            Self::RateLimited { .. } => 429,
            Self::Upstream { .. } => 502,
            Self::UpstreamTimeout => 504,
            Self::Internal(_) => 500,
        }
    }

    /// The OpenAI-compatible `error.type` tag.
    pub fn error_type(&self) -> &'static str {
        match self {
            Self::BadRequest { .. } => "invalid_request_error",
            Self::Unauthorized(_) => "authentication_error",
            Self::PaymentRequired(_) => "payment_required",
            Self::Forbidden(_) => "permission_error",
            Self::NotFound(_) => "not_found_error",
            Self::RateLimited { .. } => "rate_limit_error",
            Self::Upstream { .. } | Self::UpstreamTimeout => "upstream_error",
            Self::Internal(_) => "internal_error",
        }
    }

    /// Convenience constructor for [`BitrouterError::BadRequest`].
    pub fn bad_request(message: impl fmt::Display) -> Self {
        Self::BadRequest {
            message: message.to_string(),
        }
    }

    /// Convenience constructor for [`BitrouterError::Internal`].
    pub fn internal(message: impl fmt::Display) -> Self {
        Self::Internal(message.to_string())
    }
}
