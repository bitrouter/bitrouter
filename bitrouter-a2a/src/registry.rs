//! Agent card registry trait.
//!
//! Defines the storage and lookup interface for agent registrations.
//! A registration binds an A2A Agent Card to a BitRouter JWT identity.

use serde::{Deserialize, Serialize};

use crate::card::AgentCard;
use crate::error::A2aError;

/// An Agent Card bound to a BitRouter JWT identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRegistration {
    /// The A2A Agent Card.
    pub card: AgentCard,

    /// CAIP-10 address from `BitrouterClaims.iss`, if bound to a JWT identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iss: Option<String>,
}

/// Storage and lookup for agent registrations.
///
/// Implementations manage the persistence and retrieval of agent cards
/// bound to BitRouter JWT identities.
pub trait AgentCardRegistry: Send + Sync {
    /// Register a new agent card. Returns `AlreadyExists` if the name is taken.
    fn register(&self, registration: AgentRegistration) -> Result<(), A2aError>;

    /// Remove an agent card by name. Returns `NotFound` if absent.
    fn remove(&self, name: &str) -> Result<(), A2aError>;

    /// Get a specific agent registration by name.
    fn get(&self, name: &str) -> Result<Option<AgentRegistration>, A2aError>;

    /// List all registered agents.
    fn list(&self) -> Result<Vec<AgentRegistration>, A2aError>;

    /// Reverse lookup: find agent name from a JWT `iss` claim.
    fn resolve_by_iss(&self, iss: &str) -> Result<Option<String>, A2aError>;
}

/// Validate that an agent name follows DNS label rules: lowercase alphanumeric
/// and hyphens, starting with a letter or digit, max 63 characters.
pub fn validate_name(name: &str) -> Result<(), A2aError> {
    if name.is_empty() {
        return Err(A2aError::InvalidName {
            name: name.to_string(),
            reason: "name cannot be empty".to_string(),
        });
    }
    if name.len() > 63 {
        return Err(A2aError::InvalidName {
            name: name.to_string(),
            reason: "name cannot exceed 63 characters".to_string(),
        });
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(A2aError::InvalidName {
            name: name.to_string(),
            reason: "name must contain only lowercase letters, digits, and hyphens".to_string(),
        });
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err(A2aError::InvalidName {
            name: name.to_string(),
            reason: "name cannot start or end with a hyphen".to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_names() {
        assert!(validate_name("claude-code").is_ok());
        assert!(validate_name("agent1").is_ok());
        assert!(validate_name("a").is_ok());
        assert!(validate_name("my-agent-v2").is_ok());
    }

    #[test]
    fn invalid_names() {
        assert!(validate_name("").is_err());
        assert!(validate_name("Claude-Code").is_err()); // uppercase
        assert!(validate_name("-leading").is_err());
        assert!(validate_name("trailing-").is_err());
        assert!(validate_name("has space").is_err());
        assert!(validate_name("has_underscore").is_err());
        let long = "a".repeat(64);
        assert!(validate_name(&long).is_err());
    }
}
