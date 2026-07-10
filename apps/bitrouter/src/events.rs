//! `bitrouter events` — the agent-facing cost/failover feed.
//!
//! Reads the local metering database (never the network, never the
//! daemon socket) and renders **aggregated** plain-text lines for
//! harness surfaces:
//!
//! - `--follow`: a long-running stream for Claude Code plugin monitors.
//!   Every line it prints is injected into the agent's context — i.e.
//!   it costs tokens — so emission is throttled hard: immediate lines
//!   for failures, a line per whole-dollar spend crossing, and a rolling
//!   summary at most every ten minutes. Steady-state budget ≤ 6
//!   lines/hour.
//! - `--turn`: a one-shot spend-since-last-call summary for turn-end
//!   hooks (Codex `Stop`). Persists its cursor under the bitrouter
//!   home; with `--hook codex` it reads the hook JSON on stdin (for the
//!   session id) and emits the hook's `{"systemMessage": …}` response.
//!
//! Both modes are **infallible by design**: a missing database, an
//! unreadable config, or a torn cursor file degrades to silence — a
//! cost feed must never break a session.

use std::path::PathBuf;
use std::time::Duration;

use chrono::Utc;

use crate::metering::reader::open_readonly;
use crate::metering::store::{SettledRequest, TimeWindow};
use crate::paths::ConfigSource;

/// Poll cadence while the database exists and has traffic.
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Poll cadence while the database doesn't exist yet.
const ABSENT_RETRY: Duration = Duration::from_secs(30);
/// Minimum spacing between rolling summaries.
const SUMMARY_INTERVAL: Duration = Duration::from_secs(600);
/// Minimum spacing between failure lines.
const FAILURE_COOLDOWN: Duration = Duration::from_secs(60);
/// Row cap per poll.
const TAIL_LIMIT: u64 = 500;

/// Render micro-USD for agent surfaces: two decimals normally, four
/// when the amount would otherwise round to nothing.
pub fn fmt_usd(micro_usd: u64) -> String {
    let usd = micro_usd as f64 / 1_000_000.0;
    if micro_usd == 0 || usd >= 0.01 {
        format!("${usd:.2}")
    } else {
        format!("${usd:.4}")
    }
}

/// `bitrouter events --follow`: stream aggregated lines until killed.
/// Exempt from the one-JSON-object CLI contract (like `serve`): stdout
/// is a line protocol consumed by harness monitors.
pub async fn follow(source: &ConfigSource) {
    let mut cursor = Utc::now().to_rfc3339();
    let mut session = SessionTally::default();
    let mut last_summary_at = std::time::Instant::now();
    let mut last_summary_spend: u64 = 0;
    let mut last_failure_at: Option<std::time::Instant> = None;

    loop {
        let Some(store) = open_readonly(source).await else {
            tokio::time::sleep(ABSENT_RETRY).await;
            continue;
        };
        let rows = match store.settled_since(&cursor, TAIL_LIMIT).await {
            Ok(rows) => rows,
            Err(_) => {
                // Schema not migrated yet or a torn read — stay quiet.
                tokio::time::sleep(ABSENT_RETRY).await;
                continue;
            }
        };
        if let Some(last) = rows.last() {
            cursor = last.created_at.clone();
        }

        let crossed = session.absorb(&rows);
        // Failure lines: immediate but rate-limited to one per cooldown.
        if let Some(failure) = rows.iter().rev().find(|r| r.error.is_some())
            && last_failure_at.is_none_or(|at| at.elapsed() >= FAILURE_COOLDOWN)
        {
            println!("{}", failure_line(failure, session.failed));
            last_failure_at = Some(std::time::Instant::now());
        }
        // Whole-dollar crossings.
        if let Some(dollars) = crossed {
            println!(
                "session spend crossed ${dollars}.00 ({} requests so far)",
                session.requests
            );
        }
        // Rolling summary — only when spend moved since the last one.
        if last_summary_at.elapsed() >= SUMMARY_INTERVAL
            && session.spend_micro_usd != last_summary_spend
        {
            let today = store
                .spend_summary(TimeWindow::Today)
                .await
                .unwrap_or_default();
            println!("{}", summary_line(&session, today.spend_micro_usd));
            last_summary_at = std::time::Instant::now();
            last_summary_spend = session.spend_micro_usd;
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// `bitrouter events --turn`: one-shot spend since the previous call.
/// Prints one plain line (or, in `codex` hook dialect, the hook JSON
/// response) — or nothing at all when there is nothing to report.
pub async fn turn(source: &ConfigSource, hook: Option<&str>) {
    let session_id = match hook {
        // The hook dialects deliver their event JSON on stdin; the
        // session id keys the cursor so concurrent sessions don't
        // steal each other's turns.
        Some(_) => stdin_session_id(),
        None => None,
    };
    let Some(line) = turn_line(source, session_id.as_deref()).await else {
        return;
    };
    match hook {
        Some("codex") => {
            let response = serde_json::json!({ "systemMessage": line });
            println!("{response}");
        }
        _ => println!("{line}"),
    }
}

/// Compute the turn summary and advance the cursor. `None` when there
/// is nothing to report (no database, first call, or an idle turn).
async fn turn_line(source: &ConfigSource, session_id: Option<&str>) -> Option<String> {
    let store = open_readonly(source).await?;
    let cursor_path = turn_cursor_path(source, session_id);
    let now = Utc::now().to_rfc3339();
    let previous = std::fs::read_to_string(&cursor_path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let Some(previous) = previous else {
        // First call for this session: baseline only, never dump history.
        write_cursor(&cursor_path, &now);
        return None;
    };
    let rows = store.settled_since(&previous, TAIL_LIMIT).await.ok()?;
    let next = rows.last().map_or(now, |r| r.created_at.clone());
    write_cursor(&cursor_path, &next);
    if rows.is_empty() {
        return None;
    }
    let spend: u64 = rows.iter().map(|r| r.charge_micro_usd).sum();
    let failed = rows.iter().filter(|r| r.error.is_some()).count();
    let today = store
        .spend_summary(TimeWindow::Today)
        .await
        .unwrap_or_default();
    let failures = if failed > 0 {
        format!(", {failed} failed")
    } else {
        String::new()
    };
    Some(format!(
        "bitrouter: turn spend {} ({} requests{failures}) · today {}",
        fmt_usd(spend),
        rows.len(),
        fmt_usd(today.spend_micro_usd)
    ))
}

/// Process-lifetime tally for `--follow` (the monitor's lifetime is the
/// harness session's lifetime).
#[derive(Default)]
struct SessionTally {
    spend_micro_usd: u64,
    requests: u64,
    failed: u64,
}

impl SessionTally {
    /// Fold a batch in; returns the whole-dollar mark crossed, if any.
    fn absorb(&mut self, rows: &[SettledRequest]) -> Option<u64> {
        if rows.is_empty() {
            return None;
        }
        let before = self.spend_micro_usd / 1_000_000;
        for row in rows {
            self.spend_micro_usd += row.charge_micro_usd;
            self.requests += 1;
            if row.error.is_some() {
                self.failed += 1;
            }
        }
        let after = self.spend_micro_usd / 1_000_000;
        (after > before).then_some(after)
    }
}

fn failure_line(row: &SettledRequest, session_failed: u64) -> String {
    let error = row.error.as_deref().unwrap_or("unknown error");
    let error: String = error.chars().take(80).collect();
    format!(
        "request failed on {}/{}: {error} ({session_failed} failed this session)",
        row.provider_id, row.model_id
    )
}

fn summary_line(session: &SessionTally, today_micro_usd: u64) -> String {
    format!(
        "session spend {} ({} requests, {} failed) · today {}",
        fmt_usd(session.spend_micro_usd),
        session.requests,
        session.failed,
        fmt_usd(today_micro_usd)
    )
}

/// Where a `--turn` cursor lives: `<home>/events/turn-<session>.cursor`.
fn turn_cursor_path(source: &ConfigSource, session_id: Option<&str>) -> PathBuf {
    let key = session_id
        .map(sanitize_session_id)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "default".to_string());
    source
        .home()
        .join("events")
        .join(format!("turn-{key}.cursor"))
}

/// Keep cursor filenames safe regardless of what the harness puts in
/// its session id.
fn sanitize_session_id(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect()
}

/// Best-effort cursor write — a failed write means the next turn
/// re-reports this one, which beats crashing a hook.
fn write_cursor(path: &std::path::Path, value: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, value);
}

/// Extract `session_id` from the hook event JSON on stdin. Both Claude
/// Code and Codex deliver `{"session_id": …}`; anything unreadable
/// degrades to `None` (→ the shared "default" cursor).
fn stdin_session_id() -> Option<String> {
    use std::io::Read;
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(charge: u64, error: Option<&str>) -> SettledRequest {
        SettledRequest {
            created_at: "2026-07-10T00:00:00+00:00".to_string(),
            model_id: "openai/gpt-4o".to_string(),
            provider_id: "openai".to_string(),
            charge_micro_usd: charge,
            error: error.map(|e| e.to_string()),
        }
    }

    #[test]
    fn tally_reports_whole_dollar_crossings_only() {
        let mut tally = SessionTally::default();
        assert_eq!(tally.absorb(&[row(400_000, None)]), None);
        assert_eq!(tally.absorb(&[row(700_000, None)]), Some(1));
        assert_eq!(tally.absorb(&[row(100_000, None)]), None);
        assert_eq!(tally.absorb(&[]), None);
        // A single large batch can cross several dollars at once.
        assert_eq!(tally.absorb(&[row(2_000_000, None)]), Some(3));
        assert_eq!(tally.requests, 4);
    }

    #[test]
    fn tally_counts_failures() {
        let mut tally = SessionTally::default();
        tally.absorb(&[row(0, Some("429 rate limited")), row(10, None)]);
        assert_eq!(tally.failed, 1);
        assert_eq!(tally.requests, 2);
    }

    #[test]
    fn failure_line_truncates_long_errors() {
        let long = "x".repeat(200);
        let line = failure_line(&row(0, Some(&long)), 3);
        assert!(line.contains(&"x".repeat(80)));
        assert!(!line.contains(&"x".repeat(81)));
        assert!(line.ends_with("(3 failed this session)"));
    }

    #[test]
    fn fmt_usd_picks_precision() {
        assert_eq!(fmt_usd(0), "$0.00");
        assert_eq!(fmt_usd(420_000), "$0.42");
        assert_eq!(fmt_usd(3_100_000), "$3.10");
        assert_eq!(fmt_usd(3_200), "$0.0032");
    }

    #[test]
    fn session_ids_are_sanitized() {
        assert_eq!(sanitize_session_id("abc-123_XYZ"), "abc-123_XYZ");
        assert_eq!(sanitize_session_id("../../etc/passwd"), "etcpasswd");
        let long = "a".repeat(100);
        assert_eq!(sanitize_session_id(&long).len(), 64);
    }
}
