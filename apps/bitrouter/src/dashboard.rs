//! Canonical Code entry point and target assembly.

use std::path::Path;

use anyhow::Result;

use crate::acp_cli::{RoutingOptions, SessionSelection};
use crate::contexts::RemoteContext;

/// An optional ACP session to open when Code starts.
#[derive(Clone)]
pub struct SessionRequest {
    pub agent: String,
    pub selection: SessionSelection,
    pub turn_timeout: Option<u64>,
    pub routing: RoutingOptions,
}

/// Open the shared conversation surface for a local coding session or an
/// operations-only local/remote target.
pub async fn run(
    remote: Option<(String, RemoteContext)>,
    config: Option<&Path>,
    socket: Option<&Path>,
    initial_session: Option<SessionRequest>,
) -> Result<()> {
    let services = crate::actions::code::CodeServices::open(remote, config, socket).await?;
    crate::chat::code::run(services, initial_session).await
}
