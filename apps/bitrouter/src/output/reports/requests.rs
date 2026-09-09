//! `bro status --requests` — what the router has actually done.
//!
//! # Why this is a report and not a printer
//!
//! It used to be neither. A ratatui table drew these rows; `#830` deleted the
//! widget and promoted its string layer to the product, which left one command
//! that returned a `String` and `print!`ed it — bypassing
//! [`crate::output::Output`] entirely.
//! Two things followed, and both were invisible from the call site:
//!
//! - **`--json` was silently ignored.** Every other command answers the global
//!   flags; this one could not, so the router's own request history was the
//!   one thing an agent could not read as JSON.
//! - **The table was rendered twice over.** Column sizing, the
//!   never-pad-the-last-column rule, and the `●`/`○` state glyph all exist in
//!   [`Human`] already; the printer reimplemented each, untested against the
//!   originals and with no theme, so it was also the only human-facing table
//!   in the binary that ignored `NO_COLOR`.
//!
//! As a [`CliReport`] both fall out: [`Human::table`] sizes the columns and
//! [`Human::status_block`] draws the glyph, and the JSON view is the derive.
//!
//! # What survives from the printer
//!
//! The cell decisions, which are domain judgment rather than formatting: a
//! request that was never charged renders `—` and not `$0.00`, because a
//! computed zero and nothing-to-bill are different claims; an upstream error
//! is flattened and truncated so one pathological message cannot push every
//! other column off the line; token counts round to `12.4k`, which reads at a
//! glance where `12431` does not.
//!
//! # The scope the rollup can finally state
//!
//! [`RequestRow`] is a display read and deliberately not the export artifact,
//! so this module declares its own [`RequestView`] rather than serializing it —
//! otherwise the store's field names would become a JSON contract by accident.
//!
//! The spend figures cover **every caller**, and now say so. They always did,
//! but the poll once had a launch-scoped branch it fell back out of without
//! recording which one ran, so the figure could not be labelled at all. That
//! branch had no reachable caller and is gone with it, which is what lets this
//! surface keep the rule `tui <agent>`'s cost line keeps: a currency figure states
//! whose spend it is.

use chrono::Datelike;
use serde::{Deserialize, Serialize};

use crate::metering::fmt_usd;
use crate::metering::pricing::ChargeStatus;
use crate::metering::store::{RateMetrics, RequestRow, SpendSummary, TimeWindow};
use crate::output::CliReport;
use crate::output::human::{Health, Human, Table};

/// Column headers for the request table, in the order [`RequestView::cells`]
/// emits.
const HEADERS: [&str; 8] = [
    "time", "model", "provider", "in", "out", "cost", "latency", "status",
];

/// What the report is looking at.
///
/// Derived from the data rather than stored, so it can never disagree with the
/// rows beside it: an empty list because nothing ran and an empty list because
/// the daemon is gone are different facts and must read differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
/// Not [`RequestRow`] itself: that type is the metering store's display read,
/// and serializing it here would make its field names a public JSON contract
/// that could not then be changed without a break.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RequestView {
    /// Request id — also the join key into the trajectory store.
    pub request_id: String,
    /// RFC3339 settle timestamp.
    pub created_at: String,
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

impl From<RequestRow> for RequestView {
    fn from(row: RequestRow) -> Self {
        Self {
            request_id: row.request_id,
            created_at: row.created_at,
            model: row.model_id,
            provider: row.provider_id,
            prompt_tokens: row.prompt_tokens,
            completion_tokens: row.completion_tokens,
            cache_read_tokens: row.cache_read_tokens,
            cache_write_tokens: row.cache_write_tokens,
            charge_micro_usd: row.estimated_charge_micro_usd,
            charge_status: row.charge_status.as_str().to_string(),
            latency_ms: row.latency_ms,
            error: row.error,
            episode_id: row.episode_id,
        }
    }
}

impl RequestView {
    /// The row as the human table's cells, in `HEADERS` order.
    pub fn display_cells(&self) -> [String; 8] {
        [
            clock(&self.created_at),
            self.model.clone(),
            self.provider.clone(),
            tokens(self.prompt_tokens),
            tokens(self.completion_tokens),
            charge(self.charge_micro_usd, &self.charge_status),
            latency(self.latency_ms),
            status(self.error.as_deref()),
        ]
    }

    #[cfg(test)]
    fn cells(&self) -> [String; 8] {
        self.display_cells()
    }
}

/// The server-resolved request selection used to produce this report.
///
/// The input accepts optional timestamps so an ordinary request can mean
/// "today". A report must not leave that relative choice implicit: it returns
/// the absolute interval the host actually read.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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

/// Whether one independently queried metering component was observed.
///
/// Numeric fields stay additive-compatible with the original report. This
/// flag tells a reader whether their zero is an observed zero or a placeholder
/// retained while another component was unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MeteringComponentAvailability {
    /// The store query completed successfully.
    Available,
    /// The store could not be opened or this component's query failed.
    Unavailable,
}

/// Availability is per component because summary, rate, and rows are separate
/// reads. A successful component remains useful when a sibling fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct MeteringAvailability {
    /// Aggregate spend and request count over the selected filters.
    pub summary: MeteringComponentAvailability,
    /// Host-wide trailing-minute rate.
    pub rate: MeteringComponentAvailability,
    /// The newest-first request page and its truncation flag.
    pub rows: MeteringComponentAvailability,
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

/// Inputs assembled by the action after the independently queried metering
/// components have settled. This stays separate from the serialized report so
/// a store row does not accidentally become a public JSON contract.
#[derive(Debug, Clone)]
pub struct RequestReportData {
    pub window: String,
    pub filters: RequestFilterView,
    pub summary: SpendSummary,
    pub rate: RateMetrics,
    pub rows: Vec<RequestRow>,
    pub metering: MeteringAvailability,
    pub truncated: bool,
}

/// Result of `bro status --requests`.
///
/// Deliberately not `Default`: `scope` would come back `""`, a report
/// claiming no scope at all, which is the one thing this surface must never
/// emit. The constructors keep that invariant centralized.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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

/// The one scope these figures have ever had.
const SCOPE: &str = "all callers";
const RATE_SCOPE: &str = "all callers, trailing minute";

impl RequestsReport {
    /// Assemble from one poll of the store and the control socket.
    ///
    /// `mode` is computed here from the two, so no caller can set it to
    /// something the data does not support.
    pub fn new(
        daemon: Option<DaemonView>,
        window: TimeWindow,
        summary: SpendSummary,
        rate: RateMetrics,
        rows: Vec<RequestRow>,
    ) -> Self {
        let now = chrono::Utc::now();
        let until = match window {
            TimeWindow::Custom { end, .. } => end,
            _ => now,
        };
        let filters = RequestFilterView::new(
            window_start(window, now).to_rfc3339(),
            until.to_rfc3339(),
            None,
            None,
        );
        Self::new_filtered(
            daemon,
            RequestReportData {
                window: window_label(window).to_string(),
                filters,
                summary,
                rate,
                rows,
                metering: MeteringAvailability::available(),
                truncated: false,
            },
        )
    }

    /// Assemble a report for one validated, potentially filtered page.
    pub fn new_filtered(daemon: Option<DaemonView>, data: RequestReportData) -> Self {
        let mode = match (daemon.is_some(), data.rows.is_empty()) {
            (true, _) => Mode::Live,
            (false, false) => Mode::HistoryOnly,
            (false, true) => Mode::Empty,
        };
        Self {
            mode,
            daemon,
            window: data.window,
            scope: SCOPE.to_string(),
            // Nothing priced is not the same as nothing spent.
            spend_micro_usd: (data.metering.summary == MeteringComponentAvailability::Available
                && (data.summary.unpriced < data.summary.requests || data.summary.requests == 0))
                .then_some(data.summary.spend_micro_usd),
            requests: data.summary.requests,
            unpriced_requests: data.summary.unpriced,
            requests_per_minute: data.rate.requests_per_minute,
            tokens_per_minute: data.rate.tokens_per_minute,
            rows: data.rows.into_iter().map(RequestView::from).collect(),
            filters: data.filters,
            truncated: data.truncated,
            metering: data.metering,
            rate_scope: RATE_SCOPE.to_string(),
        }
    }

    /// Replace raw upstream errors with bounded, categorized remote-safe text.
    ///
    /// Local reports retain their stored diagnostic because the host owner may
    /// use it to investigate a failed request. Remote reports must never carry
    /// a provider URL, bearer token, prompt fragment, or upstream response.
    pub fn sanitize_for_remote(&mut self) {
        for row in &mut self.rows {
            if let Some(error) = row.error.as_deref() {
                row.error = Some(remote_error_summary(error).to_string());
            }
        }
    }

    /// The glyph the state line carries.
    fn health(&self) -> Health {
        match self.mode {
            Mode::Live => Health::Up,
            Mode::HistoryOnly | Mode::Empty => Health::Down,
        }
    }

    /// The state, stated rather than implied. An empty list must never be left
    /// to look like "no traffic" when the real answer is "nothing is running".
    fn headline(&self) -> String {
        match (&self.daemon, self.mode) {
            (Some(d), _) => format!("live · pid {} · {} · {} models", d.pid, d.listen, d.models),
            (None, _) if self.metering.rows == MeteringComponentAvailability::Unavailable => {
                "metering unavailable — daemon not running".to_string()
            }
            (None, Mode::HistoryOnly) => "history only — daemon not running".to_string(),
            (None, _) => format!(
                "nothing recorded yet — try {} serve",
                bitrouter_sdk::invocation::name()
            ),
        }
    }

    /// The rollup, scope included.
    ///
    /// `unreported` rather than `$0.00` when nothing in the window carries
    /// charge evidence. This is the rule `bro chat`'s cost line already
    /// keeps — *a client that cannot see a price has not observed a free
    /// turn* — and the two surfaces contradicted each other until it did.
    fn rollup(&self) -> String {
        let spend = match self.spend_micro_usd {
            Some(micro_usd) => fmt_usd(micro_usd),
            None => "unreported".to_string(),
        };
        let summary = match self.metering.summary {
            MeteringComponentAvailability::Available => format!("{} req", self.requests),
            MeteringComponentAvailability::Unavailable => "summary unavailable".to_string(),
        };
        let rate = match self.metering.rate {
            MeteringComponentAvailability::Available => format!(
                "{:.1} req/min · {} tok/min ({})",
                self.requests_per_minute,
                tokens(self.tokens_per_minute as i64),
                self.rate_scope,
            ),
            MeteringComponentAvailability::Unavailable => "rate unavailable".to_string(),
        };
        let mut line = format!(
            "{} {spend} · {summary} · {rate} · {}",
            self.window, self.scope,
        );
        if self.metering.summary == MeteringComponentAvailability::Available && self.requests == 0 {
            line.push_str("  ·  no requests in this window");
        }
        line
    }

    /// What the rollup cannot say, said rather than left to be inferred.
    ///
    /// A partial total is worse than a labelled one: the reader has no way to
    /// tell a cheap window from an unmeasured one unless the gap is named.
    fn caveat(&self) -> Option<String> {
        let mut caveats = Vec::new();
        if self.metering.summary == MeteringComponentAvailability::Unavailable {
            caveats.push("metering summary unavailable".to_string());
        }
        if self.metering.rate == MeteringComponentAvailability::Unavailable {
            caveats.push("metering rate unavailable".to_string());
        }
        if self.metering.rows == MeteringComponentAvailability::Unavailable {
            caveats.push("metering request rows unavailable".to_string());
        }
        if self.metering.summary == MeteringComponentAvailability::Available {
            let pricing = match (self.unpriced_requests, self.spend_micro_usd) {
                (0, _) => None,
                (n, None) => Some(format!(
                    "no charge evidence for any of these {n} requests — the daemon \
                     recorded them but could not price them"
                )),
                (n, Some(_)) => Some(format!(
                    "{n} of {} requests have no charge evidence; the total above is \
                     a floor, not a price",
                    self.requests
                )),
            };
            if let Some(pricing) = pricing {
                caveats.push(pricing);
            }
        }
        if caveats.is_empty() {
            None
        } else {
            Some(caveats.join("; "))
        }
    }
}

fn remote_error_summary(error: &str) -> &'static str {
    let category = error
        .chars()
        .take(256)
        .collect::<String>()
        .to_ascii_lowercase();
    if category.contains("rate limit") || category.contains("rate_limited") {
        "upstream rate limited"
    } else if category.contains("timeout") || category.contains("timed out") {
        "upstream timed out"
    } else if category.contains("policy") || category.contains("content filter") {
        "upstream rejected request"
    } else {
        "upstream request failed"
    }
}

impl CliReport for RequestsReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.status_block(self.health(), &self.headline())?;
        h.line(&self.rollup())?;
        if let Some(caveat) = self.caveat() {
            h.note(&caveat)?;
        }
        if self.rows.is_empty() {
            // An empty table header is noise, not information.
            return Ok(());
        }
        h.blank()?;
        let mut table = Table::new(HEADERS);
        for row in &self.rows {
            table.push(row.display_cells());
        }
        h.table(&table)
    }
}

/// The window as the rollup names it.
fn window_label(window: TimeWindow) -> &'static str {
    match window {
        TimeWindow::LastMinute => "last minute",
        TimeWindow::LastHour => "last hour",
        TimeWindow::Today => "today",
        TimeWindow::ThisWeek => "this week",
        TimeWindow::ThisMonth => "this month",
        TimeWindow::Custom { .. } => "window",
    }
}

/// The legacy constructor only receives a named window. Its report still
/// returns absolute bounds, although filtered callers use their already
/// resolved bounds through [`RequestsReport::new_filtered`].
fn window_start(
    window: TimeWindow,
    now: chrono::DateTime<chrono::Utc>,
) -> chrono::DateTime<chrono::Utc> {
    let midnight = now.date_naive().and_time(chrono::NaiveTime::MIN).and_utc();
    match window {
        TimeWindow::LastMinute => now - chrono::Duration::minutes(1),
        TimeWindow::LastHour => now - chrono::Duration::hours(1),
        TimeWindow::Today => midnight,
        TimeWindow::ThisWeek => {
            midnight - chrono::Duration::days(now.weekday().num_days_from_monday().into())
        }
        TimeWindow::ThisMonth => {
            let first = chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1);
            match first {
                Some(first) => first.and_time(chrono::NaiveTime::MIN).and_utc(),
                None => midnight,
            }
        }
        TimeWindow::Custom { start, .. } => start,
    }
}

/// `HH:MM:SS` in local time, or the raw value when it will not parse — a
/// timestamp we cannot read is still more useful shown than blanked.
fn clock(created_at: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(created_at) {
        Ok(t) => t
            .with_timezone(&chrono::Local)
            .format("%H:%M:%S")
            .to_string(),
        Err(_) => created_at.to_string(),
    }
}

/// Compact token counts: `12.4k` reads at a glance where `12431` does not.
fn tokens(n: i64) -> String {
    let n = n.max(0);
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// The charge, and only when it is evidence.
///
/// Three outcomes, because there are three facts to tell apart:
///
/// - a priced request shows its price;
/// - a request an authoritative receipt says was **not** charged shows `—`,
///   which reads as "nothing to bill";
/// - a request whose pricing evidence was incomplete shows `?`, because its
///   stored charge is a placeholder `0`. Rendering that as `—` would claim a
///   free request, and as `$0.00` a measured one. Neither was observed.
fn charge(micro_usd: i64, status: &str) -> String {
    match ChargeStatus::from_persisted(status) {
        ChargeStatus::Computed if micro_usd > 0 => fmt_usd(micro_usd as u64),
        ChargeStatus::Computed | ChargeStatus::NotCharged => "—".to_string(),
        ChargeStatus::Unknown | ChargeStatus::LegacyUnknown => "?".to_string(),
    }
}

fn latency(ms: i64) -> String {
    if ms <= 0 {
        "—".to_string()
    } else if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// How many characters of an upstream error the status column shows.
const ERROR_CHARS: usize = 24;

/// `ok`, or the error — flattened and truncated, because one pathological
/// upstream message must not push every other column off the line.
fn status(error: Option<&str>) -> String {
    match error {
        None => "ok".to_string(),
        Some(e) => {
            let one_line = e.replace('\n', " ");
            let mut short: String = one_line.chars().take(ERROR_CHARS).collect();
            if one_line.chars().count() > ERROR_CHARS {
                short.push('…');
            }
            short
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{Format, Output};

    fn row() -> RequestRow {
        RequestRow {
            request_id: "r1".into(),
            created_at: "2026-08-10T12:00:00Z".into(),
            model_id: "gpt-5".into(),
            provider_id: "openai".into(),
            prompt_tokens: 12_431,
            completion_tokens: 891,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            estimated_charge_micro_usd: 42_000,
            latency_ms: 1_800,
            error: None,
            charge_status: ChargeStatus::Computed,
            episode_id: None,
        }
    }

    /// A row the daemon recorded but could not price — the overwhelmingly
    /// common shape in a real BYOK store.
    fn unpriced() -> RequestRow {
        RequestRow {
            estimated_charge_micro_usd: 0,
            charge_status: ChargeStatus::LegacyUnknown,
            ..row()
        }
    }

    fn daemon() -> DaemonView {
        DaemonView {
            pid: 4412,
            listen: "127.0.0.1:4356".to_string(),
            models: 47,
        }
    }

    fn report(daemon: Option<DaemonView>, rows: Vec<RequestRow>) -> RequestsReport {
        RequestsReport::new(
            daemon,
            TimeWindow::Today,
            SpendSummary::default(),
            RateMetrics::default(),
            rows,
        )
    }

    /// A report over `priced` evidenced requests and `unpriced` unevidenced
    /// ones, as the store's `summarize` would produce.
    fn spend(priced: u64, unpriced: u64, micro_usd: u64) -> RequestsReport {
        RequestsReport::new(
            None,
            TimeWindow::Today,
            SpendSummary {
                spend_micro_usd: micro_usd,
                requests: priced + unpriced,
                unpriced,
            },
            RateMetrics::default(),
            Vec::new(),
        )
    }

    fn human(report: &RequestsReport) -> String {
        String::from_utf8(Output::new(Format::Human).render_to_vec(report))
            .unwrap_or_else(|e| format!("not utf-8: {e}"))
    }

    fn json(report: &RequestsReport) -> serde_json::Value {
        let bytes = Output::new(Format::Json).render_to_vec(report);
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    #[test]
    fn a_row_renders_every_column_the_table_promises() {
        let cells = RequestView::from(row()).cells();
        assert_eq!(cells[1], "gpt-5");
        assert_eq!(cells[2], "openai");
        assert_eq!(cells[3], "12.4k");
        assert_eq!(cells[4], "891");
        assert_eq!(cells[5], "$0.04");
        assert_eq!(cells[6], "1.8s");
        assert_eq!(cells[7], "ok");
    }

    #[test]
    fn an_uncharged_request_shows_a_dash_not_a_zero() {
        // `$0.00` claims a computed zero cost; `—` says there is nothing to
        // report, which is what an unpriced or failed request means.
        let mut r = row();
        r.estimated_charge_micro_usd = 0;
        r.latency_ms = 0;
        r.charge_status = ChargeStatus::NotCharged;
        let cells = RequestView::from(r).cells();
        assert_eq!(cells[5], "—");
        assert_eq!(cells[6], "—");
    }

    /// The bug this pair exists to prevent: an unpriced request is neither a
    /// free one nor a measured zero. `—` would claim the first and `$0.00`
    /// the second; only `?` claims neither.
    #[test]
    fn an_unpriced_request_is_not_rendered_as_free() {
        let cells = RequestView::from(unpriced()).cells();
        assert_eq!(cells[5], "?", "unknown evidence must not read as a price");
        assert_ne!(cells[5], "—");
        assert_ne!(cells[5], "$0.00");
    }

    /// `bro chat` renders an unscoped cost as `unreported`. This surface
    /// showed `$0.00` for the same condition until the two were reconciled.
    #[test]
    fn a_window_with_no_charge_evidence_reports_unreported() {
        let r = spend(0, 264, 0);
        assert!(r.rollup().contains("unreported"), "{}", r.rollup());
        assert!(!r.rollup().contains("$0.00"), "{}", r.rollup());
        assert_eq!(
            json(&r)["spend_micro_usd"],
            serde_json::Value::Null,
            "a zero here would be believed by an agent that did not check"
        );
        assert_eq!(json(&r)["unpriced_requests"], 264);
    }

    /// A partial total must say it is partial, or a cheap window and an
    /// unmeasured one look identical.
    #[test]
    fn a_partial_total_says_it_is_a_floor() {
        let r = spend(3, 2, 110_450);
        assert!(r.rollup().contains("$0.11"), "{}", r.rollup());
        let caveat = r.caveat().unwrap_or_default();
        assert!(caveat.contains("2 of 5"), "{caveat}");
        assert!(caveat.contains("floor"), "{caveat}");
        assert!(
            human(&r).contains("floor"),
            "the caveat must reach the page"
        );
    }

    /// A fully evidenced window carries no caveat — the note is information,
    /// not decoration.
    #[test]
    fn a_fully_priced_window_has_no_caveat() {
        assert!(spend(5, 0, 110_450).caveat().is_none());
    }

    /// The thread onward. `null` is the common case and must stay legible as
    /// "nothing further to read" rather than being omitted.
    #[test]
    fn a_row_carries_its_episode_id_when_one_exists() {
        let mut r = row();
        r.episode_id = Some("ep_7f3a".into());
        let value = json(&report(None, vec![r]));
        assert_eq!(value["rows"][0]["episode_id"], "ep_7f3a");

        let without = json(&report(None, vec![row()]));
        assert_eq!(without["rows"][0]["episode_id"], serde_json::Value::Null);
    }

    /// An agent must be able to tell measured from unmeasured per row, not
    /// only in aggregate.
    #[test]
    fn each_row_carries_its_charge_evidence() {
        let value = json(&report(None, vec![row(), unpriced()]));
        assert_eq!(value["rows"][0]["charge_status"], "computed");
        assert_eq!(value["rows"][1]["charge_status"], "legacy_unknown");
    }

    #[test]
    fn a_long_upstream_error_cannot_push_other_columns_off_screen() {
        let mut r = row();
        r.error =
            Some("upstream refused the request\nwith a very long multi-line explanation".into());
        let cells = RequestView::from(r).cells();
        assert!(!cells[7].contains('\n'), "newlines would break the row");
        assert!(cells[7].chars().count() <= ERROR_CHARS + 1, "{}", cells[7]);
    }

    #[test]
    fn remote_reports_replace_raw_upstream_errors_with_safe_categories() {
        let secret = "https://provider.example/v1?token=brk_do-not-disclose";
        let mut failed = row();
        failed.error = Some(format!("request timed out at {secret}"));
        let mut report = report(None, vec![failed]);
        assert!(
            report.rows[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains(secret))
        );

        report.sanitize_for_remote();

        assert_eq!(report.rows[0].error.as_deref(), Some("upstream timed out"));
        assert!(!json(&report).to_string().contains(secret));
    }

    #[test]
    fn unavailable_metering_is_not_rendered_as_observed_zero_usage() {
        let report = RequestsReport::new_filtered(
            None,
            RequestReportData {
                window: "today".to_string(),
                filters: RequestFilterView::new(
                    "2026-09-08T00:00:00+00:00".to_string(),
                    "2026-09-08T10:30:00+00:00".to_string(),
                    None,
                    None,
                ),
                summary: SpendSummary::default(),
                rate: RateMetrics::default(),
                rows: Vec::new(),
                metering: MeteringAvailability::unavailable(),
                truncated: false,
            },
        );
        let value = json(&report);
        assert_eq!(value["spend_micro_usd"], serde_json::Value::Null);
        assert_eq!(value["metering"]["summary"], "unavailable");
        assert_eq!(value["metering"]["rate"], "unavailable");
        assert!(report.rollup().contains("summary unavailable"));
        assert!(report.rollup().contains("rate unavailable"));
        assert!(report.headline().contains("metering unavailable"));
        assert!(
            report
                .caveat()
                .is_some_and(|note| note.contains("request rows unavailable"))
        );
    }

    #[test]
    fn a_dead_daemon_with_history_is_not_the_same_as_a_fresh_install() {
        let history = report(None, vec![row()]);
        assert_eq!(history.mode, Mode::HistoryOnly);
        assert!(history.headline().contains("history only"));

        let fresh = report(None, Vec::new());
        assert_eq!(fresh.mode, Mode::Empty);
        assert!(
            fresh.headline().contains("bro serve"),
            "an empty view must say what to do, not just show nothing"
        );
    }

    #[test]
    fn a_live_daemon_with_no_traffic_still_reads_as_live() {
        // The failure this guards: showing "nothing recorded yet" while a
        // daemon is up and simply idle, which reads as broken.
        let idle = report(Some(daemon()), Vec::new());
        assert_eq!(idle.mode, Mode::Live);
        let line = idle.headline();
        assert!(line.contains("live"), "{line}");
        assert!(line.contains("pid 4412"), "{line}");
        assert!(line.contains("47 models"), "{line}");
    }

    #[test]
    fn an_empty_window_says_so_rather_than_showing_a_bare_zero() {
        let empty = report(None, Vec::new());
        assert!(empty.rollup().contains("no requests"), "{}", empty.rollup());
        // Genuinely empty, not unmeasured: no rows means nothing to price.
        assert!(empty.caveat().is_none());
    }

    /// The honesty rule `chat`'s cost line keeps: a currency figure states
    /// whose spend it is. This surface could not do that while the poll had a
    /// launch-scoped branch it silently fell out of.
    #[test]
    fn the_rollup_states_whose_spend_it_is() {
        let r = report(Some(daemon()), vec![row()]);
        assert!(r.rollup().contains("all callers"), "{}", r.rollup());
        assert_eq!(json(&r)["scope"], "all callers");
    }

    #[test]
    fn the_human_view_is_a_padded_table_with_no_trailing_whitespace() {
        let text = human(&report(None, vec![row()]));
        assert!(text.contains("provider"), "header row present");
        assert!(text.contains("openai"), "data row present");
        for line in text.lines() {
            assert_eq!(line, line.trim_end(), "output must not pad line ends");
        }
    }

    #[test]
    fn the_human_view_still_reports_state_when_there_is_nothing_to_list() {
        let text = human(&report(None, Vec::new()));
        assert!(text.contains("nothing recorded yet"), "{text}");
        assert!(
            !text.contains("provider"),
            "an empty table header is noise, not information"
        );
    }

    /// The defect this whole module exists to fix: `--requests` used to
    /// bypass `Output`, so the router's own history was the one thing an
    /// agent could not read as JSON.
    #[test]
    fn the_json_view_carries_the_rows_an_agent_needs() {
        let value = json(&report(Some(daemon()), vec![row()]));
        assert_eq!(value["mode"], "live");
        assert_eq!(value["daemon"]["pid"], 4412);
        assert_eq!(value["rows"][0]["model"], "gpt-5");
        assert_eq!(value["rows"][0]["provider"], "openai");
        assert_eq!(value["rows"][0]["charge_micro_usd"], 42_000);
        assert_eq!(value["rows"][0]["request_id"], "r1");
    }

    /// An unreadable timestamp is shown rather than blanked: a value we cannot
    /// parse is still evidence.
    #[test]
    fn an_unparseable_timestamp_survives_to_the_cell() {
        let mut r = row();
        r.created_at = "not a timestamp".into();
        assert_eq!(RequestView::from(r).cells()[0], "not a timestamp");
    }
}
