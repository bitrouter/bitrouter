//! Named router configuration and effective-router normalization.
//!
//! `RouterConfig` is the user-facing input introduced by the router migration.
//! Resolution and policy consumers use the borrowed
//! `EffectiveRouterDefinition` below, which is also the normalization target
//! for legacy `presets:`. Keeping one effective representation prevents the
//! compatibility syntax from growing a second execution path.

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use crate::config::checker::CheckerConfig;
use crate::config::{Config, PresetConfig, RoutingConfig};
use crate::error::{BitrouterError, Result};
use crate::language_model::request_checks::RequestCheckBinding;
use crate::language_model::routing::{PromptOverrides, SortOrder};

/// One named router definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RouterConfig {
    /// The model or policy selection performed by this router.
    pub selection: RouterSelection,
    /// Request values filled only when the caller omitted them.
    #[serde(default)]
    pub defaults: RouterDefaults,
    /// Fail-closed checks run before this router may contact an upstream.
    #[serde(default)]
    pub checks: RouterChecks,
}

/// Request checks attached to a named router.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RouterChecks {
    /// Ordered remote checks. Every checker must allow the request.
    pub request: Vec<RouterRequestCheck>,
}

/// One router-to-checker binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RouterRequestCheck {
    /// Checker id under the top-level `checkers` map.
    pub checker: String,
    /// Total checker deadline, including queue admission and response body reads.
    #[serde(default = "default_checker_timeout_ms")]
    pub timeout_ms: u64,
    /// Maximum projected text bytes. Oversize input is rejected, never truncated.
    #[serde(default = "default_checker_max_input_bytes")]
    pub max_input_bytes: u64,
}

/// Default total checker deadline.
pub const DEFAULT_CHECKER_TIMEOUT_MS: u64 = 500;
/// Longest configurable checker deadline.
pub const MAX_CHECKER_TIMEOUT_MS: u64 = 30_000;
/// Default serialized checker invocation limit.
pub const DEFAULT_CHECKER_MAX_INPUT_BYTES: u64 = 256 * 1024;
/// Largest configurable serialized checker invocation limit.
pub const MAX_CHECKER_INPUT_BYTES: u64 = 4 * 1024 * 1024;
/// Maximum number of request checks on one router.
pub const MAX_REQUEST_CHECKS_PER_ROUTER: usize = 16;

const fn default_checker_timeout_ms() -> u64 {
    DEFAULT_CHECKER_TIMEOUT_MS
}

const fn default_checker_max_input_bytes() -> u64 {
    DEFAULT_CHECKER_MAX_INPUT_BYTES
}

impl RouterRequestCheck {
    /// Resolve this config entry into the redaction-safe runtime binding.
    pub fn resolve(&self, router_id: &str, checker: &CheckerConfig) -> Result<RequestCheckBinding> {
        #[derive(Serialize)]
        struct DigestInput<'a> {
            version: &'static str,
            router_id: &'a str,
            checker_id: &'a str,
            endpoint: &'a str,
            credential_env: Option<&'a str>,
            contract_version: u16,
            timeout_ms: u64,
            max_input_bytes: u64,
        }

        let canonical = serde_json::to_vec(&DigestInput {
            version: "checker-binding-v1",
            router_id,
            checker_id: &self.checker,
            endpoint: &checker.endpoint,
            credential_env: checker.credential_env.as_deref(),
            contract_version: checker.contract_version,
            timeout_ms: self.timeout_ms,
            max_input_bytes: self.max_input_bytes,
        })
        .map_err(|error| {
            BitrouterError::internal(format!("serializing checker binding identity: {error}"))
        })?;
        Ok(RequestCheckBinding {
            checker_id: self.checker.clone(),
            binding_digest: format!(
                "checker-binding-v1:sha256:{}",
                hex::encode(Sha256::digest(canonical))
            ),
            max_input_bytes: self.max_input_bytes,
            timeout_ms: self.timeout_ms,
        })
    }
}

/// Model selection performed by a named router.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RouterSelection {
    /// Resolve one existing model selector.
    Model {
        /// Existing physical or virtual model selector.
        model: String,
        /// Provider preferences applied by the existing cascade resolver.
        #[serde(default)]
        #[schemars(with = "StrictRoutingConfig")]
        routing: RoutingConfig,
    },
    /// Let one existing policy choose the effective model.
    Policy {
        /// Policy name in `policy-lock.yaml`.
        policy: String,
        /// Exact base-model input retained by the current policy runtime.
        base_model: String,
        /// Provider preferences applied after the policy selects a model.
        #[serde(default)]
        #[schemars(with = "StrictRoutingConfig")]
        routing: RoutingConfig,
    },
}

/// Defaults supplied by a named router.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RouterDefaults {
    /// System prompt used only when the request did not provide one.
    pub system_prompt: Option<String>,
    /// Generation parameters shallow-merged behind explicit request values.
    pub params: serde_json::Map<String, serde_json::Value>,
}

/// Where a normalized router definition came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RouterConfigSource {
    /// A canonical entry under `routers:`.
    User,
    /// A compatibility entry normalized from `presets:`.
    LegacyPreset,
}

/// Redaction-safe selection summary for router diagnostics.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RouterInventorySelection {
    /// Fixed model selection. `None` is possible only for invalid legacy input.
    Model {
        /// Exact configured model selector, if present.
        model: Option<String>,
        /// Provider routing preferences applied to the selection.
        routing: RoutingConfig,
    },
    /// Policy selection. A missing base model is preserved for legacy validation.
    Policy {
        /// App-owned policy name.
        policy: String,
        /// Exact policy base-model input, if present.
        base_model: Option<String>,
        /// Provider routing preferences applied after policy selection.
        routing: RoutingConfig,
    },
}

/// Redaction-safe defaults summary for router diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct RouterDefaultsSummary {
    /// Whether a system prompt is configured, without exposing its contents.
    pub system_prompt_present: bool,
    /// Sorted configured parameter keys, without their values.
    pub param_keys: Vec<String>,
}

/// One normalized router available to read/validation consumers.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
pub struct RouterInventoryEntry {
    /// Router id used by canonical and compatibility addresses.
    pub id: String,
    /// Configuration surface that supplied this definition.
    pub source: RouterConfigSource,
    /// Normalized model or policy selection summary.
    pub selection: RouterInventorySelection,
    /// Redacted request-default shape.
    pub defaults: RouterDefaultsSummary,
    /// Versioned digest of the redaction-safe effective binding.
    pub binding_digest: String,
}

/// Provider routing preferences accepted by a named router.
#[derive(Default, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(rename = "RouterRoutingConfig")]
struct StrictRoutingConfig {
    sort: Option<SortOrder>,
    require_tags: Vec<String>,
    only: Vec<String>,
    ignore: Vec<String>,
}

impl From<StrictRoutingConfig> for RoutingConfig {
    fn from(value: StrictRoutingConfig) -> Self {
        Self {
            sort: value.sort,
            require_tags: value.require_tags,
            only: value.only,
            ignore: value.ignore,
        }
    }
}

impl<'de> Deserialize<'de> for RouterSelection {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
        enum Input {
            Model {
                model: String,
                #[serde(default)]
                routing: StrictRoutingConfig,
            },
            Policy {
                policy: String,
                base_model: String,
                #[serde(default)]
                routing: StrictRoutingConfig,
            },
        }

        Ok(match Input::deserialize(deserializer)? {
            Input::Model { model, routing } => Self::Model {
                model,
                routing: routing.into(),
            },
            Input::Policy {
                policy,
                base_model,
                routing,
            } => Self::Policy {
                policy,
                base_model,
                routing: routing.into(),
            },
        })
    }
}

/// One normalized router selection.
///
/// A missing model is retained here because legacy preset parsing accepts it;
/// the existing resolver and policy validation boundaries report the error
/// only when that definition is consumed.
#[derive(Clone, Copy)]
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
#[derive(Clone, Copy)]
pub(super) struct EffectiveRouterDefaults<'a> {
    system_prompt: Option<&'a str>,
    params: &'a serde_json::Map<String, serde_json::Value>,
}

impl EffectiveRouterDefaults<'_> {
    pub(super) fn to_prompt_overrides(self) -> PromptOverrides {
        PromptOverrides {
            system_prompt: self.system_prompt.map(ToOwned::to_owned),
            params: self.params.clone(),
        }
    }
}

/// The single definition consumed after a config input has been normalized.
#[derive(Clone, Copy)]
pub(super) struct EffectiveRouterDefinition<'a> {
    pub(super) selection: EffectiveRouterSelection<'a>,
    pub(super) defaults: EffectiveRouterDefaults<'a>,
    pub(super) routing: &'a RoutingConfig,
    checks: &'a [RouterRequestCheck],
}

impl<'a> EffectiveRouterDefinition<'a> {
    /// Translate one named router.
    pub(super) fn from_router(router: &'a RouterConfig) -> Self {
        let (selection, routing) = match &router.selection {
            RouterSelection::Model { model, routing } => (
                EffectiveRouterSelection::Model { model: Some(model) },
                routing,
            ),
            RouterSelection::Policy {
                policy,
                base_model,
                routing,
            } => (
                EffectiveRouterSelection::Policy {
                    policy,
                    base_model: Some(base_model),
                },
                routing,
            ),
        };
        Self {
            selection,
            defaults: EffectiveRouterDefaults {
                system_prompt: router.defaults.system_prompt.as_deref(),
                params: &router.defaults.params,
            },
            routing,
            checks: &router.checks.request,
        }
    }

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
            checks: &[],
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

    pub(super) fn request_checks(
        &self,
        router_id: &str,
        checkers: &std::collections::HashMap<String, CheckerConfig>,
    ) -> Result<Vec<RequestCheckBinding>> {
        self.checks
            .iter()
            .map(|binding| {
                let checker = checkers.get(&binding.checker).ok_or_else(|| {
                    BitrouterError::bad_request(format!(
                        "router '{router_id}' request check references unknown checker '{}'",
                        binding.checker
                    ))
                })?;
                binding.resolve(router_id, checker)
            })
            .collect()
    }

    /// Redaction-safe identity for one effective router binding.
    ///
    /// The versioned digest covers routing behavior, checker bindings, and the
    /// presence/key shape of defaults. It deliberately excludes prompt and
    /// parameter values (and hashes of those values), which could disclose
    /// enumerable secrets. The policy artifact digest remains a separate
    /// identity owned by the policy runtime.
    pub(super) fn binding_digest(
        &self,
        router_id: &str,
        checkers: &std::collections::HashMap<String, CheckerConfig>,
    ) -> Result<String> {
        #[derive(Serialize)]
        #[serde(tag = "kind", rename_all = "snake_case")]
        enum DigestSelection<'a> {
            Model {
                model: Option<&'a str>,
            },
            Policy {
                policy: &'a str,
                base_model: Option<&'a str>,
            },
        }

        #[derive(Serialize)]
        struct DigestDefaults<'a> {
            system_prompt_present: bool,
            param_keys: Vec<&'a str>,
        }

        #[derive(Serialize)]
        struct DigestInput<'a> {
            version: &'static str,
            router_id: &'a str,
            selection: DigestSelection<'a>,
            routing: &'a RoutingConfig,
            defaults: DigestDefaults<'a>,
            #[serde(skip_serializing_if = "Vec::is_empty")]
            checks: Vec<DigestCheck<'a>>,
        }

        #[derive(Serialize)]
        struct DigestCheck<'a> {
            checker: &'a str,
            endpoint: &'a str,
            credential_env: Option<&'a str>,
            contract_version: u16,
            timeout_ms: u64,
            max_input_bytes: u64,
        }

        let selection = match self.selection {
            EffectiveRouterSelection::Model { model } => DigestSelection::Model { model },
            EffectiveRouterSelection::Policy { policy, base_model } => {
                DigestSelection::Policy { policy, base_model }
            }
        };
        let mut param_keys = self
            .defaults
            .params
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        param_keys.sort_unstable();
        let checks = self
            .checks
            .iter()
            .filter_map(|binding| {
                checkers.get(&binding.checker).map(|checker| DigestCheck {
                    checker: &binding.checker,
                    endpoint: &checker.endpoint,
                    credential_env: checker.credential_env.as_deref(),
                    contract_version: checker.contract_version,
                    timeout_ms: binding.timeout_ms,
                    max_input_bytes: binding.max_input_bytes,
                })
            })
            .collect::<Vec<_>>();
        let digest_version = if checks.is_empty() {
            "router-v1"
        } else {
            "router-v2"
        };
        let canonical = serde_json::to_vec(&DigestInput {
            version: digest_version,
            router_id,
            selection,
            routing: self.routing,
            defaults: DigestDefaults {
                system_prompt_present: self.defaults.system_prompt.is_some(),
                param_keys,
            },
            checks,
        })
        .map_err(|error| {
            BitrouterError::internal(format!("serializing router binding identity: {error}"))
        })?;
        Ok(format!(
            "{digest_version}:sha256:{}",
            hex::encode(Sha256::digest(canonical))
        ))
    }

    fn inventory_entry(
        &self,
        router_id: &str,
        source: RouterConfigSource,
        checkers: &std::collections::HashMap<String, CheckerConfig>,
    ) -> Result<RouterInventoryEntry> {
        let selection = match self.selection {
            EffectiveRouterSelection::Model { model } => RouterInventorySelection::Model {
                model: model.map(ToOwned::to_owned),
                routing: self.routing.clone(),
            },
            EffectiveRouterSelection::Policy { policy, base_model } => {
                RouterInventorySelection::Policy {
                    policy: policy.to_owned(),
                    base_model: base_model.map(ToOwned::to_owned),
                    routing: self.routing.clone(),
                }
            }
        };
        let mut param_keys = self.defaults.params.keys().cloned().collect::<Vec<_>>();
        param_keys.sort_unstable();
        Ok(RouterInventoryEntry {
            id: router_id.to_owned(),
            source,
            selection,
            defaults: RouterDefaultsSummary {
                system_prompt_present: self.defaults.system_prompt.is_some(),
                param_keys,
            },
            binding_digest: self.binding_digest(router_id, checkers)?,
        })
    }
}

pub(super) fn router_inventory(config: &Config) -> Result<Vec<RouterInventoryEntry>> {
    validate_router_config(config)?;
    let mut entries = Vec::with_capacity(config.routers.len() + config.presets.len());
    for (id, router) in &config.routers {
        entries.push(
            EffectiveRouterDefinition::from_router(router).inventory_entry(
                id,
                RouterConfigSource::User,
                &config.checkers,
            )?,
        );
    }
    for (id, preset) in &config.presets {
        entries.push(
            EffectiveRouterDefinition::from_legacy_preset(preset).inventory_entry(
                id,
                RouterConfigSource::LegacyPreset,
                &config.checkers,
            )?,
        );
    }
    entries.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(entries)
}

pub(super) fn validate_router_config(config: &Config) -> Result<()> {
    for (checker_id, checker) in &config.checkers {
        if !valid_router_id(checker_id) {
            return Err(BitrouterError::bad_request(format!(
                "invalid checker id '{checker_id}' (use a lowercase letter followed by up to 63 lowercase letters, digits, '_' or '-')"
            )));
        }
        checker.validate(checker_id)?;
    }

    for router_id in config.routers.keys() {
        if !valid_router_id(router_id) {
            return Err(BitrouterError::bad_request(format!(
                "invalid router id '{router_id}' (use a lowercase letter followed by up to 63 lowercase letters, digits, '_' or '-')"
            )));
        }
        if config.presets.contains_key(router_id) {
            return Err(BitrouterError::bad_request(format!(
                "router '{router_id}' conflicts with legacy preset '@{router_id}'"
            )));
        }
        if router_id == "fusion" {
            return Err(BitrouterError::bad_request(
                "router id 'fusion' is reserved by server_tools.fusion",
            ));
        }
    }

    for (router_id, router) in &config.routers {
        if router.checks.request.len() > MAX_REQUEST_CHECKS_PER_ROUTER {
            return Err(BitrouterError::bad_request(format!(
                "router '{router_id}' checks.request may contain at most {MAX_REQUEST_CHECKS_PER_ROUTER} bindings"
            )));
        }
        for binding in &router.checks.request {
            if !config.checkers.contains_key(&binding.checker) {
                return Err(BitrouterError::bad_request(format!(
                    "router '{router_id}' request check references unknown checker '{}'",
                    binding.checker
                )));
            }
            if binding.timeout_ms == 0 || binding.timeout_ms > MAX_CHECKER_TIMEOUT_MS {
                return Err(BitrouterError::bad_request(format!(
                    "router '{router_id}' checker '{}' timeout_ms must be between 1 and {MAX_CHECKER_TIMEOUT_MS}",
                    binding.checker
                )));
            }
            if binding.max_input_bytes == 0 || binding.max_input_bytes > MAX_CHECKER_INPUT_BYTES {
                return Err(BitrouterError::bad_request(format!(
                    "router '{router_id}' checker '{}' max_input_bytes must be between 1 and {MAX_CHECKER_INPUT_BYTES}",
                    binding.checker
                )));
            }
        }
        match &router.selection {
            RouterSelection::Model { model, .. } => {
                if router_id == "auto" {
                    return Err(BitrouterError::bad_request(
                        "router 'auto' must use policy selection",
                    ));
                }
                validate_model_selector(router_id, "selection.model", model)?;
            }
            RouterSelection::Policy {
                policy, base_model, ..
            } => {
                if policy.trim().is_empty() {
                    return Err(BitrouterError::bad_request(format!(
                        "router '{router_id}' selection.policy must not be empty"
                    )));
                }
                validate_model_selector(router_id, "selection.base_model", base_model)?;
            }
        }
    }
    Ok(())
}

fn valid_router_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

fn validate_model_selector(router_id: &str, field: &str, selector: &str) -> Result<()> {
    if selector.trim().is_empty() {
        return Err(BitrouterError::bad_request(format!(
            "router '{router_id}' {field} must not be empty"
        )));
    }
    let recursive = selector.starts_with('@')
        || selector.starts_with(crate::config::presets::RESERVED_NAMESPACE)
        || matches!(selector.strip_prefix("bitrouter:"), Some("auto" | "fusion"));
    if recursive {
        return Err(BitrouterError::bad_request(format!(
            "router '{router_id}' {field} cannot reference a router or preset ('{selector}')"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model_router(model: &str) -> RouterConfig {
        RouterConfig {
            selection: RouterSelection::Model {
                model: model.to_owned(),
                routing: RoutingConfig::default(),
            },
            defaults: RouterDefaults::default(),
            checks: RouterChecks::default(),
        }
    }

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
        let mut config = Config::default();
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

    #[test]
    fn digest_excludes_default_values_but_tracks_keys_and_selection() -> Result<()> {
        let first = RouterConfig {
            selection: RouterSelection::Model {
                model: "vendor:first".into(),
                routing: RoutingConfig::default(),
            },
            defaults: RouterDefaults {
                system_prompt: Some("secret one".into()),
                params: serde_json::Map::from_iter([("temperature".into(), 0.2.into())]),
            },
            checks: RouterChecks::default(),
        };
        let values_changed = RouterConfig {
            selection: first.selection.clone(),
            defaults: RouterDefaults {
                system_prompt: Some("secret two".into()),
                params: serde_json::Map::from_iter([("temperature".into(), 0.9.into())]),
            },
            checks: RouterChecks::default(),
        };
        let selection_changed = RouterConfig {
            selection: RouterSelection::Model {
                model: "vendor:second".into(),
                routing: RoutingConfig::default(),
            },
            defaults: first.defaults.clone(),
            checks: RouterChecks::default(),
        };
        let keys_changed = RouterConfig {
            selection: first.selection.clone(),
            defaults: RouterDefaults {
                system_prompt: first.defaults.system_prompt.clone(),
                params: serde_json::Map::from_iter([("top_p".into(), 0.9.into())]),
            },
            checks: RouterChecks::default(),
        };

        let empty = std::collections::HashMap::new();
        let first_digest =
            EffectiveRouterDefinition::from_router(&first).binding_digest("coding", &empty)?;
        let values_digest = EffectiveRouterDefinition::from_router(&values_changed)
            .binding_digest("coding", &empty)?;
        let selection_digest = EffectiveRouterDefinition::from_router(&selection_changed)
            .binding_digest("coding", &empty)?;
        let keys_digest = EffectiveRouterDefinition::from_router(&keys_changed)
            .binding_digest("coding", &empty)?;

        assert!(first_digest.starts_with("router-v1:sha256:"));
        assert_eq!(first_digest, values_digest);
        assert_ne!(first_digest, selection_digest);
        assert_ne!(first_digest, keys_digest);
        Ok(())
    }

    #[test]
    fn validation_reads_direct_config_mutations() -> Result<()> {
        let mut config = Config::default();
        config
            .routers
            .insert("project".into(), model_router("vendor:base"));
        config.validate_router_config()?;

        config
            .routers
            .insert("project".into(), model_router("bitrouter/another-router"));
        let recursive = config
            .validate_router_config()
            .err()
            .ok_or_else(|| BitrouterError::internal("recursive router selector was accepted"))?;
        assert!(recursive.to_string().contains("cannot reference"));

        config
            .routers
            .insert("project".into(), model_router("vendor:base"));
        config.presets.insert(
            "project".into(),
            PresetConfig {
                model: Some("vendor:base".into()),
                ..PresetConfig::default()
            },
        );
        let collision = config
            .validate_router_config()
            .err()
            .ok_or_else(|| BitrouterError::internal("router/preset collision was accepted"))?;
        assert!(collision.to_string().contains("conflicts"));
        Ok(())
    }

    #[test]
    fn validation_rejects_invalid_and_reserved_router_ids() -> Result<()> {
        for router_id in ["Uppercase", "nested/path", "has:variant", "fusion"] {
            let mut config = Config::default();
            config
                .routers
                .insert(router_id.into(), model_router("vendor:base"));
            let error = config
                .validate_router_config()
                .err()
                .ok_or_else(|| BitrouterError::internal("invalid router id was accepted"))?;
            assert!(
                error.to_string().contains("invalid router id")
                    || error.to_string().contains("reserved")
            );
        }

        let mut auto = Config::default();
        auto.routers
            .insert("auto".into(), model_router("vendor:base"));
        let error = auto
            .validate_router_config()
            .err()
            .ok_or_else(|| BitrouterError::internal("fixed-model auto router was accepted"))?;
        assert!(error.to_string().contains("must use policy selection"));
        Ok(())
    }
}
