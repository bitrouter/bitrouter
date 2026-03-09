use thiserror::Error;

pub type Result<T> = std::result::Result<T, ServerError>;

#[derive(Debug, Clone, Error)]
pub enum ServerError {
    #[error("not found: {resource}")]
    NotFound { resource: String },
    #[error("invalid input: {message}")]
    InvalidInput { message: String },
    #[error("unauthorized: {message}")]
    Unauthorized { message: String },
    #[error("forbidden: {message}")]
    Forbidden { message: String },
    #[error("conflict: {message}")]
    Conflict { message: String },
    #[error("internal error: {message}")]
    Internal { message: String },
}
