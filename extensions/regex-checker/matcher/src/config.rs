//! Serializable guardrail configuration — the data contract the plugin runs
//! off, independent of where it's loaded from. A host deserialises a
//! [`GuardrailConfig`] from its own source (a config file, a control-plane
//! database, …) and [`compile`](GuardrailConfig::compile)s it into the
//! runtime [`RuleSet`]. The plugin never touches a config *file*; it depends
//! only on this data.

use serde::{Deserialize, Serialize};

use crate::rules::{Action, GuardrailRule, RuleSet};

/// Input surface supported by the compiled request-check extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputScope {
    /// Text fragments from the effective entry request.
    Input,
}

/// Action supported by the compiled request-check extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputAction {
    /// Deny a request when the expression matches.
    Block,
}

/// One strict rule for the compiled request-check extension.
///
/// `action` is required even though only `block` is supported. This makes an
/// attempted migration of a legacy `redact` rule fail during startup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputRuleSpec {
    /// Operator-facing rule name. It is not included in request-check decisions.
    pub name: String,
    /// Rust `regex` expression matched against the newline-flattened input.
    pub pattern: String,
    /// Required input action. The request-check callback supports only `block`.
    pub action: InputAction,
}

/// Strict startup configuration for the compiled request-check extension.
///
/// Both `scope` and each rule's `action` are required. Unknown fields, other
/// scopes, and other actions are deserialization errors rather than ignored
/// configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputGuardrailConfig {
    /// Required checker scope. Only `input` is supported.
    pub scope: InputScope,
    /// Fixed rules compiled once at host startup.
    pub rules: Vec<InputRuleSpec>,
}

/// Sanitized input-rule compilation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputConfigError {
    /// A rules document with no rules would silently allow every request.
    EmptyRules,
    /// The pattern at this zero-based position is not a valid bounded regex.
    InvalidRegex { rule_index: usize },
}

impl std::fmt::Display for InputConfigError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyRules => {
                formatter.write_str("rules document must contain at least one rule")
            }
            Self::InvalidRegex { rule_index } => {
                write!(formatter, "rule {rule_index} contains an invalid regex")
            }
        }
    }
}

impl std::error::Error for InputConfigError {}

impl InputGuardrailConfig {
    /// Compile every rule without exposing its name or expression in errors.
    pub fn compile(&self) -> Result<RuleSet, InputConfigError> {
        if self.rules.is_empty() {
            return Err(InputConfigError::EmptyRules);
        }
        let mut set = RuleSet::new();
        for (rule_index, spec) in self.rules.iter().enumerate() {
            let rule = GuardrailRule::new(&spec.name, &spec.pattern, Action::Block)
                .map_err(|_| InputConfigError::InvalidRegex { rule_index })?;
            set.push(rule);
        }
        Ok(set)
    }
}

/// One guardrail rule in serializable form: a name, a regex pattern, and a
/// match action. Compile it into a runtime rule with `RuleSpec::compile`.
//
// NOTE: the doc comments on this type and its fields are copied verbatim by
// `schemars` into the JSON Schema `description`, which a host (e.g.
// bitrouter-cloud) republishes in its OpenAPI document. Keep them as plain
// prose — no rustdoc intra-doc links (`[`Foo`]`), which would leak the
// bracket syntax into the published spec. (`PartialEq`/`Eq`/`JsonSchema` are
// derived precisely so a host can embed this in its own comparable,
// OpenAPI-published policy types.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RuleSpec {
    /// Human-readable rule name (surfaced in deny reasons / logs).
    pub name: String,
    /// The regex pattern to match.
    pub pattern: String,
    /// What to do on a match. Defaults to `block` when omitted.
    #[serde(default)]
    pub action: Action,
}

impl RuleSpec {
    /// Compile this spec's regex into a runtime [`GuardrailRule`].
    pub fn compile(&self) -> Result<GuardrailRule, regex::Error> {
        GuardrailRule::new(&self.name, &self.pattern, self.action)
    }
}

/// The guardrail data contract. In a config file this is the `custom_patterns`
/// array under `plugins.bitrouter-guardrails`; a control plane builds the same
/// shape from its own store.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct GuardrailConfig {
    /// The configured rules.
    #[serde(default, rename = "custom_patterns")]
    pub rules: Vec<RuleSpec>,
}

impl GuardrailConfig {
    /// Compile every [`RuleSpec`] into a [`RuleSet`], surfacing the first regex
    /// that fails to compile.
    pub fn compile(&self) -> Result<RuleSet, regex::Error> {
        let mut set = RuleSet::new();
        for spec in &self.rules {
            set.push(spec.compile()?);
        }
        Ok(set)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_input(yaml: &str) -> Result<InputGuardrailConfig, serde_json::Error> {
        serde_json::from_str(yaml)
    }

    #[test]
    fn strict_input_config_compiles_block_rules() -> Result<(), Box<dyn std::error::Error>> {
        let config = parse_input(
            r#"{
                "scope": "input",
                "rules": [{"name": "secret", "pattern": "token-[0-9]+", "action": "block"}]
            }"#,
        )?;
        assert_eq!(config.scope, InputScope::Input);
        assert_eq!(config.compile()?.first_block("token-42"), Some("secret"));
        Ok(())
    }

    #[test]
    fn strict_input_config_rejects_redact_scope_and_unknown_fields() {
        let redact = r#"{
            "scope": "input",
            "rules": [{"name": "secret", "pattern": "x", "action": "redact"}]
        }"#;
        let output = r#"{
            "scope": "output",
            "rules": [{"name": "secret", "pattern": "x", "action": "block"}]
        }"#;
        let unknown = r#"{
            "scope": "input",
            "rules": [{"name": "secret", "pattern": "x", "action": "block", "enabled": true}]
        }"#;
        assert!(parse_input(redact).is_err());
        assert!(parse_input(output).is_err());
        assert!(parse_input(unknown).is_err());
    }

    #[test]
    fn strict_input_config_requires_scope_action_and_nonempty_rules()
    -> Result<(), Box<dyn std::error::Error>> {
        let missing_scope = r#"{
            "rules": [{"name": "secret", "pattern": "x", "action": "block"}]
        }"#;
        let missing_action = r#"{
            "scope": "input",
            "rules": [{"name": "secret", "pattern": "x"}]
        }"#;
        assert!(parse_input(missing_scope).is_err());
        assert!(parse_input(missing_action).is_err());
        assert!(matches!(
            parse_input(r#"{"scope":"input","rules":[]}"#)?.compile(),
            Err(InputConfigError::EmptyRules)
        ));
        Ok(())
    }

    #[test]
    fn strict_input_config_hides_invalid_expression() -> Result<(), Box<dyn std::error::Error>> {
        let config = parse_input(
            r#"{
                "scope": "input",
                "rules": [{"name": "private-name", "pattern": "private-secret-(", "action": "block"}]
            }"#,
        )?;
        let error = config
            .compile()
            .err()
            .ok_or_else(|| std::io::Error::other("invalid regex unexpectedly compiled"))?
            .to_string();
        assert!(!error.contains("private-name"));
        assert!(!error.contains("private-secret"));
        Ok(())
    }

    #[test]
    fn deserialises_custom_patterns_with_default_action() {
        let json = serde_json::json!({
            "custom_patterns": [
                { "name": "ssn", "pattern": r"\d{3}-\d{2}-\d{4}", "action": "redact" },
                { "name": "secret", "pattern": "sk-[a-z0-9]+" }
            ]
        });
        let cfg: GuardrailConfig = serde_json::from_value(json).unwrap();
        assert_eq!(cfg.rules.len(), 2);
        assert_eq!(cfg.rules[0].action, Action::Redact);
        // Omitted action falls back to Block.
        assert_eq!(cfg.rules[1].action, Action::Block);
        // And every pattern compiles into a runtime rule set.
        assert!(!cfg.compile().unwrap().is_empty());
    }

    #[test]
    fn empty_config_compiles_to_empty_rule_set() {
        let cfg = GuardrailConfig::default();
        assert!(cfg.compile().unwrap().is_empty());
    }

    #[test]
    fn unknown_action_is_a_deserialisation_error() {
        let json = serde_json::json!({
            "custom_patterns": [{ "name": "x", "pattern": "y", "action": "nope" }]
        });
        assert!(serde_json::from_value::<GuardrailConfig>(json).is_err());
    }

    #[test]
    fn invalid_regex_surfaces_at_compile() {
        let cfg = GuardrailConfig {
            rules: vec![RuleSpec {
                name: "bad".to_string(),
                pattern: "(".to_string(),
                action: Action::Block,
            }],
        };
        assert!(cfg.compile().is_err());
    }
}
