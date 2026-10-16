//! Errors from intrinsic public-contract validation.

/// An invalid capability declaration, without transport or provider state.
#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid model request: {message}")]
pub struct ValidationError {
    /// The declaration detail that must be corrected.
    pub message: String,
}

impl ValidationError {
    /// Describe an invalid public capability declaration.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}
