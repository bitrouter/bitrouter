//! A2A agent connection configuration.
//!
//! Provider-specific config type that describes how to connect to an upstream
//! A2A agent. This is the A2A equivalent of `OpenAiConfig` / `AnthropicConfig`
//! for model providers.

use std::collections::HashMap;

/// Configuration for connecting to a single upstream A2A agent.
#[derive(Debug, Clone)]
pub struct A2aAgentConfig {
    /// Display name for this upstream agent.
    pub name: String,
    /// Base URL of the upstream agent (used for card discovery).
    pub url: String,
    /// HTTP headers to send to upstream (e.g., auth tokens).
    pub headers: HashMap<String, String>,
}

impl A2aAgentConfig {
    /// Validate the configuration.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_empty() {
            return Err("agent name cannot be empty".to_string());
        }
        if self.name.contains('/') {
            return Err(format!("agent name '{}' cannot contain '/'", self.name));
        }
        if self.url.is_empty() {
            return Err("agent URL cannot be empty".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_config() {
        let config = A2aAgentConfig {
            name: "my-agent".into(),
            url: "https://agent.example.com".into(),
            headers: HashMap::new(),
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn empty_name_rejected() {
        let config = A2aAgentConfig {
            name: String::new(),
            url: "https://agent.example.com".into(),
            headers: HashMap::new(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn empty_url_rejected() {
        let config = A2aAgentConfig {
            name: "my-agent".into(),
            url: String::new(),
            headers: HashMap::new(),
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn slash_in_name_rejected() {
        let config = A2aAgentConfig {
            name: "a/b".into(),
            url: "https://agent.example.com".into(),
            headers: HashMap::new(),
        };
        assert!(config.validate().is_err());
    }
}
