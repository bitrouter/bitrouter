//! The `status` action, implemented over the daemon's control socket.
//!
//! One implementation, two surfaces: `bitrouter status` calls
//! [`DaemonStatus::report`] directly, and the origin MCP server's `status` tool
//! calls it through the [`StatusQuery`] port. Both get the same
//! [`StatusReport`], so the CLI's `--json` and the tool's structured content
//! cannot drift.

use std::path::{Path, PathBuf};

use bitrouter_mcp::actions::status::{StatusQuery, StatusReport};
use bitrouter_mcp::backend::CallerAuth;
use bitrouter_mcp::error::ToolError;

use crate::daemon::{self, DaemonCommand, DaemonResponse};

/// Reads BitRouter's state off a control socket.
pub struct DaemonStatus {
    socket: PathBuf,
}

impl DaemonStatus {
    /// Probe the daemon listening on `socket`.
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    /// Ask the daemon how it is doing.
    ///
    /// Nothing listening is `running: false`, not an error — the CLI has always
    /// exited 0 on a stopped daemon, and an agent polling for health has to be
    /// able to tell "down" from "broken". Everything else (permission denied, a
    /// malformed response, a daemon answering something other than `Status`) is
    /// a real failure and propagates.
    pub async fn report(&self) -> anyhow::Result<StatusReport> {
        report_over(&self.socket).await
    }
}

/// The probe itself, split out so it needs no `self` and can be called from the
/// CLI path without constructing a port.
async fn report_over(socket: &Path) -> anyhow::Result<StatusReport> {
    match daemon::send_command(socket, &DaemonCommand::Status).await {
        Ok(DaemonResponse::Status {
            pid,
            listen,
            models,
            providers,
        }) => Ok(StatusReport::running(
            pid,
            listen,
            models,
            providers,
            socket.display().to_string(),
        )),
        Ok(DaemonResponse::Error { message }) => Err(anyhow::anyhow!(message)),
        Ok(other) => Err(anyhow::anyhow!("unexpected response: {other:?}")),
        // No daemon listening on the socket → report stopped, not error.
        // Anything else (permission denied, malformed response, …) is a
        // real failure and bubbles to the pretty reporter.
        Err(e) if daemon::is_not_reachable(&e) => {
            Ok(StatusReport::stopped(socket.display().to_string()))
        }
        Err(e) => Err(e),
    }
}

#[async_trait::async_trait]
impl StatusQuery for DaemonStatus {
    /// `caller` is ignored: the control socket is a single-machine, single-
    /// tenant channel that reaches no upstream, so there is no credential to
    /// forward and nothing per-caller to scope. The parameter stays on the port
    /// because the cloud implementation of the same action does forward it.
    async fn status(&self, _caller: &CallerAuth) -> Result<StatusReport, ToolError> {
        self.report()
            .await
            .map_err(|e| ToolError::new(e.to_string()))
    }
}
