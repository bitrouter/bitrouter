//! The `status` action: *is BitRouter up, and am I OK to spend?*
//!
//! One report type, shared by `bitrouter status` and the MCP `status` tool, so
//! the CLI's `--json` and the tool's structured content are the same bytes.
//! The crate owns the type and the port; the implementation lives app-side
//! (over the daemon's control socket, plus the local metering database) or,
//! for the cloud profile, in
//! [`CloudBackend`](crate::backend::cloud::CloudBackend), which already holds
//! the credential.

use crate::backend::CallerAuth;
use crate::error::ToolError;

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
}

impl StatusReport {
    /// Nothing is listening. Not a failure: the CLI exits 0 and the MCP tool
    /// returns a result, not an error.
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
        }
    }
}

/// The `status` port. `caller` carries the MCP caller's own bearer so a
/// multi-tenant HTTP deployment reports *that* caller's spend position, never
/// the server's.
///
/// **Attribution is not solved for the local implementation.** The local
/// metering store's window rollup is machine-wide, so a local `status` reports
/// every caller's spend to whoever asks and ignores `caller` entirely. Scoped
/// reads do exist in the store (per API key, per launch, per ACP session), so
/// closing this means threading the caller's key id from `CallerAuth` into a
/// scoped query rather than inventing one. Left as-is deliberately: today's
/// only local `status` surface is stdio, which is single-tenant by
/// construction.
#[async_trait::async_trait]
pub trait StatusQuery: Send + Sync {
    /// Report BitRouter's state, or a `ToolError` when the probe itself failed
    /// (a permission-denied socket, a malformed response). A stopped daemon is
    /// `Ok` with `running: false`.
    async fn status(&self, caller: &CallerAuth) -> Result<StatusReport, ToolError>;
}
