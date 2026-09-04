//! Reports for the daemon-lifecycle (`start` / `stop` / `restart` / `reload` /
//! `status`) and `route` commands.

use bitrouter_mcp::actions::status::StatusReport;
use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::{Health, Human};

/// Result of a daemon lifecycle action (`start` / `stop` / `restart` /
/// `reload`). `pid`/`listen`/`models`/`log` are present only when the action
/// produced a live daemon (start / restart).
#[derive(Serialize)]
pub struct DaemonActionReport {
    /// The action performed: `start` | `stop` | `restart` | `reload`.
    pub action: &'static str,
    /// The resulting state: `started` | `stopped` | `restarted` | `reloaded` |
    /// `not_ready`.
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
}

impl DaemonActionReport {
    /// A daemon came up and answered its control socket.
    pub fn started(
        action: &'static str,
        status: &'static str,
        pid: u32,
        listen: String,
        models: usize,
        log: String,
    ) -> Self {
        Self {
            action,
            status,
            pid: Some(pid),
            listen: Some(listen),
            models: Some(models),
            log: Some(log),
        }
    }

    /// The daemon is alive but slow to answer (still migrating / fetching).
    pub fn not_ready(action: &'static str, pid: u32, log: String) -> Self {
        Self {
            action,
            status: "not_ready",
            pid: Some(pid),
            listen: None,
            models: None,
            log: Some(log),
        }
    }

    /// A payload-less outcome (stop / reload).
    pub fn simple(action: &'static str, status: &'static str) -> Self {
        Self {
            action,
            status,
            pid: None,
            listen: None,
            models: None,
            log: None,
        }
    }
}

impl CliReport for DaemonActionReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        match self.pid {
            Some(pid) => {
                let health = if self.status == "not_ready" {
                    Health::Unknown
                } else {
                    Health::Up
                };
                h.status_block(health, &format!("bitrouter daemon {}", self.status))?;
                h.field("pid", pid)?;
                if let Some(listen) = &self.listen {
                    h.field("listen", listen)?;
                }
                if let Some(models) = self.models {
                    h.field("models", format!("{models} routable"))?;
                }
                if let Some(log) = &self.log {
                    h.field("log", log)?;
                }
                Ok(())
            }
            None => h.line(&format!("daemon {}", self.status)),
        }
    }
}

/// The human view of `bitrouter status`. Exit code stays 0 whether running or
/// stopped — "stopped" is an answer, not a failure.
///
/// The report type itself is
/// [`bitrouter_mcp::actions::status::StatusReport`](StatusReport): the `status`
/// tool returns the same type, so `bitrouter status --json` and the tool's
/// structured content are the same bytes. Rendering stays here — a local trait
/// on a foreign type is legal, and it keeps [`Human`] out of the crate.
impl CliReport for StatusReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        if !self.running {
            h.status_block(Health::Down, "bitrouter is stopped")?;
            if let Some(socket) = &self.socket {
                h.field("socket", socket)?;
            }
            // Spend outlives the daemon: what a past daemon spent is on disk
            // and stays true after it exits, so it is shown here too.
            render_spend(self.spend.as_ref(), h)?;
            return h.note("Run `bitrouter start` to launch the daemon.");
        }
        h.status_block(Health::Up, "bitrouter is running")?;
        if let Some(pid) = self.pid {
            h.field("pid", pid)?;
        }
        if let Some(listen) = &self.listen {
            h.field("listen", listen)?;
        }
        if let Some(models) = self.models {
            h.field("models", format!("{models} routable"))?;
        }
        if !self.providers.is_empty() {
            h.field("providers", self.providers.join(", "))?;
        }
        if let Some(socket) = &self.socket {
            h.field("socket", socket)?;
        }
        render_spend(self.spend.as_ref(), h)
    }
}

/// The `spend` block of [`StatusReport`], in the human view.
///
/// Two lines at most, because they are two independent facts: `spend` is money
/// already gone, `credits` is what a capped deployment will still let you
/// spend. A deployment that answers only one prints only one.
///
/// `unpriced` is never rounded away or averaged in. When some requests in the
/// window carried no charge evidence the total is labelled a **floor** and the
/// count is shown, because the one place being wrong about this costs the
/// reader money is exactly here.
fn render_spend(
    spend: Option<&bitrouter_mcp::actions::status::Spend>,
    h: &mut Human<'_>,
) -> std::io::Result<()> {
    let Some(spend) = spend else {
        return Ok(());
    };
    if let Some(spent) = &spend.spent {
        let amount = crate::metering::fmt_usd(spent.estimated_micro_usd);
        let line = match spent.unpriced {
            0 => format!("{amount} {} ({} requests)", spent.window, spent.requests),
            unpriced => format!(
                "{amount}+ {} ({} requests, {unpriced} unpriced — floor, not a total)",
                spent.window, spent.requests
            ),
        };
        h.field("spend", line)?;
    }
    if let Some(limit) = &spend.limit {
        // Not `metering::fmt_usd`: that takes an unsigned amount and hard-
        // codes a `$`, neither of which holds for a signed balance in a
        // currency the account declares.
        h.field(
            "credits",
            format!(
                "{:.2} {} remaining",
                limit.remaining_micro_usd as f64 / 1_000_000.0,
                spend.currency
            ),
        )?;
    }
    Ok(())
}

/// One hop of a resolved route chain: provider → upstream service id → protocol.
#[derive(Serialize)]
pub struct RouteHopView {
    pub provider: String,
    pub service_id: String,
    pub protocol: String,
}

/// Result of `bitrouter route <model>`.
#[derive(Serialize)]
pub struct RouteReport {
    pub model: String,
    /// Where the chain came from: `live daemon` | `config` | `zero-config`.
    pub resolved_via: String,
    pub chain: Vec<RouteHopView>,
}

impl CliReport for RouteReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.line(&format!(
            "model: {}  (resolved via: {})",
            self.model, self.resolved_via
        ))?;
        if self.chain.is_empty() {
            return h.line("  (empty chain — no provider declares this model)");
        }
        for (i, hop) in self.chain.iter().enumerate() {
            h.line(&format!(
                "  {}. {} → {} ({})",
                i + 1,
                hop.provider,
                hop.service_id,
                hop.protocol
            ))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{Format, Output};

    fn json(r: &dyn CliReport) -> serde_json::Value {
        serde_json::from_slice(&Output::new(Format::Json).render_to_vec(r)).unwrap()
    }

    use bitrouter_mcp::actions::status::{Spend, SpendLimit, Spent};

    /// What the local path builds: the `spent` half only.
    fn local_spend(estimated_micro_usd: u64, requests: u64, unpriced: u64) -> Spend {
        Spend {
            currency: "USD".into(),
            spent: Some(Spent {
                window: "today".into(),
                estimated_micro_usd,
                requests,
                unpriced,
            }),
            limit: None,
        }
    }

    #[test]
    fn status_running_json_and_human() {
        let r = StatusReport::running(
            7,
            "127.0.0.1:4356".into(),
            42,
            vec!["anthropic".into(), "openai".into()],
            "/x.sock".into(),
            Some(local_spend(1_230_000, 9, 0)),
        );
        assert_eq!(
            json(&r),
            serde_json::json!({
                "running": true, "pid": 7, "listen": "127.0.0.1:4356", "models": 42,
                "providers": ["anthropic", "openai"], "socket": "/x.sock",
                "spend": {
                    "currency": "USD",
                    "spent": {
                        "window": "today", "estimated_micro_usd": 1_230_000,
                        "requests": 9, "unpriced": 0
                    }
                }
            })
        );
        let h = String::from_utf8(Output::new(Format::Human).render_to_vec(&r)).unwrap();
        assert!(h.contains("● bitrouter is running"), "{h:?}");
        assert!(h.contains("  models    42 routable"), "{h:?}");
        assert!(h.contains("anthropic, openai"), "{h:?}");
        assert!(h.contains("$1.23 today (9 requests)"), "{h:?}");
    }

    /// The invariant that costs money to get wrong: a partial figure must
    /// never read like a total. `unpriced` reaches JSON verbatim and the human
    /// view marks the number a floor.
    #[test]
    fn status_spend_never_hides_unpriced_requests() {
        let r = StatusReport::running(
            7,
            "127.0.0.1:4356".into(),
            1,
            vec!["openai".into()],
            "/x.sock".into(),
            Some(local_spend(500_000, 10, 4)),
        );
        assert_eq!(json(&r)["spend"]["spent"]["unpriced"], 4);
        let h = String::from_utf8(Output::new(Format::Human).render_to_vec(&r)).unwrap();
        assert!(h.contains("4 unpriced"), "{h:?}");
        assert!(h.contains("floor, not a total"), "{h:?}");
    }

    /// Spend is not a liveness fact. The metering database records what a past
    /// daemon spent and reads fine with nothing listening, so a stopped report
    /// still answers "am I OK to spend?".
    #[test]
    fn status_stopped_still_reports_spend() {
        let r = StatusReport::stopped("/x.sock".into(), Some(local_spend(0, 0, 0)));
        assert_eq!(
            json(&r),
            serde_json::json!({
                "running": false, "providers": [], "socket": "/x.sock",
                "spend": {
                    "currency": "USD",
                    "spent": {
                        "window": "today", "estimated_micro_usd": 0,
                        "requests": 0, "unpriced": 0
                    }
                }
            })
        );
        let h = String::from_utf8(Output::new(Format::Human).render_to_vec(&r)).unwrap();
        assert!(h.contains("○ bitrouter is stopped"), "{h:?}");
        assert!(h.contains("$0.00 today (0 requests)"), "{h:?}");
    }

    #[test]
    fn status_stopped_omits_optional_fields() {
        let r = StatusReport::stopped("/x.sock".into(), None);
        assert_eq!(
            json(&r),
            serde_json::json!({"running": false, "providers": [], "socket": "/x.sock"})
        );
    }

    /// The cloud half: a cap with no spend-to-date. The two halves render
    /// independently, so a report carrying only `limit` prints only `credits`.
    #[test]
    fn status_renders_a_limit_without_a_spent_figure() {
        let r = StatusReport::metered(Spend {
            currency: "USD".into(),
            spent: None,
            limit: Some(SpendLimit {
                balance_micro_usd: 5_000_000,
                pending_micro_usd: 769_000,
                remaining_micro_usd: 4_231_000,
            }),
        });
        let h = String::from_utf8(Output::new(Format::Human).render_to_vec(&r)).unwrap();
        assert!(h.contains("credits   4.23 USD remaining"), "{h:?}");
        assert!(!h.contains("spend"), "{h:?}");
    }

    /// The whole point of the shared type: what the CLI prints is what the MCP
    /// tool returns, so the tool's structured content deserializes straight
    /// back into the report the CLI emitted.
    #[test]
    fn status_json_round_trips_through_the_shared_type() {
        let r = StatusReport::running(
            7,
            "127.0.0.1:4356".into(),
            42,
            vec!["openai".into()],
            "/x.sock".into(),
            Some(local_spend(42, 1, 1)),
        );
        let back: StatusReport = serde_json::from_value(json(&r)).expect("round trip");
        assert_eq!(serde_json::to_value(&back).unwrap(), json(&r));
    }

    #[test]
    fn route_empty_chain_is_empty_array() {
        let r = RouteReport {
            model: "m".into(),
            resolved_via: "config".into(),
            chain: vec![],
        };
        assert_eq!(json(&r)["chain"], serde_json::json!([]));
    }

    #[test]
    fn daemon_action_simple_one_liner() {
        let r = DaemonActionReport::simple("stop", "stopped");
        assert_eq!(
            json(&r),
            serde_json::json!({"action": "stop", "status": "stopped"})
        );
        let h = String::from_utf8(Output::new(Format::Human).render_to_vec(&r)).unwrap();
        assert_eq!(h, "daemon stopped\n");
    }
}
