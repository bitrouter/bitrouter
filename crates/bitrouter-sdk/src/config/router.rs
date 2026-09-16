//! Internal effective-router normalization.
//!
//! R1 keeps `presets:` as the only serialized input, but downstream readers
//! consume this one representation. Later router syntax can normalize into the
//! same shape without teaching resolution and policy validation a second
//! config model.

use crate::config::{PresetConfig, RoutingConfig};
use crate::language_model::routing::PromptOverrides;

/// One normalized router selection.
///
/// A missing model is retained here because legacy preset parsing accepts it;
/// the existing resolver and policy validation boundaries report the error
/// only when that definition is consumed.
pub(super) enum EffectiveRouterSelection<'a> {
    Model {
        model: Option<&'a str>,
    },
    Policy {
        policy: &'a str,
        base_model: Option<&'a str>,
    },
}

/// Request defaults supplied by one normalized router definition.
pub(super) struct EffectiveRouterDefaults<'a> {
    system_prompt: Option<&'a str>,
    params: &'a serde_json::Map<String, serde_json::Value>,
}

impl EffectiveRouterDefaults<'_> {
    pub(super) fn to_prompt_overrides(&self) -> PromptOverrides {
        PromptOverrides {
            system_prompt: self.system_prompt.map(ToOwned::to_owned),
            params: self.params.clone(),
        }
    }
}

/// The single definition consumed after a config input has been normalized.
pub(super) struct EffectiveRouterDefinition<'a> {
    pub(super) selection: EffectiveRouterSelection<'a>,
    pub(super) defaults: EffectiveRouterDefaults<'a>,
    pub(super) routing: &'a RoutingConfig,
}

impl<'a> EffectiveRouterDefinition<'a> {
    /// Translate one legacy preset without changing when invalid legacy input
    /// is rejected.
    pub(super) fn from_legacy_preset(preset: &'a PresetConfig) -> Self {
        let model = preset.model.as_deref();
        let selection = match preset.policy.as_deref() {
            Some(policy) => EffectiveRouterSelection::Policy {
                policy,
                base_model: model,
            },
            None => EffectiveRouterSelection::Model { model },
        };
        Self {
            selection,
            defaults: EffectiveRouterDefaults {
                system_prompt: preset.system_prompt.as_deref(),
                params: &preset.params,
            },
            routing: &preset.routing,
        }
    }

    pub(super) fn base_model(&self) -> Option<&'a str> {
        match self.selection {
            EffectiveRouterSelection::Model { model } => model,
            EffectiveRouterSelection::Policy { base_model, .. } => base_model,
        }
    }

    pub(super) fn policy(&self) -> Option<&'a str> {
        match self.selection {
            EffectiveRouterSelection::Model { .. } => None,
            EffectiveRouterSelection::Policy { policy, .. } => Some(policy),
        }
    }

    pub(super) fn into_policy_binding(self) -> Option<(&'a str, Option<&'a str>)> {
        match self.selection {
            EffectiveRouterSelection::Model { .. } => None,
            EffectiveRouterSelection::Policy { policy, base_model } => Some((policy, base_model)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_model_preset_normalizes_to_model_selection() {
        let preset = PresetConfig {
            model: Some("vendor:base".into()),
            system_prompt: Some("Be precise".into()),
            params: serde_json::Map::from_iter([("temperature".into(), 0.2.into())]),
            routing: RoutingConfig {
                only: vec!["vendor".into()],
                ..RoutingConfig::default()
            },
            ..PresetConfig::default()
        };

        let router = EffectiveRouterDefinition::from_legacy_preset(&preset);

        assert_eq!(router.base_model(), Some("vendor:base"));
        assert_eq!(router.policy(), None);
        assert_eq!(router.routing.only, ["vendor"]);
        let defaults = router.defaults.to_prompt_overrides();
        assert_eq!(defaults.system_prompt.as_deref(), Some("Be precise"));
        assert_eq!(defaults.params["temperature"], 0.2);
    }

    #[test]
    fn legacy_policy_preset_keeps_exact_base_model() {
        let preset = PresetConfig {
            model: Some("vendor:strong".into()),
            policy: Some("coding".into()),
            ..PresetConfig::default()
        };

        let router = EffectiveRouterDefinition::from_legacy_preset(&preset);

        assert_eq!(router.base_model(), Some("vendor:strong"));
        assert_eq!(router.policy(), Some("coding"));
    }

    #[test]
    fn legacy_missing_model_remains_deferred_for_existing_consumers() {
        let preset = PresetConfig {
            policy: Some("coding".into()),
            ..PresetConfig::default()
        };

        let router = EffectiveRouterDefinition::from_legacy_preset(&preset);

        assert_eq!(router.base_model(), None);
        assert_eq!(router.policy(), Some("coding"));
    }

    #[test]
    fn policy_bindings_follow_direct_config_mutation_without_a_cache() {
        let mut config = crate::config::Config::default();
        config.presets.insert(
            "coding".into(),
            PresetConfig {
                policy: Some("first".into()),
                ..PresetConfig::default()
            },
        );

        let missing_base = config.router_policy_bindings().collect::<Vec<_>>();
        assert_eq!(missing_base, [("coding", "first", None)]);

        config.presets.insert(
            "coding".into(),
            PresetConfig {
                model: Some("vendor:strong".into()),
                policy: Some("second".into()),
                ..PresetConfig::default()
            },
        );

        let updated = config.router_policy_bindings().collect::<Vec<_>>();
        assert_eq!(updated, [("coding", "second", Some("vendor:strong"))]);
    }
}
