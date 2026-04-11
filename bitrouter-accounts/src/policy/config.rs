use bitrouter_core::routers::admin::ToolFilter;
use serde::{Deserialize, Serialize};

/// Per-provider tool policy configuration.
///
/// Defines an allow-list controlling which tools from this provider are
/// visible and callable. Used in policy files under the `tool_rules` key.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolProviderPolicy {
    /// Visibility filter controlling which tools appear in discovery
    /// and are callable at request time.
    #[serde(default, flatten)]
    pub filter: ToolFilter,
}
