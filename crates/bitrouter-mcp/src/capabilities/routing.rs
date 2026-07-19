//! The routing-introspection port: the `route_preview` tool's backing query.
//!
//! Read-only — it previews how BitRouter *would* route a model/prompt without
//! sending anything upstream. The app-side adapter reads the policy table and
//! the model registry; the crate stays registry-free and only owns the tool's
//! argument shape.

use crate::error::ToolError;

/// Arguments to `route_preview`.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RoutePreviewArgs {
    /// The model selector to preview (as you'd pass to `complete`).
    pub model: String,
    /// Optional prompt text. Used to derive the agent-loop step the policy
    /// table keys on; omit for a bare model resolution.
    pub prompt: Option<String>,
}

/// Preview BitRouter's routing for a model/prompt. Returns pre-built JSON (the
/// policy decision, the resolved provider chain, and a cost estimate) — the
/// crate never touches the policy table or registry itself.
#[async_trait::async_trait]
pub trait RoutingQuery: Send + Sync {
    /// Preview how `args` would route, or a `ToolError` when routing isn't
    /// configured / the model doesn't resolve.
    async fn preview(&self, args: RoutePreviewArgs) -> Result<serde_json::Value, ToolError>;
}
