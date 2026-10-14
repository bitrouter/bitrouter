//! Typed model and router inspection contracts.

use crate::routing::{ModelInfo, RoutingConfig};

/// Which view of the catalog answered.
///
/// The two are not interchangeable, and an agent deciding whether it may route
/// a model needs to know which it is holding: [`Self::Live`] is what a running
/// router *will* accept right now, [`Self::Config`] is what a configuration
/// *would* accept if one were started.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelsSource {
    /// A running router answered: the local daemon's live routing table, or a
    /// metered account's catalog. Reflects `reload`s and the state the router
    /// actually resolved at start-up.
    Live,
    /// No router was reachable, so the catalog was projected from static
    /// configuration, resolved the way a daemon would resolve it at start-up
    /// (built-in defaults, stored credentials). Honest but weaker: it is what
    /// the config says *now*, so a file edited since a daemon started would be
    /// listed even though that daemon would refuse it, and nothing a running
    /// daemon learned after start-up is reflected.
    Config,
}

/// Where one effective router definition came from.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouterSource {
    /// A definition under the public `routers:` config section.
    User,
    /// A compatibility definition normalized from `presets:`.
    Legacy,
    /// The product's built-in coding initialization slot has no saved binding.
    Default,
}

/// Whether a router can be resolved against the inspected runtime/config.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouterReadiness {
    Ready,
    NotReady,
    Uninitialized,
    Unknown,
}

/// Public, non-secret selection portion of a normalized router definition.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RouterSelectionStatus {
    Model {
        model: Option<String>,
        routing: RoutingConfig,
    },
    Policy {
        policy: String,
        base_model: Option<String>,
        routing: RoutingConfig,
    },
}

/// One router as the inspected config/runtime understands it. Defaults expose
/// only field presence and parameter names; prompt and parameter values never
/// cross the diagnostics boundary.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RouterStatus {
    pub id: String,
    pub address: String,
    pub source: RouterSource,
    pub readiness: RouterReadiness,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection: Option<RouterSelectionStatus>,
    pub system_prompt_default: bool,
    #[serde(default)]
    pub parameter_defaults: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_digest: Option<String>,
}

/// Every model BitRouter can route, each with the providers that can serve it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelsReport {
    /// The routable models. Each carries **every** provider that declares it,
    /// in fallback order — not just the first.
    pub models: Vec<ModelInfo>,
    /// Which view produced [`Self::models`].
    pub resolved_via: ModelsSource,
    /// Router definitions from the same live/config source. `None` means the
    /// answering daemon predates router inventory support; it is not an empty
    /// successful inventory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routers: Option<Vec<RouterStatus>>,
}

impl ModelsReport {
    /// A running router's own catalog.
    pub fn live(models: Vec<ModelInfo>) -> Self {
        Self {
            models,
            resolved_via: ModelsSource::Live,
            routers: None,
        }
    }

    /// A static configuration's projection, used when no router answered.
    pub fn from_config(models: Vec<ModelInfo>) -> Self {
        Self {
            models,
            resolved_via: ModelsSource::Config,
            routers: None,
        }
    }

    pub fn with_routers(mut self, routers: Option<Vec<RouterStatus>>) -> Self {
        self.routers = routers;
        self
    }

    /// Keep only the models `provider` can serve; `None` keeps everything.
    ///
    /// The filter lives on the report rather than in the port so every adapter
    /// narrows the same list with the same rule.
    pub fn filtered(mut self, provider: Option<&str>) -> Self {
        if let Some(provider) = provider {
            self.models
                .retain(|m| m.providers.iter().any(|p| p == provider));
        }
        self
    }
}
