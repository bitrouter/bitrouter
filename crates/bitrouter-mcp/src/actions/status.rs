//! The `status` action: *is BitRouter up, and am I OK to spend?*
//!
//! One report type, shared by `bitrouter status` and the MCP `status` tool, so
//! the CLI's `--json` and the tool's structured content are the same bytes.
//! The crate owns the type and the port; the implementation lives app-side
//! (over the daemon's control socket) or, for the cloud profile, in
//! [`CloudBackend`](crate::backend::cloud::CloudBackend), which already holds
//! the credential.

use crate::backend::CallerAuth;
use crate::error::ToolError;

/// The account's credit position on a metered (cloud) deployment.
///
/// Field-for-field `GET /v1/billing/balance`, so an agent asking "am I OK to
/// spend" reads the same numbers the billing surface reports.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct Credits {
    /// Raw balance from the credit account (before pending debits).
    pub balance_micro_usd: i64,
    /// Sum of pending debits not yet drained into the credit account.
    pub pending_debits_micro_usd: i64,
    /// `max(balance - pending, 0)` — what the next inference call will see.
    pub available_micro_usd: i64,
    /// Currency code (today: `"USD"`).
    pub currency: String,
}

/// What BitRouter reports about itself.
///
/// Every field beyond `running` is optional because the two deployments answer
/// different halves of the question: a local daemon has a pid, a listen address
/// and a control socket; a metered cloud account has credits. `running: false`
/// is an **answer**, never an error — an agent polling for health has to be
/// able to tell "down" from "broken", and the CLI has always exited 0 on a
/// stopped daemon.
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
    /// The credit position, on deployments that meter one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<Credits>,
}

impl StatusReport {
    /// Nothing is listening. Not a failure: the CLI exits 0 and the MCP tool
    /// returns a result, not an error.
    pub fn stopped(socket: String) -> Self {
        Self {
            running: false,
            pid: None,
            listen: None,
            models: None,
            providers: Vec::new(),
            socket: Some(socket),
            credits: None,
        }
    }

    /// A daemon answered its control socket.
    pub fn running(
        pid: u32,
        listen: String,
        models: usize,
        providers: Vec<String>,
        socket: String,
    ) -> Self {
        Self {
            running: true,
            pid: Some(pid),
            listen: Some(listen),
            models: Some(models),
            providers,
            socket: Some(socket),
            credits: None,
        }
    }

    /// A metered account answered. There is no process to report — the
    /// deployment is somebody else's — so the question collapses to
    /// "reachable, and this much credit".
    pub fn credited(credits: Credits) -> Self {
        Self {
            running: true,
            pid: None,
            listen: None,
            models: None,
            providers: Vec::new(),
            socket: None,
            credits: Some(credits),
        }
    }
}

/// The `status` port. `caller` carries the MCP caller's own bearer so a
/// multi-tenant HTTP deployment reports *that* caller's credits, never the
/// server's. Implementations that reach no upstream — the local control
/// socket — ignore it.
#[async_trait::async_trait]
pub trait StatusQuery: Send + Sync {
    /// Report BitRouter's state, or a `ToolError` when the probe itself failed
    /// (a permission-denied socket, a malformed response). A stopped daemon is
    /// `Ok` with `running: false`.
    async fn status(&self, caller: &CallerAuth) -> Result<StatusReport, ToolError>;
}
