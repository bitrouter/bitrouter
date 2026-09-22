//! The `status` action: *is BitRouter up, and am I OK to spend?*
//!
//! One report type is shared by `bro status`, Code sessions, and typed remote
//! control so all retained surfaces preserve the same JSON shape. The app owns
//! the type, port, and implementation over the daemon control socket and local
//! metering database.

use super::ToolError;

/// Where BitRouter stands on money: what has been spent, and what is left.
///
/// The two halves are **independent facts**, not two views of one, which is
/// why each is separately optional:
///
/// - [`Self::spent`] is money already gone. Every deployment can answer it —
///   BitRouter meters its own requests — so a BYOK install gets a real answer
///   to "am I OK to spend?" instead of nothing.
/// - [`Self::limit`] is money still available before a cap. Only a deployment
///   that *has* a cap can answer it; a BYOK install bills the upstream
///   provider directly and has none.
///
/// A metered cloud account fills `limit` and leaves `spent` empty — the
/// balance endpoint is a ledger of what remains and knows nothing of
/// spend-to-date. A local daemon fills `spent` and leaves `limit` empty.
/// Neither has to lie about the half it cannot see, and an agent reads the
/// fields that are there instead of guessing which deployment it is talking
/// to.
///
/// Named `spend`, not `cost`: `cost` is the prospective per-token rate of a
/// request not yet made (`route_preview`'s `estimated_cost`). This is money
/// already gone, and it matches the vocabulary of the metering store it is
/// read from.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct Spend {
    /// Currency both halves are denominated in (today: `"USD"`). The amounts
    /// are named `*_micro_usd` because that is the unit BitRouter meters and
    /// bills in; a metered account that declares another currency reports it
    /// here rather than having it silently dropped.
    pub currency: String,
    /// What has been spent, where a spend record is readable. `None` means no
    /// metering database was reachable — **not** that nothing was spent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spent: Option<Spent>,
    /// What is left before a cap, on deployments that impose one. `None` means
    /// the deployment caps nothing — **not** that the balance is zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<SpendLimit>,
}

/// Money already spent within a window, as BitRouter's own metering priced it.
///
/// **This is an estimate, and it is a floor.** The figure is priced from
/// BitRouter's registry at settle time, not from a provider invoice, and
/// [`Self::unpriced`] counts the requests inside the same window that carried
/// no charge evidence at all. Those rows are *excluded* rather than summed as
/// zero, because adding them would report a floor as a price. An agent
/// comparing this against a [`SpendLimit`] — which is an authoritative ledger
/// — must read `unpriced` to know how much of the window the figure does not
/// cover.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct Spent {
    /// The window the figure covers, e.g. `"today"` (since 00:00 UTC).
    pub window: String,
    /// Estimated spend over `window`, counting only requests that carry charge
    /// evidence.
    pub estimated_micro_usd: u64,
    /// Requests observed in `window`, successes and failures alike.
    pub requests: u64,
    /// How many of `requests` had no charge evidence and are therefore absent
    /// from `estimated_micro_usd`. Non-zero means the figure understates by an
    /// unknown amount.
    pub unpriced: u64,
}

/// What a capped deployment will still let the caller spend.
///
/// An authoritative ledger, unlike [`Spent`]: these are the numbers the
/// account is actually settled against, not an estimate priced locally.
///
/// Today the only reachable cap is a metered account's prepaid credit balance
/// (`GET /v1/billing/balance`). Locally issued API keys carry a
/// `spend_limit_micro_usd` of their own, which would be a second kind of cap —
/// but `status` reads no per-key state today (see [`StatusQuery`] on
/// attribution), so modelling it here would be a shape nothing fills. That is
/// the extension point when per-caller status arrives.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct SpendLimit {
    /// Raw balance on the credit account, before pending debits.
    pub balance_micro_usd: i64,
    /// Debits recorded but not yet drained from `balance_micro_usd`.
    pub pending_micro_usd: i64,
    /// `max(balance - pending, 0)` — what the next call may actually spend.
    pub remaining_micro_usd: i64,
}

/// What BitRouter reports about itself.
///
/// Every field beyond `running` is optional because the two deployments answer
/// different halves of the question: a local daemon has a pid, a listen address
/// and a control socket; a metered cloud account has a credit balance.
/// `running: false` is an **answer**, never an error — an agent polling for
/// health has to be able to tell "down" from "broken", and the CLI has always
/// exited 0 on a stopped daemon.
///
/// [`Self::spend`] is the exception to that split: both deployments can say
/// something about money, so both fill it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct StatusReport {
    /// Whether BitRouter answered.
    pub running: bool,
    /// The daemon's process id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// The daemon's HTTP listen address, as the daemon itself reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    /// Count of routable models.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<usize>,
    /// The distinct providers behind those models, sorted. Empty when nothing
    /// is running, or when the daemon is too old to report them.
    #[serde(default)]
    pub providers: Vec<String>,
    /// The control socket the report was read from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
    /// The spend position — what has gone, and what is left where a cap
    /// exists. `None` only when neither half could be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend: Option<Spend>,
    /// Router definitions read from the currently saved configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub saved_routers: Option<Vec<crate::actions::models::RouterStatus>>,
    /// Router definitions held by the running daemon. `None` means there is no
    /// daemon or it predates router inventory support.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_routers: Option<Vec<crate::actions::models::RouterStatus>>,
    /// Whether saved public router definitions require a daemon restart to
    /// become active. Unknown unless both views were observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub router_restart_required: Option<bool>,
    /// Redaction-safe state of the daemon's complete primary and auxiliary
    /// configuration sources. `None` means runtime evidence is unavailable; it
    /// never means the saved and running configurations match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_state: Option<crate::reload::ConfigurationState>,
    /// Version of this CLI, only for local status queries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    /// Version reported by the local daemon, if supported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_version: Option<String>,
    /// Safe handoff protocol advertised by the local daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_protocol: Option<u32>,
    /// Build fingerprint used to distinguish same-version development builds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_build_id: Option<String>,
    /// Observed activity; unknown counts remain absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff_activity: Option<crate::daemon::HandoffActivity>,
    /// Whether the detached CLI launcher owns this daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_cli_owned: Option<bool>,
    /// `compatible`, `handoff_required`, `legacy_unknown`,
    /// `externally_managed`, or `daemon_newer`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<String>,
}

impl StatusReport {
    /// Nothing is listening. Not a failure: adapters return a report with
    /// `running: false`, not an error.
    ///
    /// `spend` is still carried, because it is not a liveness fact: the
    /// metering database records what a *past* daemon spent and reads fine
    /// with nothing running.
    pub fn stopped(socket: String, spend: Option<Spend>) -> Self {
        Self {
            running: false,
            pid: None,
            listen: None,
            models: None,
            providers: Vec::new(),
            socket: Some(socket),
            spend,
            saved_routers: None,
            running_routers: None,
            router_restart_required: None,
            config_state: None,
            installed_version: None,
            daemon_version: None,
            handoff_protocol: None,
            handoff_build_id: None,
            handoff_activity: None,
            daemon_cli_owned: None,
            compatibility: None,
        }
    }

    /// A daemon answered its control socket.
    pub fn running(
        pid: u32,
        listen: String,
        models: usize,
        providers: Vec<String>,
        socket: String,
        spend: Option<Spend>,
    ) -> Self {
        Self {
            running: true,
            pid: Some(pid),
            listen: Some(listen),
            models: Some(models),
            providers,
            socket: Some(socket),
            spend,
            saved_routers: None,
            running_routers: None,
            router_restart_required: None,
            config_state: None,
            installed_version: None,
            daemon_version: None,
            handoff_protocol: None,
            handoff_build_id: None,
            handoff_activity: None,
            daemon_cli_owned: None,
            compatibility: None,
        }
    }

    /// A metered account answered. There is no process to report — the
    /// deployment is somebody else's — so the question collapses to
    /// "reachable, and this much room left to spend".
    pub fn metered(spend: Spend) -> Self {
        Self {
            running: true,
            pid: None,
            listen: None,
            models: None,
            providers: Vec::new(),
            socket: None,
            spend: Some(spend),
            saved_routers: None,
            running_routers: None,
            router_restart_required: None,
            config_state: None,
            installed_version: None,
            daemon_version: None,
            handoff_protocol: None,
            handoff_build_id: None,
            handoff_activity: None,
            daemon_cli_owned: None,
            compatibility: None,
        }
    }

    pub fn with_router_views(
        mut self,
        saved: Option<Vec<crate::actions::models::RouterStatus>>,
        running: Option<Vec<crate::actions::models::RouterStatus>>,
        restart_required: Option<bool>,
    ) -> Self {
        self.router_restart_required = restart_required;
        self.saved_routers = saved;
        self.running_routers = running;
        self
    }

    pub fn with_config_state(mut self, state: Option<crate::reload::ConfigurationState>) -> Self {
        self.config_state = state;
        self
    }

    pub fn with_local_versions(
        mut self,
        daemon_version: Option<String>,
        protocol: Option<u32>,
        build_id: Option<String>,
        activity: Option<crate::daemon::HandoffActivity>,
        cli_owned: Option<bool>,
    ) -> Self {
        self.installed_version = Some(crate::VERSION.to_string());
        self.compatibility = if !self.running {
            None
        } else if protocol != Some(1) || daemon_version.is_none() || build_id.is_none() {
            Some("legacy_unknown".to_string())
        } else if daemon_version.as_deref() == Some(crate::VERSION)
            && build_id.as_deref() == Some(crate::HANDOFF_BUILD_ID)
        {
            Some("compatible".to_string())
        } else if cli_owned != Some(true) {
            Some("externally_managed".to_string())
        } else {
            let candidate = semver::Version::parse(crate::VERSION);
            let resident = daemon_version.as_deref().map(semver::Version::parse);
            Some(
                match (candidate, resident) {
                    (Ok(candidate), Some(Ok(resident))) if candidate >= resident => {
                        "handoff_required"
                    }
                    _ => "daemon_newer",
                }
                .to_string(),
            )
        };
        self.daemon_version = daemon_version;
        self.handoff_protocol = protocol;
        self.handoff_build_id = build_id;
        self.handoff_activity = activity;
        self.daemon_cli_owned = cli_owned;
        self
    }
}

/// The `status` port shared by local consumers.
#[async_trait::async_trait]
pub trait StatusQuery: Send + Sync {
    /// Report BitRouter's state, or a `ToolError` when the probe itself failed
    /// (a permission-denied socket, a malformed response). A stopped daemon is
    /// `Ok` with `running: false`.
    async fn status(&self) -> Result<StatusReport, ToolError>;
}

use std::path::{Path, PathBuf};

use crate::daemon::{self, DaemonCommand, DaemonResponse};
use crate::metering::store::TimeWindow;
use crate::paths::ConfigSource;

/// The window `status` reports spend over, and the label it carries in the
/// report. `bro status --requests` rolls up the same day, so the
/// agent-facing spend surfaces agree.
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
    /// see `local_spend`.
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
            daemon_version,
            handoff_protocol,
            handoff_build_id,
            handoff_activity,
            cli_owned,
            listen,
            models,
            providers,
            saved_routers,
            running_routers,
            router_restart_required,
            config_state,
        }) => Ok(StatusReport::running(
            pid,
            listen,
            models,
            providers,
            socket.display().to_string(),
            spend,
        )
        .with_router_views(saved_routers, running_routers, router_restart_required)
        .with_config_state(config_state)
        .with_local_versions(
            daemon_version,
            handoff_protocol,
            handoff_build_id,
            handoff_activity,
            cli_owned,
        )),
        Ok(DaemonResponse::Error { message }) => Err(anyhow::anyhow!(message)),
        Ok(other) => Err(anyhow::anyhow!("unexpected response: {other:?}")),
        // No daemon listening on the socket → report stopped, not error. The
        // spend half still rides along: what a past daemon spent is recorded
        // on disk and does not stop being true when it exits.
        Err(e) if daemon::is_not_reachable(&e) => {
            let saved_routers = match source {
                Some(source) => crate::actions::models::disk_router_statuses(source).await,
                None => None,
            };
            let config_state = match source {
                Some(source) => {
                    Some(crate::reload::configuration_state_without_runtime(source).await)
                }
                None => None,
            };
            Ok(StatusReport::stopped(socket.display().to_string(), spend)
                .with_router_views(saved_routers, None, None)
                .with_config_state(config_state)
                .with_local_versions(None, None, None, None, None))
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
/// [`SpendLimit`](crate::actions::status::SpendLimit) to report; the
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
/// retained surfaces reaching this code are single-tenant by construction.
/// Any future multi-tenant adapter must resolve the caller to an API-key id and
/// call a scoped query instead; the store already has the required primitives.
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
    /// `local_spend`). The parameter stays on the port because the cloud
    /// implementation of the same action does forward it, and because per-
    /// caller local attribution is where this implementation goes next.
    async fn status(&self) -> Result<StatusReport, ToolError> {
        self.report()
            .await
            .map_err(|e| ToolError::new(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_status_keeps_legacy_and_external_ownership_unknown_or_explicit() {
        let base = || {
            StatusReport::running(
                7,
                "127.0.0.1:4356".into(),
                0,
                Vec::new(),
                "/tmp/bitrouter.sock".into(),
                None,
            )
        };
        let legacy = base().with_local_versions(None, None, None, None, None);
        assert_eq!(legacy.compatibility.as_deref(), Some("legacy_unknown"));
        let matching = base().with_local_versions(
            Some(crate::VERSION.into()),
            Some(1),
            Some(crate::HANDOFF_BUILD_ID.into()),
            None,
            Some(true),
        );
        assert_eq!(matching.compatibility.as_deref(), Some("compatible"));
        let older = base().with_local_versions(
            Some("1.0.0-alpha.1".into()),
            Some(1),
            Some("another-build".into()),
            None,
            Some(true),
        );
        assert_eq!(older.compatibility.as_deref(), Some("handoff_required"));
        let external = base().with_local_versions(
            Some("1.0.0-alpha.1".into()),
            Some(1),
            Some("another-build".into()),
            None,
            Some(false),
        );
        assert_eq!(
            external.compatibility.as_deref(),
            Some("externally_managed")
        );
    }

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

    /// Direct calls and the injected port both fill `spend` from the same
    /// action. Same struct, same numbers.
    #[tokio::test]
    async fn both_surfaces_report_spend_on_a_local_deployment() {
        let (dir, source) = metered_home().await;
        let socket = dir.path().join("nothing-listening.sock");
        let probe = DaemonStatus::new(&socket, Some(source));

        let cli = probe.report().await.expect("cli surface");
        let port = StatusQuery::status(&probe).await.expect("port surface");

        for (surface, report) in [("cli", &cli), ("port", &port)] {
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
            serde_json::to_value(&port).expect("port json"),
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
