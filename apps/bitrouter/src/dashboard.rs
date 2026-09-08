//! Canonical Code entry point and target assembly.
//!
//! The former permanent dashboard now delegates to one conversation loop.

use std::path::Path;

use anyhow::Result;

use crate::contexts::RemoteContext;

/// Optional initial ACP session selected by `bitrouter code <agent>`.
pub struct SessionRequest {
    pub agent: String,
    pub selection: crate::acp_cli::SessionSelection,
    pub turn_timeout: Option<u64>,
    pub routing: crate::acp_cli::RoutingOptions,
}

/// Open a coding conversation or the target's read-only operations inspector.
pub async fn run(
    remote: Option<(String, RemoteContext)>,
    config: Option<&Path>,
    socket: Option<&Path>,
    initial_session: Option<SessionRequest>,
) -> Result<()> {
    let services = crate::actions::code::CodeServices::open(remote, config, socket).await?;
    crate::chat::code::run(services, initial_session).await
}
