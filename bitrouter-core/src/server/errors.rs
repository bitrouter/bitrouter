use thiserror::Error;

#[derive(Debug, Clone, Error)]
pub enum ServerError {
    #[error("not found: {entity} {id}")]
    NotFound { entity: String, id: String },

    #[error("already exists: {entity} {id}")]
    AlreadyExists { entity: String, id: String },

    #[error("unauthorized: {message}")]
    Unauthorized { message: String },

    #[error("forbidden: {message}")]
    Forbidden { message: String },

    #[error("rate limited: {message}")]
    RateLimited { message: String },

    #[error("spend limit exceeded: {message}")]
    SpendLimitExceeded { message: String },

    #[error("invalid input: {message}")]
    InvalidInput { message: String },

    #[error("internal: {message}")]
    Internal { message: String },
}

pub type ServerResult<T> = std::result::Result<T, ServerError>;

impl ServerError {
    pub fn not_found(entity: impl Into<String>, id: impl Into<String>) -> Self {
        Self::NotFound {
            entity: entity.into(),
            id: id.into(),
        }
    }

    pub fn already_exists(entity: impl Into<String>, id: impl Into<String>) -> Self {
        Self::AlreadyExists {
            entity: entity.into(),
            id: id.into(),
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

    pub fn rate_limited(message: impl Into<String>) -> Self {
        Self::RateLimited {
            message: message.into(),
        }
    }

    pub fn spend_limit_exceeded(message: impl Into<String>) -> Self {
        Self::SpendLimitExceeded {
            message: message.into(),
        }
    }

    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::InvalidInput {
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal {
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display() {
        let err = ServerError::not_found("account", "acc_123");
        assert_eq!(err.to_string(), "not found: account acc_123");
    }

    #[test]
    fn error_is_result_compatible() {
        fn example() -> ServerResult<()> {
            Err(ServerError::unauthorized("bad token"))
        }
        assert!(example().is_err());
    }
}
