//! Request inspection queries and wire reports, without storage or presentation.

use serde::{Deserialize, Serialize};

/// Maximum number of rows any interactive/read API returns.
pub const MAX_REQUEST_ROWS: u64 = 500;

/// What the report is looking at.
///
/// Derived from the data rather than stored, so it can never disagree with the
/// rows beside it: an empty list because nothing ran and an empty list because
/// the daemon is gone are different facts and must read differently.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// A daemon is answering, and history is readable.
    Live,
    /// No daemon, but the store has rows — history is still worth showing.
    HistoryOnly,
    /// No daemon and nothing recorded (or no store at all).
    Empty,
}

/// The running daemon, as its control socket describes it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonView {
    /// Process id.
    pub pid: u32,
    /// HTTP listen address.
    pub listen: String,
    /// Count of routable models.
    pub models: usize,
}

/// One settled request, in this report's own vocabulary.
///
/// Not `RequestRow` itself: that type is the metering store's display read,
/// and serializing it here would make its field names a public JSON contract
/// that could not then be changed without a break.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestView {
    /// Request id — also the join key into the trajectory store.
    pub request_id: String,
    /// RFC3339 settle timestamp.
    pub created_at: String,
    /// Named router selected during Stage 0, or `null` for direct and legacy rows.
    #[serde(default)]
    pub router_id: Option<String>,
    /// Versioned digest of the non-secret router binding, or `null` when unknown.
    #[serde(default)]
    pub binding_digest: Option<String>,
    /// Caller selector captured before ingress transforms, or `null` when unknown.
    #[serde(default)]
    pub original_selector: Option<String>,
    /// Model the router resolved to.
    pub model: String,
    /// Provider that actually served the request.
    pub provider: String,
    /// Prompt tokens consumed.
    pub prompt_tokens: i64,
    /// Completion tokens produced.
    pub completion_tokens: i64,
    /// Cache-read prompt tokens.
    pub cache_read_tokens: i64,
    /// Cache-write prompt tokens.
    pub cache_write_tokens: i64,
    /// Estimated charge in micro-USD. Meaningless without `charge_status`:
    /// a request whose pricing evidence was incomplete also stores `0`.
    pub charge_micro_usd: i64,
    /// How `charge_micro_usd` was arrived at — `computed`, `not_charged`,
    /// `unknown`, or `legacy_unknown`. Only the first two are evidence.
    pub charge_status: String,
    /// End-to-end latency in milliseconds.
    pub latency_ms: i64,
    /// Error string when the request failed, else `None`.
    pub error: Option<String>,
    /// The trajectory episode this request belongs to, or `null`.
    ///
    /// This is the thread onward: `bro trajectory inspect <episode_id>`
    /// reads the structural record. `null` is the common case — capture
    /// defaults to off — and it means "there is nothing further to read",
    /// which is exactly what a caller needs to know before trying.
    pub episode_id: Option<String>,
}

/// The server-resolved request selection used to produce this report.
///
/// The input accepts optional timestamps so an ordinary request can mean
/// "today". A report must not leave that relative choice implicit: it returns
/// the absolute interval the host actually read.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestFilterView {
    /// Inclusive RFC3339 lower bound chosen by the server.
    pub since: String,
    /// Exclusive RFC3339 upper bound chosen by the server.
    pub until: String,
    /// Resolved-model filter, when requested.
    pub model: Option<String>,
    /// Serving-provider filter, when requested.
    pub provider: Option<String>,
}

/// Whether one independently queried metering component was observed.
///
/// Numeric fields stay additive-compatible with the original report. This
/// flag tells a reader whether their zero is an observed zero or a placeholder
/// retained while another component was unavailable.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeteringComponentAvailability {
    /// The store query completed successfully.
    Available,
    /// The store could not be opened or this component's query failed.
    Unavailable,
}

/// Availability is per component because summary, rate, and rows are separate
/// reads. A successful component remains useful when a sibling fails.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeteringAvailability {
    /// Aggregate spend and request count over the selected filters.
    pub summary: MeteringComponentAvailability,
    /// Host-wide trailing-minute rate.
    pub rate: MeteringComponentAvailability,
    /// The newest-first request page and its truncation flag.
    pub rows: MeteringComponentAvailability,
}

/// Result of `bro status --requests`.
///
/// Deliberately not `Default`: `scope` would come back `""`, a report
/// claiming no scope at all, which is the one thing this surface must never
/// emit. The constructors keep that invariant centralized.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestsReport {
    /// Which of the three states this report represents.
    pub mode: Mode,
    /// `None` when nothing is listening on the control socket.
    pub daemon: Option<DaemonView>,
    /// The window the rollup and rows cover, as a label.
    pub window: String,
    /// Whose spend the rollup describes. Always every caller — stated rather
    /// than left to be guessed, which is the rule `tui <agent>`'s cost line keeps.
    pub scope: String,
    /// Total estimated spend over the window, in micro-USD, counting only
    /// requests that carry charge evidence.
    ///
    /// `null` when **no** request in the window has any — a zero there would
    /// claim a free window that was merely unmeasured, and an agent reading
    /// `spend_micro_usd` without checking `unpriced_requests` would believe
    /// it. `null` forces the question.
    pub spend_micro_usd: Option<u64>,
    /// Requests observed over the window, success and failure alike.
    pub requests: u64,
    /// How many of `requests` have no charge evidence and are therefore
    /// absent from `spend_micro_usd`.
    pub unpriced_requests: u64,
    /// Requests observed in the trailing minute.
    pub requests_per_minute: f64,
    /// Tokens observed in the trailing minute.
    pub tokens_per_minute: f64,
    /// Newest-first settled requests.
    pub rows: Vec<RequestView>,
    /// The absolute time bounds and optional filters the host applied.
    pub filters: RequestFilterView,
    /// Whether another matching row existed after the returned page.
    pub truncated: bool,
    /// Which independently queried metering components were observed.
    pub metering: MeteringAvailability,
    /// Scope of the rate fields, which are intentionally not narrowed by the
    /// report's time/model/provider filters.
    pub rate_scope: String,
}

/// Input accepted by local and remote request-inspection surfaces.
///
/// Leaving both time fields unset selects today from the server's UTC clock.
/// Supplying either one requires the other, so the database always receives an
/// explicit half-open interval rather than a client-local relative time.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestFilters {
    /// Maximum rows to return. The action validates `1..=500`.
    #[serde(default = "default_request_limit")]
    pub limit: u64,
    /// Inclusive RFC3339 time bound, paired with `until`.
    pub since: Option<String>,
    /// Exclusive RFC3339 time bound, paired with `since`.
    pub until: Option<String>,
    /// Resolved model identifier to inspect.
    pub model: Option<String>,
    /// Serving provider identifier to inspect.
    pub provider: Option<String>,
}

impl RequestFilterView {
    /// Construct the public view from already validated, absolute values.
    pub fn new(
        since: String,
        until: String,
        model: Option<String>,
        provider: Option<String>,
    ) -> Self {
        Self {
            since,
            until,
            model,
            provider,
        }
    }
}

impl MeteringAvailability {
    /// A fully observed snapshot.
    pub const fn available() -> Self {
        Self {
            summary: MeteringComponentAvailability::Available,
            rate: MeteringComponentAvailability::Available,
            rows: MeteringComponentAvailability::Available,
        }
    }

    /// No metering data was available, such as when the database cannot open.
    pub const fn unavailable() -> Self {
        Self {
            summary: MeteringComponentAvailability::Unavailable,
            rate: MeteringComponentAvailability::Unavailable,
            rows: MeteringComponentAvailability::Unavailable,
        }
    }
}

impl Default for RequestFilters {
    fn default() -> Self {
        Self {
            limit: default_request_limit(),
            since: None,
            until: None,
            model: None,
            provider: None,
        }
    }
}

impl RequestFilters {
    /// Start from the default server-relative filter with an explicit page size.
    pub fn with_limit(limit: u64) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }
}

fn default_request_limit() -> u64 {
    MAX_REQUEST_ROWS
}
