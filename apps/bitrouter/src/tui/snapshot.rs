//! The one data shape both observability surfaces render.
//!
//! Polled from the metering store (which opens read-only, and works with no
//! daemon running) plus the daemon's control socket. The split matters for
//! honesty: the store answers "what has happened", the socket answers "is
//! anything listening", and the view must never present one as the other —
//! an empty list because nothing ran and an empty list because the daemon is
//! gone are different facts, so they are different [`Mode`]s.

use std::path::Path;
use std::time::Duration;

use crate::metering::store::{RateMetrics, RequestRow, SpendSummary, TimeWindow};

/// How many rows the stream ever holds: one tall screen plus scrollback
/// margin. The query is `LIMIT`ed to this so a day-long window costs the same
/// as an empty one.
pub const STREAM_ROWS: u64 = 500;

/// What the view is looking at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// A daemon is answering, and history is readable.
    Live,
    /// No daemon, but the store has rows — history is still worth showing.
    HistoryOnly,
    /// No daemon and nothing recorded (or no store at all).
    #[default]
    Empty,
}

/// The running daemon, as its control socket describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonState {
    /// Process id.
    pub pid: u32,
    /// HTTP listen address.
    pub listen: String,
    /// Count of routable models.
    pub models: usize,
}

/// Whose traffic the numbers describe.
///
/// Derived from whether attribution **actually landed**, not from whether a
/// launch token was minted. A harness with its own session identity — Claude
/// Code on a Max subscription, for one — can ignore the credential `launch`
/// hands it, and then a bar that trusted the mint would report daemon-wide
/// figures while implying they were the session's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Scope {
    /// Every caller this daemon served.
    #[default]
    DaemonWide,
    /// Exactly one `bitrouter launch` session.
    Launch,
}

/// One poll of everything the view draws.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// `None` when nothing is listening on the control socket.
    pub daemon: Option<DaemonState>,
    /// Newest-first settled requests.
    pub rows: Vec<RequestRow>,
    /// Spend + request count for the window.
    pub summary: SpendSummary,
    /// Trailing-minute rate across every caller.
    pub rate: RateMetrics,
    /// Whose traffic `rows` and `summary` describe.
    pub scope: Scope,
}

impl Snapshot {
    /// Which of the three states this snapshot represents. Derived rather than
    /// stored so it can never disagree with the data beside it.
    pub fn mode(&self) -> Mode {
        match (self.daemon.is_some(), self.rows.is_empty()) {
            (true, _) => Mode::Live,
            (false, false) => Mode::HistoryOnly,
            (false, true) => Mode::Empty,
        }
    }

    /// The header's one-line state, stated rather than implied. An empty list
    /// must never be left to look like "no traffic" when the real answer is
    /// "nothing is running".
    pub fn state_line(&self) -> String {
        match (&self.daemon, self.mode()) {
            (Some(d), _) => format!(
                "● live · pid {} · {} · {} models",
                d.pid, d.listen, d.models
            ),
            (None, Mode::HistoryOnly) => "○ history only — daemon not running".to_string(),
            (None, _) => "○ nothing recorded yet — try bitrouter serve".to_string(),
        }
    }
}

/// Read one snapshot. Never fails: every source degrades to absence, because
/// a monitoring view that errors out is worse than one reporting less.
pub async fn poll(
    source: &crate::paths::ConfigSource,
    socket: &Path,
    window: TimeWindow,
    launch_id: Option<&str>,
) -> Snapshot {
    let daemon = daemon_state(socket).await;
    let Some(store) = crate::metering::reader::open_readonly(source).await else {
        return Snapshot {
            daemon,
            ..Default::default()
        };
    };
    let rate = store.get_total_rate().await.unwrap_or_default();

    // Prefer launch scope, but only once it has something to show. Minting a
    // token is a request, not a guarantee: the harness has to send it back,
    // and some do not. Falling back — and *saying so* via `Scope` — keeps the
    // bar honest either way, instead of showing an empty session forever or,
    // worse, showing the daemon's numbers as if they were the session's.
    if let Some(launch) = launch_id {
        let summary = store
            .spend_summary_for_launch(launch, window)
            .await
            .unwrap_or_default();
        if summary.requests > 0 {
            return Snapshot {
                daemon,
                rows: store
                    .recent_requests(window, STREAM_ROWS, Some(launch))
                    .await
                    .unwrap_or_default(),
                summary,
                rate,
                scope: Scope::Launch,
            };
        }
    }
    Snapshot {
        daemon,
        rows: store
            .recent_requests(window, STREAM_ROWS, None)
            .await
            .unwrap_or_default(),
        summary: store.spend_summary(window).await.unwrap_or_default(),
        rate,
        scope: Scope::DaemonWide,
    }
}

/// Bound on the control-socket probe.
///
/// `send_command` has no timeout of its own, and this runs inline in the
/// render loop: a daemon that accepts the connection but never answers — busy,
/// half-dead, paused under a debugger — would otherwise freeze the view and,
/// in hosted mode, stop forwarding keystrokes to the child. The surface is
/// least useful exactly when the daemon is misbehaving, so it must never wait
/// on one.
const DAEMON_PROBE_TIMEOUT: Duration = Duration::from_millis(750);

async fn daemon_state(socket: &Path) -> Option<DaemonState> {
    let probe = crate::daemon::send_command(socket, &crate::daemon::DaemonCommand::Status);
    let Ok(response) = tokio::time::timeout(DAEMON_PROBE_TIMEOUT, probe).await else {
        // Unresponsive reads as absent: the header then says the daemon is not
        // answering, which is true and more useful than a frozen frame.
        return None;
    };
    match response {
        Ok(crate::daemon::DaemonResponse::Status {
            pid,
            listen,
            models,
        }) => Some(DaemonState {
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

    fn row(id: &str) -> RequestRow {
        RequestRow {
            request_id: id.to_string(),
            created_at: "2026-08-10T12:00:00Z".to_string(),
            model_id: "gpt-5".to_string(),
            provider_id: "openai".to_string(),
            prompt_tokens: 10,
            completion_tokens: 5,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            estimated_charge_micro_usd: 70,
            latency_ms: 100,
            error: None,
        }
    }

    fn daemon() -> DaemonState {
        DaemonState {
            pid: 4412,
            listen: "127.0.0.1:4356".to_string(),
            models: 47,
        }
    }

    #[test]
    fn a_dead_daemon_with_history_is_not_the_same_as_a_fresh_install() {
        let history = Snapshot {
            daemon: None,
            rows: vec![row("a")],
            ..Default::default()
        };
        assert_eq!(history.mode(), Mode::HistoryOnly);
        assert!(history.state_line().contains("history only"));

        let fresh = Snapshot::default();
        assert_eq!(fresh.mode(), Mode::Empty);
        assert!(
            fresh.state_line().contains("bitrouter serve"),
            "an empty view must say what to do, not just show nothing"
        );
    }

    #[test]
    fn a_live_daemon_with_no_traffic_still_reads_as_live() {
        // The failure this guards: showing "nothing recorded yet" while a
        // daemon is up and simply idle, which reads as broken.
        let idle = Snapshot {
            daemon: Some(daemon()),
            ..Default::default()
        };
        assert_eq!(idle.mode(), Mode::Live);
        let line = idle.state_line();
        assert!(line.contains("● live"), "{line}");
        assert!(line.contains("pid 4412"), "{line}");
        assert!(line.contains("47 models"), "{line}");
    }
}
