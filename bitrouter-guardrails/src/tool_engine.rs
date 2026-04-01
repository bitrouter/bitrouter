use bitrouter_core::routers::admin::{ParamRestrictions, ToolFilter};

use crate::tool_config::ToolGuardrailConfig;

/// The tool guardrail engine.
///
/// Parallel to [`Guardrail`](crate::engine::Guardrail) for models, but
/// evaluates access policy (visibility filters, parameter restrictions)
/// rather than content patterns.
///
/// Constructed from [`ToolGuardrailConfig`], immutable after construction.
/// Hot-reload is handled by rebuilding and swapping the `Arc<ToolGuardrail>`.
#[derive(Debug, Clone)]
pub struct ToolGuardrail {
    config: ToolGuardrailConfig,
}

impl ToolGuardrail {
    /// Create a new tool guardrail engine from the given configuration.
    pub fn new(config: ToolGuardrailConfig) -> Self {
        Self { config }
    }

    /// Returns `true` when the tool guardrail is disabled and will skip
    /// all policy checks.
    pub fn is_disabled(&self) -> bool {
        !self.config.enabled
    }

    /// Look up the visibility filter for a provider, if one is configured.
    pub fn filter_for(&self, provider: &str) -> Option<&ToolFilter> {
        self.config
            .providers
            .get(provider)
            .and_then(|p| p.filter.as_ref())
    }

    /// Look up the parameter restrictions for a provider, if configured.
    pub fn param_restrictions_for(&self, provider: &str) -> Option<&ParamRestrictions> {
        self.config
            .providers
            .get(provider)
            .and_then(|p| p.param_restrictions.as_ref())
    }

    /// Returns `true` if there are parameter restrictions configured for
    /// the given provider. Used by [`GuardedToolRouter`](crate::tool_router::GuardedToolRouter)
    /// to skip wrapping when no policy applies.
    pub fn has_restrictions_for(&self, provider: &str) -> bool {
        self.config
            .providers
            .get(provider)
            .and_then(|p| p.param_restrictions.as_ref())
            .is_some_and(|r| !r.rules.is_empty())
    }

    /// Validate and optionally mutate tool call arguments against the
    /// restrictions configured for the given provider and tool.
    ///
    /// Returns `Ok(())` if allowed (possibly with stripped params).
    /// Returns `Err` if a parameter is denied and action is `Reject`.
    pub fn check_params(
        &self,
        provider: &str,
        tool_name: &str,
        arguments: &mut serde_json::Value,
    ) -> bitrouter_core::errors::Result<()> {
        if self.is_disabled() {
            return Ok(());
        }

        let Some(restrictions) = self.param_restrictions_for(provider) else {
            return Ok(());
        };

        // `ParamRestrictions::check` operates on `Option<Map<String, Value>>`.
        // Bridge from `serde_json::Value` to the expected shape. We clone
        // rather than `mem::take` so the caller's value stays intact on error.
        if let serde_json::Value::Object(map) = arguments {
            let mut opt_map = Some(map.clone());
            restrictions.check(tool_name, &mut opt_map)?;
            if let Some(checked) = opt_map {
                *map = checked;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_config::ToolProviderPolicy;
    use bitrouter_core::routers::admin::{ParamRule, ParamViolationAction};
    use std::collections::HashMap;

    fn config_with_filter(provider: &str, filter: ToolFilter) -> ToolGuardrailConfig {
        let mut providers = HashMap::new();
        providers.insert(
            provider.to_owned(),
            ToolProviderPolicy {
                filter: Some(filter),
                param_restrictions: None,
            },
        );
        ToolGuardrailConfig {
            enabled: true,
            providers,
        }
    }

    fn config_with_restrictions(
        provider: &str,
        restrictions: ParamRestrictions,
    ) -> ToolGuardrailConfig {
        let mut providers = HashMap::new();
        providers.insert(
            provider.to_owned(),
            ToolProviderPolicy {
                filter: None,
                param_restrictions: Some(restrictions),
            },
        );
        ToolGuardrailConfig {
            enabled: true,
            providers,
        }
    }

    #[test]
    fn disabled_engine_is_noop() {
        let config = ToolGuardrailConfig {
            enabled: false,
            ..Default::default()
        };
        let g = ToolGuardrail::new(config);
        assert!(g.is_disabled());

        let mut args = serde_json::json!({"force": true});
        assert!(g.check_params("github", "search", &mut args).is_ok());
        // Args unchanged
        assert_eq!(args, serde_json::json!({"force": true}));
    }

    #[test]
    fn filter_for_returns_configured_filter() {
        let config = config_with_filter(
            "github",
            ToolFilter {
                allow: None,
                deny: Some(vec!["delete_repo".to_owned()]),
            },
        );
        let g = ToolGuardrail::new(config);

        let filter = g.filter_for("github");
        assert!(filter.is_some());
        assert!(!filter.map_or(true, |f| f.accepts("delete_repo")));
        assert!(filter.map_or(false, |f| f.accepts("search")));

        // Unknown provider returns None
        assert!(g.filter_for("jira").is_none());
    }

    #[test]
    fn check_params_rejects_denied_parameter() {
        let restrictions = ParamRestrictions {
            rules: HashMap::from([(
                "search".to_owned(),
                ParamRule {
                    deny: Some(vec!["force".to_owned()]),
                    allow: None,
                    action: ParamViolationAction::Reject,
                },
            )]),
        };
        let config = config_with_restrictions("github", restrictions);
        let g = ToolGuardrail::new(config);

        let mut args = serde_json::json!({"query": "test", "force": true});
        let result = g.check_params("github", "search", &mut args);
        assert!(result.is_err());
    }

    #[test]
    fn check_params_strips_denied_parameter() {
        let restrictions = ParamRestrictions {
            rules: HashMap::from([(
                "search".to_owned(),
                ParamRule {
                    deny: Some(vec!["force".to_owned()]),
                    allow: None,
                    action: ParamViolationAction::Strip,
                },
            )]),
        };
        let config = config_with_restrictions("github", restrictions);
        let g = ToolGuardrail::new(config);

        let mut args = serde_json::json!({"query": "test", "force": true});
        assert!(g.check_params("github", "search", &mut args).is_ok());
        assert_eq!(args, serde_json::json!({"query": "test"}));
    }

    #[test]
    fn check_params_passthrough_with_no_restrictions() {
        let g = ToolGuardrail::new(ToolGuardrailConfig::default());

        let mut args = serde_json::json!({"query": "test"});
        assert!(g.check_params("github", "search", &mut args).is_ok());
        assert_eq!(args, serde_json::json!({"query": "test"}));
    }

    #[test]
    fn check_params_handles_non_object_args() {
        let restrictions = ParamRestrictions {
            rules: HashMap::from([(
                "search".to_owned(),
                ParamRule {
                    deny: Some(vec!["force".to_owned()]),
                    allow: None,
                    action: ParamViolationAction::Reject,
                },
            )]),
        };
        let config = config_with_restrictions("github", restrictions);
        let g = ToolGuardrail::new(config);

        // Non-object values pass through unchanged
        let mut args = serde_json::Value::Null;
        assert!(g.check_params("github", "search", &mut args).is_ok());

        let mut args = serde_json::json!("string_arg");
        assert!(g.check_params("github", "search", &mut args).is_ok());
    }

    #[test]
    fn has_restrictions_for_checks_non_empty_rules() {
        let restrictions = ParamRestrictions {
            rules: HashMap::from([(
                "search".to_owned(),
                ParamRule {
                    deny: Some(vec!["force".to_owned()]),
                    allow: None,
                    action: ParamViolationAction::Reject,
                },
            )]),
        };
        let config = config_with_restrictions("github", restrictions);
        let g = ToolGuardrail::new(config);

        assert!(g.has_restrictions_for("github"));
        assert!(!g.has_restrictions_for("jira"));
    }
}
