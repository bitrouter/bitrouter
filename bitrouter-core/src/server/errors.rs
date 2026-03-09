use std::time::Duration;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, ServerError>;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ServerError {
    #[error("invalid input: {message}")]
    InvalidInput { message: String },
    #[error("unauthorized: {message}")]
    Unauthorized { message: String },
    #[error("forbidden: {message}")]
    Forbidden { message: String },
    #[error("not found: {resource}")]
    NotFound { resource: String },
    #[error("conflict: {message}")]
    Conflict { message: String },
    #[error("rate limited: {message}")]
    RateLimited {
        message: String,
        retry_after: Option<Duration>,
    },
    #[error("service unavailable: {message}")]
    Unavailable { message: String },
    #[error("internal error: {message}")]
    Internal { message: String },
}

impl ServerError {
    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::InvalidInput {
            message: message.into(),
        }
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::Unauthorized {
            message: message.into(),
        }
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::Forbidden {
            message: message.into(),
        }
    }

    pub fn not_found(resource: impl Into<String>) -> Self {
        Self::NotFound {
            resource: resource.into(),
        }
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict {
            message: message.into(),
        }
    }

    pub fn rate_limited(message: impl Into<String>, retry_after: Option<Duration>) -> Self {
        Self::RateLimited {
            message: message.into(),
            retry_after,
        }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable {
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
        }
    }
}
