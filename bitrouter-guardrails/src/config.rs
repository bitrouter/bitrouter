use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::pattern::PatternId;
use crate::rule::Action;

/// Top-level guardrail configuration, embedded in the bitrouter config under
/// the `guardrails` key.
///
/// ```yaml
/// guardrails:
///   enabled: true
///   upgoing:
///     api_keys: redact
///     private_keys: block
///   downgoing:
///     suspicious_commands: block
/// ```
///
/// Any pattern not explicitly listed uses the default action for its
/// direction (`Warn` for both upgoing and downgoing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardrailConfig {
    /// Master switch. When `false` the guardrail engine is a no-op.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Per-pattern action overrides for **outbound** traffic (user → LLM).
    #[serde(default)]
    pub upgoing: HashMap<PatternId, Action>,

    /// Per-pattern action overrides for **inbound** traffic (LLM → user).
    #[serde(default)]
    pub downgoing: HashMap<PatternId, Action>,
}

fn default_enabled() -> bool {
    true
}

impl Default for GuardrailConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            upgoing: HashMap::new(),
            downgoing: HashMap::new(),
        }
    }
}

impl GuardrailConfig {
    /// Resolve the effective action for an upgoing pattern.
    ///
    /// Returns the user-configured action if present, otherwise `Warn`.
    pub fn upgoing_action(&self, id: PatternId) -> Action {
        self.upgoing.get(&id).copied().unwrap_or(Action::Warn)
    }

    /// Resolve the effective action for a downgoing pattern.
    ///
    /// Returns the user-configured action if present, otherwise `Warn`.
    pub fn downgoing_action(&self, id: PatternId) -> Action {
        self.downgoing.get(&id).copied().unwrap_or(Action::Warn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_enabled_with_warn() {
        let config = GuardrailConfig::default();
        assert!(config.enabled);
        assert_eq!(config.upgoing_action(PatternId::ApiKeys), Action::Warn);
        assert_eq!(
            config.downgoing_action(PatternId::SuspiciousCommands),
            Action::Warn
        );
    }

    #[test]
    fn override_action_takes_precedence() {
        let mut config = GuardrailConfig::default();
        config.upgoing.insert(PatternId::ApiKeys, Action::Redact);
        config.upgoing.insert(PatternId::PrivateKeys, Action::Block);
        assert_eq!(config.upgoing_action(PatternId::ApiKeys), Action::Redact);
        assert_eq!(config.upgoing_action(PatternId::PrivateKeys), Action::Block);
        // Unset patterns still default to Warn
        assert_eq!(config.upgoing_action(PatternId::Credentials), Action::Warn);
    }

    #[test]
    fn config_round_trips_through_yaml() {
        let yaml = r#"
enabled: true
upgoing:
  api_keys: redact
  private_keys: block
downgoing:
  suspicious_commands: block
"#;
        let config: GuardrailConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.enabled);
        assert_eq!(config.upgoing_action(PatternId::ApiKeys), Action::Redact);
        assert_eq!(config.upgoing_action(PatternId::PrivateKeys), Action::Block);
        assert_eq!(
            config.downgoing_action(PatternId::SuspiciousCommands),
            Action::Block
        );

        // Round-trip through serialization
        let serialized = serde_yaml::to_string(&config).unwrap();
        let reparsed: GuardrailConfig = serde_yaml::from_str(&serialized).unwrap();
        assert_eq!(reparsed.upgoing_action(PatternId::ApiKeys), Action::Redact);
    }

    #[test]
    fn empty_yaml_deserializes_to_defaults() {
        let config: GuardrailConfig = serde_yaml::from_str("{}").unwrap();
        assert!(config.enabled);
        assert!(config.upgoing.is_empty());
        assert!(config.downgoing.is_empty());
    }
}
