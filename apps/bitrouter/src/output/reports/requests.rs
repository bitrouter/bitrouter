//! `bitrouter status --requests` — what the router has actually done.
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
//! surface keep the rule `chat`'s cost line keeps: a currency figure states
//! whose spend it is.

use serde::Serialize;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
#[derive(Debug, Clone, Serialize)]
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
#[derive(Debug, Clone, Serialize)]
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
    pub charge_status: &'static str,
    /// End-to-end latency in milliseconds.
    pub latency_ms: i64,
    /// Error string when the request failed, else `None`.
    pub error: Option<String>,
    /// The trajectory episode this request belongs to, or `null`.
    ///
    /// This is the thread onward: `bitrouter trajectory inspect <episode_id>`
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
            charge_status: row.charge_status.as_str(),
            latency_ms: row.latency_ms,
            error: row.error,
            episode_id: row.episode_id,
        }
    }
}

impl RequestView {
    /// The row as the human table's cells, in [`HEADERS`] order.
    fn cells(&self) -> [String; 8] {
        [
            clock(&self.created_at),
            self.model.clone(),
            self.provider.clone(),
            tokens(self.prompt_tokens),
            tokens(self.completion_tokens),
            charge(self.charge_micro_usd, self.charge_status),
            latency(self.latency_ms),
            status(self.error.as_deref()),
        ]
    }
}

/// Result of `bitrouter status --requests`.
///
/// Deliberately not `Default`: `scope` would come back `""`, a report
/// claiming no scope at all, which is the one thing this surface must never
/// emit. [`RequestsReport::new`] is the only way to build one.
#[derive(Debug, Clone, Serialize)]
pub struct RequestsReport {
    /// Which of the three states this report represents.
    pub mode: Mode,
    /// `None` when nothing is listening on the control socket.
    pub daemon: Option<DaemonView>,
    /// The window the rollup and rows cover, as a label.
    pub window: String,
    /// Whose spend the rollup describes. Always every caller — stated rather
    /// than left to be guessed, which is the rule `chat`'s cost line keeps.
    pub scope: &'static str,
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
}

/// The one scope these figures have ever had.
const SCOPE: &str = "all callers";

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
        let mode = match (daemon.is_some(), rows.is_empty()) {
            (true, _) => Mode::Live,
            (false, false) => Mode::HistoryOnly,
            (false, true) => Mode::Empty,
        };
        Self {
            mode,
            daemon,
            window: window_label(window).to_string(),
            scope: SCOPE,
            // Nothing priced is not the same as nothing spent.
            spend_micro_usd: (summary.unpriced < summary.requests || summary.requests == 0)
                .then_some(summary.spend_micro_usd),
            requests: summary.requests,
            unpriced_requests: summary.unpriced,
            requests_per_minute: rate.requests_per_minute,
            tokens_per_minute: rate.tokens_per_minute,
            rows: rows.into_iter().map(RequestView::from).collect(),
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
            (None, Mode::HistoryOnly) => "history only — daemon not running".to_string(),
            (None, _) => "nothing recorded yet — try bitrouter serve".to_string(),
        }
    }

    /// The rollup, scope included.
    ///
    /// `unreported` rather than `$0.00` when nothing in the window carries
    /// charge evidence. This is the rule `bitrouter chat`'s cost line already
    /// keeps — *a client that cannot see a price has not observed a free
    /// turn* — and the two surfaces contradicted each other until it did.
    fn rollup(&self) -> String {
        let spend = match self.spend_micro_usd {
            Some(micro_usd) => fmt_usd(micro_usd),
            None => "unreported".to_string(),
        };
        let mut line = format!(
            "{} {spend} · {} req · {:.1} req/min · {} tok/min · {}",
            self.window,
            self.requests,
            self.requests_per_minute,
            tokens(self.tokens_per_minute as i64),
            self.scope,
        );
        if self.requests == 0 {
            line.push_str("  ·  no requests in this window");
        }
        line
    }

    /// What the rollup cannot say, said rather than left to be inferred.
    ///
    /// A partial total is worse than a labelled one: the reader has no way to
    /// tell a cheap window from an unmeasured one unless the gap is named.
    fn caveat(&self) -> Option<String> {
        match (self.unpriced_requests, self.spend_micro_usd) {
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
        }
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
            table.push(row.cells());
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

    /// `bitrouter chat` renders an unscoped cost as `unreported`. This surface
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
    fn a_dead_daemon_with_history_is_not_the_same_as_a_fresh_install() {
        let history = report(None, vec![row()]);
        assert_eq!(history.mode, Mode::HistoryOnly);
        assert!(history.headline().contains("history only"));

        let fresh = report(None, Vec::new());
        assert_eq!(fresh.mode, Mode::Empty);
        assert!(
            fresh.headline().contains("bitrouter serve"),
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
