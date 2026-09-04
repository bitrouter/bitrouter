//! The `list_models` action: *what can BitRouter route, and who can serve it?*
//!
//! One report type, shared by `bitrouter models` and the MCP `list_models`
//! tool, so the CLI's `--json` and the tool's structured content are the same
//! bytes.
//!
//! The element type is [`ModelInfo`] itself — `bitrouter-sdk`'s, not a copy.
//! The copy this crate used to keep held a single `provider: String` and was
//! filled with `providers.first()`, so every model served by more than one
//! provider lost its fallback chain on the way to an agent: it asked what could
//! serve a model and was told one answer where there were three. The wire has
//! always carried the whole list.

use bitrouter_sdk::language_model::routing::ModelInfo;

use crate::backend::CallerAuth;
use crate::error::ToolError;

/// Which view of the catalog answered.
///
/// The two are not interchangeable, and an agent deciding whether it may route
/// a model needs to know which it is holding: [`Self::Live`] is what a running
/// router *will* accept right now, [`Self::Config`] is what a configuration
/// *would* accept if one were started.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ModelsSource {
    /// A running router answered: the local daemon's live routing table, or a
    /// metered account's catalog. Reflects `reload`s and any provider whose
    /// credential is resolved at start-up rather than declared in the config.
    Live,
    /// No router was reachable, so the catalog was projected from static
    /// configuration. Honest but weaker: a provider that only becomes routable
    /// once a daemon resolves its credential is missing here, and a config
    /// edited since a daemon started would be listed even though that daemon
    /// would refuse it.
    Config,
}

/// Every model BitRouter can route, each with the providers that can serve it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct ModelsReport {
    /// The routable models. Each carries **every** provider that declares it,
    /// in fallback order — not just the first.
    pub models: Vec<ModelInfo>,
    /// Which view produced [`Self::models`].
    pub resolved_via: ModelsSource,
}

impl ModelsReport {
    /// A running router's own catalog.
    pub fn live(models: Vec<ModelInfo>) -> Self {
        Self {
            models,
            resolved_via: ModelsSource::Live,
        }
    }

    /// A static configuration's projection, used when no router answered.
    pub fn from_config(models: Vec<ModelInfo>) -> Self {
        Self {
            models,
            resolved_via: ModelsSource::Config,
        }
    }

    /// Keep only the models `provider` can serve; `None` keeps everything.
    ///
    /// The filter lives on the report rather than in the port so that
    /// `bitrouter models --provider` and the tool's `provider` argument are
    /// the *same* filter over the *same* list. A port-side filter would let
    /// each surface interpret "declared by this provider" its own way, which is
    /// the drift this action exists to remove.
    pub fn filtered(mut self, provider: Option<&str>) -> Self {
        if let Some(provider) = provider {
            self.models.retain(|m| m.providers.iter().any(|p| p == provider));
        }
        self
    }
}

/// Arguments to the `list_models` tool — the tool half of
/// `bitrouter models --provider`.
#[derive(Debug, Clone, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct ListModelsArgs {
    /// Show only models this provider declares. Omit for every routable model.
    pub provider: Option<String>,
}

/// The `list_models` port. `caller` carries the MCP caller's own bearer so a
/// multi-tenant HTTP deployment lists *that* caller's catalog, never the
/// server's.
///
/// Filtering is deliberately absent: an implementation returns the whole
/// catalog and both surfaces narrow it with [`ModelsReport::filtered`].
#[async_trait::async_trait]
pub trait ModelsQuery: Send + Sync {
    /// Every routable model, or a `ToolError` when the lookup itself failed.
    async fn list_models(&self, caller: &CallerAuth) -> Result<ModelsReport, ToolError>;
}
