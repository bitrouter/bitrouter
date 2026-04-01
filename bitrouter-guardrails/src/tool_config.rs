use std::collections::HashMap;

use bitrouter_core::routers::admin::{ParamRestrictions, ToolFilter};
use serde::{Deserialize, Serialize};

/// Per-provider tool policy configuration.
///
/// Combines visibility filtering (which tools are discoverable) with
/// parameter restrictions (which arguments are allowed at call time).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolProviderPolicy {
    /// Visibility filter controlling which tools appear in discovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<ToolFilter>,

    /// Parameter restriction rules applied at tool call time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param_restrictions: Option<ParamRestrictions>,
}

/// Tool guardrail configuration, embedded under `guardrails.tools`.
///
/// ```yaml
/// guardrails:
///   tools:
///     enabled: true
///     providers:
///       github:
///         filter:
///           deny: [delete_repo]
///         param_restrictions:
///           rules:
///             search:
///               deny: [force]
///               action: reject
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolGuardrailConfig {
    /// Master switch. When `false` the tool guardrail is a no-op.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Per-provider policy. Keys are provider/server names.
    #[serde(default)]
    pub providers: HashMap<String, ToolProviderPolicy>,
}

fn default_true() -> bool {
    true
}

impl Default for ToolGuardrailConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            providers: HashMap::new(),
        }
    }
}

impl ToolGuardrailConfig {
    /// Extract all filters as a flat map (for `GuardedToolRegistry` construction).
    pub fn filters_map(&self) -> HashMap<String, ToolFilter> {
        self.providers
            .iter()
            .filter_map(|(name, policy)| policy.filter.as_ref().map(|f| (name.clone(), f.clone())))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_enabled_with_no_providers() {
        let config = ToolGuardrailConfig::default();
        assert!(config.enabled);
        assert!(config.providers.is_empty());
    }

    #[test]
    fn config_round_trips_through_yaml() {
        let yaml = r#"
enabled: true
providers:
  github:
    filter:
      deny:
        - delete_repo
"#;
        let config: ToolGuardrailConfig = serde_saphyr::from_str(yaml).unwrap();
        assert!(config.enabled);
        assert!(config.providers.contains_key("github"));
        let github = &config.providers["github"];
        assert!(github.filter.is_some());
        assert!(
            !github
                .filter
                .as_ref()
                .map_or(true, |f| f.accepts("delete_repo"))
        );

        let serialized = serde_saphyr::to_string(&config).unwrap();
        let reparsed: ToolGuardrailConfig = serde_saphyr::from_str(&serialized).unwrap();
        assert!(reparsed.providers.contains_key("github"));
    }

    #[test]
    fn empty_yaml_deserializes_to_defaults() {
        let config: ToolGuardrailConfig = serde_saphyr::from_str("{}").unwrap();
        assert!(config.enabled);
        assert!(config.providers.is_empty());
    }

    #[test]
    fn filters_map_extracts_only_present_filters() {
        let yaml = r#"
providers:
  github:
    filter:
      deny: [delete_repo]
  jira: {}
"#;
        let config: ToolGuardrailConfig = serde_saphyr::from_str(yaml).unwrap();
        let filters = config.filters_map();
        assert_eq!(filters.len(), 1);
        assert!(filters.contains_key("github"));
    }
}
