//! Configuration types for A2A gateway upstreams.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::A2aGatewayError;

/// Configuration for an upstream A2A agent to proxy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct A2aAgentConfig {
    /// Display name for this upstream agent.
    pub name: String,

    /// Base URL of the upstream A2A agent (used for discovery).
    pub url: String,

    /// Optional HTTP headers to send to upstream (e.g., auth tokens).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,

    /// Optional card discovery path override.
    /// Defaults to `/.well-known/agent-card.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card_path: Option<String>,
}

impl A2aAgentConfig {
    /// Validate the configuration.
    pub fn validate(&self) -> Result<(), A2aGatewayError> {
        if self.name.is_empty() {
            return Err(A2aGatewayError::InvalidConfig {
                reason: "agent name cannot be empty".to_string(),
            });
        }
        if self.name.contains('/') {
            return Err(A2aGatewayError::InvalidConfig {
                reason: format!("agent name '{}' cannot contain '/'", self.name),
            });
        }
        if self.url.is_empty() {
            return Err(A2aGatewayError::InvalidConfig {
                reason: "agent URL cannot be empty".to_string(),
            });
        }
        Ok(())
    }

    /// Get the discovery URL for this agent.
    pub fn discovery_url(&self) -> String {
        let base = self.url.trim_end_matches('/');
        let path = self
            .card_path
            .as_deref()
            .unwrap_or("/.well-known/agent-card.json");
        format!("{base}{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_empty_name() {
        let config = A2aAgentConfig {
            name: String::new(),
            url: "http://localhost".to_string(),
            headers: HashMap::new(),
            card_path: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_slash_in_name() {
        let config = A2aAgentConfig {
            name: "my/agent".to_string(),
            url: "http://localhost".to_string(),
            headers: HashMap::new(),
            card_path: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_url() {
        let config = A2aAgentConfig {
            name: "agent".to_string(),
            url: String::new(),
            headers: HashMap::new(),
            card_path: None,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_accepts_valid_config() {
        let config = A2aAgentConfig {
            name: "my-agent".to_string(),
            url: "http://localhost:8080".to_string(),
            headers: HashMap::new(),
            card_path: None,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn discovery_url_default_path() {
        let config = A2aAgentConfig {
            name: "agent".to_string(),
            url: "https://agent.example.com".to_string(),
            headers: HashMap::new(),
            card_path: None,
        };
        assert_eq!(
            config.discovery_url(),
            "https://agent.example.com/.well-known/agent-card.json"
        );
    }

    #[test]
    fn discovery_url_custom_path() {
        let config = A2aAgentConfig {
            name: "agent".to_string(),
            url: "https://agent.example.com/".to_string(),
            headers: HashMap::new(),
            card_path: Some("/custom/card.json".to_string()),
        };
        assert_eq!(
            config.discovery_url(),
            "https://agent.example.com/custom/card.json"
        );
    }

    #[test]
    fn config_round_trip_yaml() {
        let yaml = r#"
name: "my-agent"
url: "https://agent.example.com"
headers:
  Authorization: "Bearer token123"
"#;
        let config: A2aAgentConfig = serde_yaml::from_str(yaml).expect("parse");
        assert_eq!(config.name, "my-agent");
        assert_eq!(
            config.headers.get("Authorization").map(String::as_str),
            Some("Bearer token123")
        );
    }
}
