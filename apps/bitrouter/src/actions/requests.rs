//! Recent-request inspection shared by local CLI and remote control HTTP.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;

use crate::daemon::{self, DaemonCommand, DaemonResponse};
use crate::metering::store::TimeWindow;
use crate::output::reports::requests::{DaemonView, RequestsReport};
use crate::paths::ConfigSource;

/// Maximum number of rows any interactive/read API returns.
pub const MAX_REQUEST_ROWS: u64 = 500;

const DAEMON_PROBE_TIMEOUT: Duration = Duration::from_millis(750);

pub struct RequestsAction {
    source: ConfigSource,
    socket: PathBuf,
}

impl RequestsAction {
    pub fn new(source: ConfigSource, socket: PathBuf) -> Self {
        Self { source, socket }
    }

    /// Build one host-wide request snapshot.
    ///
    /// Metering reads remain best-effort as on the original local CLI: an
    /// unavailable store produces an empty report rather than hiding daemon
    /// health. Transport/auth/version failures are handled outside this action
    /// and remain hard errors for remote clients.
    pub async fn report(&self, limit: u64) -> Result<RequestsReport> {
        if !(1..=MAX_REQUEST_ROWS).contains(&limit) {
            anyhow::bail!("request limit must be between 1 and {MAX_REQUEST_ROWS}");
        }

        let window = TimeWindow::Today;
        let daemon = daemon_view(&self.socket).await;
        let Some(store) = crate::metering::reader::open_readonly(&self.source).await else {
            return Ok(RequestsReport::new(
                daemon,
                window,
                Default::default(),
                Default::default(),
                Vec::new(),
            ));
        };
        let summary = match store.spend_summary(window).await {
            Ok(summary) => summary,
            Err(error) => {
                tracing::debug!(%error, "request-summary read failed");
                Default::default()
            }
        };
        let rate = match store.get_total_rate().await {
            Ok(rate) => rate,
            Err(error) => {
                tracing::debug!(%error, "request-rate read failed");
                Default::default()
            }
        };
        let rows = match store.recent_requests(window, limit, None).await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::debug!(%error, "recent-request read failed");
                Vec::new()
            }
        };
        Ok(RequestsReport::new(daemon, window, summary, rate, rows))
    }
}

async fn daemon_view(socket: &std::path::Path) -> Option<DaemonView> {
    let probe = daemon::send_command(socket, &DaemonCommand::Status);
    let Ok(response) = tokio::time::timeout(DAEMON_PROBE_TIMEOUT, probe).await else {
        return None;
    };
    match response {
        Ok(DaemonResponse::Status {
            pid,
            listen,
            models,
            ..
        }) => Some(DaemonView {
            pid,
            listen,
            models,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_store_still_returns_an_empty_snapshot() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let action = RequestsAction::new(
            ConfigSource::Default {
                home: directory.path().to_path_buf(),
            },
            directory.path().join("missing.sock"),
        );
        let report = action.report(10).await?;
        assert!(report.rows.is_empty());
        assert!(report.daemon.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn row_limit_is_bounded() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let action = RequestsAction::new(
            ConfigSource::Default {
                home: directory.path().to_path_buf(),
            },
            directory.path().join("missing.sock"),
        );
        assert!(action.report(0).await.is_err());
        assert!(action.report(MAX_REQUEST_ROWS + 1).await.is_err());
        Ok(())
    }
}
