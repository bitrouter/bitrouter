//! Serializable guardrail configuration — the data contract the plugin runs
//! off, independent of where it's loaded from. A host deserialises a
//! [`GuardrailConfig`] from its own source (a config file, a control-plane
//! database, …) and [`compile`](GuardrailConfig::compile)s it into the
//! runtime [`RuleSet`]. The plugin never touches a config *file*; it depends
//! only on this data.

use serde::{Deserialize, Serialize};

use crate::rules::{Action, GuardrailRule, RuleSet};

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
