//! Downstream path — what remains of the down-facing ACP `Agent` endpoint.
//!
//! The endpoint itself (`serve` / `serve_with` / `ServeExtensions`) is gone:
//! `bitrouter acp serve` runs [`crate::acp::controller`] instead, and nothing
//! called it since the controller landed. What survives is
//! [`ProviderSurface`], the router's provider/model routing surface in ACP's
//! own nouns, because the app layer's `chat` picker still drives it. It is
//! deleted with the picker migration that moves `chat` onto the shared client
//! and `_bitrouter/route/*`.

use agent_client_protocol::schema::v1::{ProviderInfo, SetProviderRequest};

/// The router's provider/model routing surface, in ACP's own nouns.
///
/// Implemented by the app layer, which owns the routing catalog. **No
/// implementation may put a credential in a response** — `ProviderCurrentConfig`
/// is specified as non-secret routing config, and credential management stays
/// on the CLI.
#[async_trait::async_trait]
pub trait ProviderSurface: Send + Sync {
    /// The routing catalog, with `current` reflecting the effective route.
    async fn list(&self) -> Vec<ProviderInfo>;
    /// Point this session's route at `provider_id`. `Err` carries a message
    /// the manager can render.
    async fn set(&self, request: SetProviderRequest) -> std::result::Result<(), String>;
}
