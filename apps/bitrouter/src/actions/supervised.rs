//! Local daemon discovery for supervised CLI starts.

use crate::daemon::{self, DaemonStartOutcome};
use crate::paths::ConfigSource;
use anyhow::{Result, bail, ensure};
use std::path::PathBuf;
use std::time::Duration;

/// Resolve the selected configuration's live endpoint, starting its daemon
/// only when the caller explicitly permits local startup.
pub(crate) async fn ensure_daemon(
    source: &ConfigSource,
    config: &crate::config::Config,
    no_start: bool,
) -> Result<PathBuf> {
    if let Some(located) = crate::daemon_locator::locate_source(source).await? {
        crate::upgrade::ensure_compatible(source, located.socket(), no_start).await?;
        return Ok(located.socket().to_path_buf());
    }
    let socket = daemon::socket_path_for(source, config);
    if daemon::probe_status(&socket).await?.is_some() {
        crate::upgrade::ensure_compatible(source, &socket, no_start).await?;
        return Ok(socket);
    }
    ensure!(
        !no_start,
        "Supervised sessions require the selected local daemon; start it or omit --no-start"
    );
    let log = source.home().join("bitrouter.log");
    match daemon::start_and_wait(source, &log, Some(&socket), Duration::from_secs(15)).await? {
        DaemonStartOutcome::Ready(_) => Ok(socket),
        DaemonStartOutcome::Exited { status, log_tail } => {
            bail!("Session supervisor exited ({status}): {log_tail}")
        }
        DaemonStartOutcome::NotReadyInTime { pid } => {
            bail!(
                "Session supervisor process {pid} has not become ready; inspect {} before retrying",
                log.display()
            )
        }
    }
}
