use bitrouter_core::routers::admin::{ParamRestrictions, ToolFilter};
use serde::{Deserialize, Serialize};

/// Per-provider tool policy configuration.
///
/// Combines visibility filtering (which tools are discoverable) with
/// parameter restrictions (which arguments are allowed at call time).
/// Used in policy files under the `tool_rules` key.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolProviderPolicy {
    /// Visibility filter controlling which tools appear in discovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<ToolFilter>,

    /// Parameter restriction rules applied at tool call time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub param_restrictions: Option<ParamRestrictions>,
}
