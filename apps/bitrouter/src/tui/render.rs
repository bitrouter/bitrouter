//! Formatting for `bitrouter status --requests`.
//!
//! Everything that decides *what a row says* lives here as plain string
//! functions, testable without a terminal because there is no terminal
//! involved: [`oneshot`] returns a `String` and the caller prints it. This was
//! already true when a ratatui view sat on top deciding where the strings went;
//! that view is gone and this is the whole surface.

use crate::metering::fmt_usd;
use crate::metering::store::RequestRow;

use super::snapshot::Snapshot;

/// Column headers for the request stream, in the order [`stream_row`] emits.
pub const STREAM_HEADERS: [&str; 8] = [
    "time", "model", "provider", "in", "out", "cost", "latency", "status",
];

/// One stream row's cells.
pub fn stream_row(row: &RequestRow) -> [String; 8] {
    [
        clock(&row.created_at),
        row.model_id.clone(),
        row.provider_id.clone(),
        tokens(row.prompt_tokens),
        tokens(row.completion_tokens),
        charge(row),
        latency(row.latency_ms),
        status(row),
    ]
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

/// A charge, or `—` when the request was not charged. An em dash reads as
/// "nothing to bill"; `$0.00` would read as a computed zero.
fn charge(row: &RequestRow) -> String {
    if row.estimated_charge_micro_usd <= 0 {
        "—".to_string()
    } else {
        fmt_usd(row.estimated_charge_micro_usd as u64)
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

/// `ok`, or the error — truncated, because one pathological upstream message
/// must not push every other column off the screen.
fn status(row: &RequestRow) -> String {
    match &row.error {
        None => "ok".to_string(),
        Some(e) => {
            let one_line = e.replace('\n', " ");
            let mut short: String = one_line.chars().take(24).collect();
            if one_line.chars().count() > 24 {
                short.push('…');
            }
            short
        }
    }
}

/// The footer rollup, scoped daemon-wide.
///
/// Latency is deliberately absent below a request threshold: a p50 over three
/// requests is noise, and a confidently wrong number is worse than no number.
pub fn footer(snapshot: &Snapshot) -> String {
    let spend = fmt_usd(snapshot.summary.spend_micro_usd);
    let mut line = format!(
        "today {spend} · {} req · {:.1} req/min · {} tok/min",
        snapshot.summary.requests,
        snapshot.rate.requests_per_minute,
        tokens(snapshot.rate.tokens_per_minute as i64),
    );
    if snapshot.summary.requests == 0 {
        line.push_str("  ·  no requests in this window");
    }
    line
}

/// The whole snapshot as plain text — what a redirected or piped
/// `status --watch` prints once before exiting, so the view stays scriptable
/// instead of refusing to run without a terminal.
pub fn oneshot(snapshot: &Snapshot) -> String {
    let mut out = String::new();
    out.push_str(&snapshot.state_line());
    out.push('\n');
    out.push_str(&footer(snapshot));
    out.push('\n');
    if snapshot.rows.is_empty() {
        return out;
    }
    out.push('\n');
    let rows: Vec<[String; 8]> = snapshot.rows.iter().map(stream_row).collect();
    let widths = column_widths(&rows);
    out.push_str(&pad_row(&STREAM_HEADERS.map(str::to_string), &widths));
    out.push('\n');
    for row in &rows {
        out.push_str(&pad_row(row, &widths));
        out.push('\n');
    }
    out
}

/// Widest cell per column, header included, so the plain-text table lines up.
fn column_widths(rows: &[[String; 8]]) -> [usize; 8] {
    let mut widths = STREAM_HEADERS.map(|h| h.chars().count());
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    widths
}

fn pad_row(cells: &[String; 8], widths: &[usize; 8]) -> String {
    let last = cells.len() - 1;
    cells
        .iter()
        .enumerate()
        .map(|(i, cell)| {
            // Never pad the final column — trailing whitespace on every line
            // is noise in a piped table.
            if i == last {
                cell.clone()
            } else {
                format!("{cell:<width$}", width = widths[i])
            }
        })
        .collect::<Vec<_>>()
        .join("  ")
}

#[cfg(test)]
mod tests {
    use super::*;

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
        }
    }

    #[test]
    fn a_row_renders_every_column_the_stream_promises() {
        let cells = stream_row(&row());
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
        let cells = stream_row(&r);
        assert_eq!(cells[5], "—");
        assert_eq!(cells[6], "—");
    }

    #[test]
    fn a_long_upstream_error_cannot_push_other_columns_off_screen() {
        let mut r = row();
        r.error =
            Some("upstream refused the request\nwith a very long multi-line explanation".into());
        let cells = stream_row(&r);
        assert!(!cells[7].contains('\n'), "newlines would break the row");
        assert!(cells[7].chars().count() <= 25, "{}", cells[7]);
    }

    #[test]
    fn an_empty_window_says_so_rather_than_showing_a_bare_zero() {
        let empty = Snapshot::default();
        assert!(footer(&empty).contains("no requests"), "{}", footer(&empty));
    }

    #[test]
    fn the_oneshot_is_a_padded_table_with_no_trailing_whitespace() {
        let snapshot = Snapshot {
            rows: vec![row()],
            ..Default::default()
        };
        let text = oneshot(&snapshot);
        assert!(text.contains("provider"), "header row present");
        assert!(text.contains("openai"), "data row present");
        for line in text.lines() {
            assert_eq!(line, line.trim_end(), "piped output must not pad line ends");
        }
    }

    #[test]
    fn the_oneshot_still_reports_state_when_there_is_nothing_to_list() {
        let text = oneshot(&Snapshot::default());
        assert!(text.contains("nothing recorded yet"));
        assert!(
            !text.contains("provider"),
            "an empty table header is noise, not information"
        );
    }
}
