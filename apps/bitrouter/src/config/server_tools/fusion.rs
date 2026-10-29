//! Deployment defaults for the optional Fusion tool and model alias.

use serde::Deserialize;

/// The `server_tools.fusion` config section. Its presence enables the Fusion
/// server tool and the `bitrouter/fusion` model alias; its fields supply the
/// alias defaults (the panel/judge a bare `bitrouter/fusion` request expands
/// to). An explicit `bitrouter:fusion` declaration on a request overrides these.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct FusionSettings {
    /// Default panel models the alias expands to.
    pub panel: Vec<String>,
    /// Default judge model (defaults to the first panel model).
    pub judge: Option<String>,
    /// Optional dedicated synthesizer model.
    pub synthesizer: Option<String>,
    /// Alias slug (defaults to `bitrouter/fusion`). Custom aliases cannot use
    /// `@preset`, `bitrouter:`, or the reserved `bitrouter/` namespace.
    pub alias: Option<String>,
    /// The model the alias resolves to (defaults to the judge, then the first
    /// panel model).
    pub outer_model: Option<String>,
    /// Provider web tools forwarded to every panel member, in
    /// provider-namespaced declaration form (e.g. `{type: "<provider>:<tool>"}`).
    pub web_tools: Vec<serde_json::Value>,
}
