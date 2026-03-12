use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::pattern::PatternId;
use crate::rule::Action;

/// The repository URL included in block messages when `include_help_link` is
/// enabled.
pub const REPO_URL: &str = "https://github.com/bitrouter/bitrouter";

/// Top-level guardrail configuration, embedded in the bitrouter config under
/// the `guardrails` key.
///
/// ```yaml
/// guardrails:
///   enabled: true
///   disabled_patterns:
///     - ip_addresses
///     - pii_phone_numbers
///   custom_patterns:
///     - name: my_token
///       regex: "myapp_[A-Za-z0-9]{32}"
///       direction: upgoing
///   upgoing:
///     api_keys: redact
///     private_keys: block
///   downgoing:
///     suspicious_commands: block
///   block_message:
///     include_details: true
///     include_help_link: true
/// ```
///
/// Any pattern not explicitly listed uses the default action for its
/// direction (`Warn` for both upgoing and downgoing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardrailConfig {
    /// Master switch. When `false` the guardrail engine is a no-op.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Built-in patterns to disable. Any [`PatternId`] listed here will be
    /// skipped during inspection, regardless of the action configured for it.
    #[serde(default)]
    pub disabled_patterns: Vec<PatternId>,

    /// User-defined custom patterns appended to the built-in set.
    #[serde(default)]
    pub custom_patterns: Vec<CustomPatternDef>,

    /// Per-pattern action overrides for **outbound** traffic (user → LLM).
    #[serde(default)]
    pub upgoing: HashMap<PatternId, Action>,

    /// Per-pattern action overrides for **inbound** traffic (LLM → user).
    #[serde(default)]
    pub downgoing: HashMap<PatternId, Action>,

    /// Per-custom-pattern action overrides for **outbound** traffic.
    #[serde(default)]
    pub custom_upgoing: HashMap<String, Action>,

    /// Per-custom-pattern action overrides for **inbound** traffic.
    #[serde(default)]
    pub custom_downgoing: HashMap<String, Action>,

    /// Controls the content of error messages produced when content is blocked.
    #[serde(default)]
    pub block_message: BlockMessageConfig,
}

/// Configuration for a user-defined custom pattern.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomPatternDef {
    /// Unique name for this custom pattern (used to reference it in
    /// `custom_upgoing` / `custom_downgoing` action maps).
    pub name: String,

    /// The regex pattern string.
    pub regex: String,

    /// Whether the pattern applies to outbound (`upgoing`), inbound
    /// (`downgoing`), or `both` directions.
    #[serde(default)]
    pub direction: PatternDirection,
}

/// Direction for a custom pattern.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatternDirection {
    /// Apply only to outbound (user → LLM) traffic.
    #[default]
    Upgoing,
    /// Apply only to inbound (LLM → user) traffic.
    Downgoing,
    /// Apply in both directions.
    Both,
}

/// Controls what information is included in block error messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockMessageConfig {
    /// Include a human-readable description of why the content was blocked.
    #[serde(default = "default_true")]
    pub include_details: bool,

    /// Include a link to the bitrouter repository for further information.
    #[serde(default = "default_true")]
    pub include_help_link: bool,
}

impl Default for BlockMessageConfig {
    fn default() -> Self {
        Self {
            include_details: true,
            include_help_link: true,
        }
    }
}

fn default_enabled() -> bool {
    true
}

fn default_true() -> bool {
    true
}

impl Default for GuardrailConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            disabled_patterns: Vec::new(),
            custom_patterns: Vec::new(),
            upgoing: HashMap::new(),
            downgoing: HashMap::new(),
            custom_upgoing: HashMap::new(),
            custom_downgoing: HashMap::new(),
            block_message: BlockMessageConfig::default(),
        }
    }
}

impl GuardrailConfig {
    /// Returns `true` if the given built-in pattern has been disabled by the
    /// user.
    pub fn is_pattern_disabled(&self, id: PatternId) -> bool {
        self.disabled_patterns.contains(&id)
    }

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

    /// Resolve the effective action for a custom upgoing pattern.
    pub fn custom_upgoing_action(&self, name: &str) -> Action {
        self.custom_upgoing
            .get(name)
            .copied()
            .unwrap_or(Action::Warn)
    }

    /// Resolve the effective action for a custom downgoing pattern.
    pub fn custom_downgoing_action(&self, name: &str) -> Action {
        self.custom_downgoing
            .get(name)
            .copied()
            .unwrap_or(Action::Warn)
    }

    /// Format a block error message, respecting `block_message` config.
    pub fn format_block_message(&self, direction: &str, description: &str) -> String {
        let mut msg = format!("guardrail blocked {direction} content");

        if self.block_message.include_details {
            msg.push_str(&format!(": {description}"));
        }

        if self.block_message.include_help_link {
            msg.push_str(&format!(". For more information, see {REPO_URL}"));
        }

        msg
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
        assert!(config.disabled_patterns.is_empty());
        assert!(config.custom_patterns.is_empty());
        assert!(config.block_message.include_details);
        assert!(config.block_message.include_help_link);
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
        assert!(config.disabled_patterns.is_empty());
        assert!(config.custom_patterns.is_empty());
    }

    #[test]
    fn disabled_patterns_from_yaml() {
        let yaml = r#"
disabled_patterns:
  - ip_addresses
  - pii_phone_numbers
"#;
        let config: GuardrailConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.is_pattern_disabled(PatternId::IpAddresses));
        assert!(config.is_pattern_disabled(PatternId::PiiPhoneNumbers));
        assert!(!config.is_pattern_disabled(PatternId::ApiKeys));
    }

    #[test]
    fn custom_patterns_from_yaml() {
        let yaml = r#"
custom_patterns:
  - name: my_token
    regex: "myapp_[A-Za-z0-9]{32}"
    direction: upgoing
  - name: bad_url
    regex: "https://evil\\.com"
    direction: downgoing
  - name: both_dirs
    regex: "secret_value"
    direction: both
"#;
        let config: GuardrailConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.custom_patterns.len(), 3);
        assert_eq!(config.custom_patterns[0].name, "my_token");
        assert_eq!(
            config.custom_patterns[0].direction,
            PatternDirection::Upgoing
        );
        assert_eq!(
            config.custom_patterns[1].direction,
            PatternDirection::Downgoing
        );
        assert_eq!(config.custom_patterns[2].direction, PatternDirection::Both);
    }

    #[test]
    fn custom_pattern_action_overrides() {
        let yaml = r#"
custom_patterns:
  - name: my_token
    regex: "myapp_[A-Za-z0-9]{32}"
custom_upgoing:
  my_token: block
"#;
        let config: GuardrailConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.custom_upgoing_action("my_token"), Action::Block);
        assert_eq!(config.custom_upgoing_action("nonexistent"), Action::Warn);
    }

    #[test]
    fn block_message_config_from_yaml() {
        let yaml = r#"
block_message:
  include_details: false
  include_help_link: false
"#;
        let config: GuardrailConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(!config.block_message.include_details);
        assert!(!config.block_message.include_help_link);
    }

    #[test]
    fn format_block_message_full() {
        let config = GuardrailConfig::default();
        let msg = config.format_block_message("upgoing", "API keys detected");
        assert!(msg.contains("API keys detected"));
        assert!(msg.contains(REPO_URL));
    }

    #[test]
    fn format_block_message_no_details() {
        let mut config = GuardrailConfig::default();
        config.block_message.include_details = false;
        let msg = config.format_block_message("upgoing", "API keys detected");
        assert!(!msg.contains("API keys detected"));
        assert!(msg.contains(REPO_URL));
    }

    #[test]
    fn format_block_message_no_link() {
        let mut config = GuardrailConfig::default();
        config.block_message.include_help_link = false;
        let msg = config.format_block_message("upgoing", "API keys detected");
        assert!(msg.contains("API keys detected"));
        assert!(!msg.contains(REPO_URL));
    }

    #[test]
    fn format_block_message_bare() {
        let mut config = GuardrailConfig::default();
        config.block_message.include_details = false;
        config.block_message.include_help_link = false;
        let msg = config.format_block_message("upgoing", "API keys detected");
        assert_eq!(msg, "guardrail blocked upgoing content");
    }
}
