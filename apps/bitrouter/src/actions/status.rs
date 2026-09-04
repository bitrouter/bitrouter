//! The `status` action, implemented over the daemon's control socket plus the
//! local metering database.
//!
//! One implementation, two surfaces: `bitrouter status` calls
//! [`DaemonStatus::report`] directly, and the origin MCP server's `status` tool
//! calls it through the [`StatusQuery`] port. Both get the same
//! [`StatusReport`], so the CLI's `--json` and the tool's structured content
//! cannot drift.
//!
//! Two independent reads make one report. Liveness comes off the control
//! socket; the spend position comes off the metering database, which is
//! readable whether or not anything is listening. Either can be absent without
//! failing the other.

use std::path::{Path, PathBuf};

use bitrouter_mcp::actions::status::{Spend, Spent, StatusQuery, StatusReport};
use bitrouter_mcp::backend::CallerAuth;
use bitrouter_mcp::error::ToolError;

use crate::daemon::{self, DaemonCommand, DaemonResponse};
use crate::metering::store::TimeWindow;
use crate::paths::ConfigSource;

/// The window `status` reports spend over, and the label it carries in the
/// report. `bitrouter status --requests` and the MCP `complete` footer both
/// roll up the same day, so the three agent-facing spend surfaces agree.
const SPEND_WINDOW: TimeWindow = TimeWindow::Today;

/// The `window` label on [`Spent`] — the wire name for [`SPEND_WINDOW`].
const SPEND_WINDOW_LABEL: &str = "today";

/// Reads BitRouter's state off a control socket, and its spend off the local
/// metering database.
pub struct DaemonStatus {
    socket: PathBuf,
    source: Option<ConfigSource>,
}

impl DaemonStatus {
    /// Probe the daemon listening on `socket`, reporting spend from the
    /// metering database `source` resolves to.
    ///
    /// `source` is `Option` because the spend read is best-effort by
    /// construction: a caller that could not resolve a config passes `None`
    /// and gets a report with no `spend`, never a failure.
    pub fn new(socket: impl Into<PathBuf>, source: Option<ConfigSource>) -> Self {
        Self {
            socket: socket.into(),
            source,
        }
    }

    /// Ask the daemon how it is doing, and the metering store what it cost.
    ///
    /// Nothing listening is `running: false`, not an error — the CLI has always
    /// exited 0 on a stopped daemon, and an agent polling for health has to be
    /// able to tell "down" from "broken". Everything else (permission denied, a
    /// malformed response, a daemon answering something other than `Status`) is
    /// a real failure and propagates. The spend read never propagates anything:
    /// see [`local_spend`].
    pub async fn report(&self) -> anyhow::Result<StatusReport> {
        report_over(&self.socket, self.source.as_ref()).await
    }
}

/// The probe itself, split out so it needs no `self` and can be called from the
/// CLI path without constructing a port.
async fn report_over(socket: &Path, source: Option<&ConfigSource>) -> anyhow::Result<StatusReport> {
    let spend = local_spend(source).await;
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
            spend,
        )),
        Ok(DaemonResponse::Error { message }) => Err(anyhow::anyhow!(message)),
        Ok(other) => Err(anyhow::anyhow!("unexpected response: {other:?}")),
        // No daemon listening on the socket → report stopped, not error. The
        // spend half still rides along: what a past daemon spent is recorded
        // on disk and does not stop being true when it exits.
        Err(e) if daemon::is_not_reachable(&e) => {
            Ok(StatusReport::stopped(socket.display().to_string(), spend))
        }
        Err(e) => Err(e),
    }
}

/// This machine's spend position, read from the local metering database.
///
/// **Best-effort, never fatal.** No config, no database file, an unreadable
/// database, or a failing query all yield `None` — `status` must still answer
/// "is BitRouter up" for an install that has never served a request.
///
/// Only the `spent` half is filled. A BYOK deployment pays its providers
/// directly and imposes no cap of its own, so there is no
/// [`SpendLimit`](bitrouter_mcp::actions::status::SpendLimit) to report; the
/// cloud backend fills that half instead.
///
/// The figure is deliberately reported even when the window is empty: `0` over
/// `0` requests means "nothing spent today", which is a different answer from
/// `None`'s "no spend record was readable". `unpriced` rides along untouched —
/// it is what tells the reader the total is a floor rather than a price.
///
/// **Machine-wide, not per-caller.** [`MeteringStore::spend_summary`] rolls up
/// every caller of this daemon, so on a shared machine this reports other
/// callers' spend to whoever asks. That is tolerable today because the only
/// surfaces reaching this code are single-tenant by construction — the CLI, and
/// `mcp serve` over stdio — but it is why the port still takes a
/// [`CallerAuth`]. Closing it means resolving the caller's bearer to an API key
/// id and calling a scoped query instead; the store already has them
/// (`get_spend` per key, `spend_summary_for_launch`, and
/// `spend_summary_for_acp_session`), so this is attribution plumbing, not a new
/// measurement.
///
/// [`MeteringStore::spend_summary`]: crate::metering::MeteringStore::spend_summary
async fn local_spend(source: Option<&ConfigSource>) -> Option<Spend> {
    let store = crate::metering::reader::open_readonly(source?).await?;
    let summary = store.spend_summary(SPEND_WINDOW).await.ok()?;
    Some(Spend {
        currency: "USD".to_string(),
        spent: Some(Spent {
            window: SPEND_WINDOW_LABEL.to_string(),
            estimated_micro_usd: summary.spend_micro_usd,
            requests: summary.requests,
            unpriced: summary.unpriced,
        }),
        limit: None,
    })
}

#[async_trait::async_trait]
impl StatusQuery for DaemonStatus {
    /// `caller` is ignored, and that is a documented limitation rather than an
    /// oversight: the control socket is a single-machine channel that reaches
    /// no upstream, and the local spend rollup is machine-wide (see
    /// [`local_spend`]). The parameter stays on the port because the cloud
    /// implementation of the same action does forward it, and because per-
    /// caller local attribution is where this implementation goes next.
    async fn status(&self, _caller: &CallerAuth) -> Result<StatusReport, ToolError> {
        self.report()
            .await
            .map_err(|e| ToolError::new(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config whose metering database exists on disk, so `open_readonly`
    /// has something to open. No daemon is started: spend is not a liveness
    /// fact, and the point of these tests is that it answers without one.
    async fn metered_home() -> (tempfile::TempDir, ConfigSource) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("meter.db");
        let db = crate::db::connect(&format!("sqlite://{}", db_path.display()))
            .await
            .expect("create metering db");
        crate::db::run_migrations(&db).await.expect("migrate");
        let config = dir.path().join("bitrouter.yaml");
        std::fs::write(
            &config,
            r#"
server:
  listen: "127.0.0.1:0"
  skip_auth: true
database:
  url: "sqlite://meter.db"
providers:
  openai:
    api_base: https://api.openai.com/v1
    api_key: k1
    models: [{ id: gpt-5 }]
"#,
        )
        .expect("write config");
        let source = ConfigSource::File(config);
        (dir, source)
    }

    /// The change's whole point: on a local deployment **both** surfaces fill
    /// `spend`. `bitrouter status` goes through `report()`; the MCP `status`
    /// tool goes through the `StatusQuery` port. Same struct, same numbers.
    #[tokio::test]
    async fn both_surfaces_report_spend_on_a_local_deployment() {
        let (dir, source) = metered_home().await;
        let socket = dir.path().join("nothing-listening.sock");
        let probe = DaemonStatus::new(&socket, Some(source));

        let cli = probe.report().await.expect("cli surface");
        let tool = StatusQuery::status(&probe, &CallerAuth::default())
            .await
            .expect("mcp surface");

        for (surface, report) in [("cli", &cli), ("mcp", &tool)] {
            let spend = report
                .spend
                .as_ref()
                .unwrap_or_else(|| panic!("{surface} surface reported no spend"));
            let spent = spend
                .spent
                .as_ref()
                .unwrap_or_else(|| panic!("{surface} surface reported no spent half"));
            assert_eq!(spent.window, "today");
            // An empty window is `0`, not absence: "nothing spent today" and
            // "no spend record readable" are different answers.
            assert_eq!(spent.estimated_micro_usd, 0);
            assert_eq!(spent.requests, 0);
            assert_eq!(spent.unpriced, 0);
            // A BYOK deployment caps nothing, so the other half stays empty
            // rather than being fabricated as an unlimited allowance.
            assert!(spend.limit.is_none(), "{surface} invented a spend cap");
        }
        assert_eq!(
            serde_json::to_value(&cli).expect("cli json"),
            serde_json::to_value(&tool).expect("mcp json"),
            "the two surfaces of one action must be the same bytes"
        );
        // …and none of that depended on a daemon being up.
        assert!(!cli.running);
    }

    /// Best-effort, never fatal: no config source means no spend, not a failed
    /// `status`. An agent polling for health must still get an answer.
    #[tokio::test]
    async fn an_unreadable_metering_database_costs_spend_not_the_report() {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("nothing-listening.sock");
        let report = DaemonStatus::new(&socket, None)
            .report()
            .await
            .expect("a missing metering database must not fail status");
        assert!(!report.running);
        assert!(report.spend.is_none());
    }
}
