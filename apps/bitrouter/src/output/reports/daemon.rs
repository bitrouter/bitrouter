//! Reports for the daemon-lifecycle (`start` / `stop` / `restart` / `reload` /
//! `status`) and `route` commands.

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

/// Result of `bitrouter status`. Exit code stays 0 whether running or stopped —
/// "stopped" is an answer, not a failure.
#[derive(Serialize)]
pub struct StatusReport {
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<usize>,
    pub socket: String,
}

impl StatusReport {
    pub fn running(pid: u32, listen: String, models: usize, socket: String) -> Self {
        Self {
            running: true,
            pid: Some(pid),
            listen: Some(listen),
            models: Some(models),
            socket,
        }
    }
    pub fn stopped(socket: String) -> Self {
        Self {
            running: false,
            pid: None,
            listen: None,
            models: None,
            socket,
        }
    }
}

impl CliReport for StatusReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        if self.running {
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
            h.field("socket", &self.socket)
        } else {
            h.status_block(Health::Down, "bitrouter is stopped")?;
            h.field("socket", &self.socket)?;
            h.note("Run `bitrouter start` to launch the daemon.")
        }
    }
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

    #[test]
    fn status_running_json_and_human() {
        let r = StatusReport::running(7, "127.0.0.1:4356".into(), 42, "/x.sock".into());
        assert_eq!(
            json(&r),
            serde_json::json!({
                "running": true, "pid": 7, "listen": "127.0.0.1:4356", "models": 42, "socket": "/x.sock"
            })
        );
        let h = String::from_utf8(Output::new(Format::Human).render_to_vec(&r)).unwrap();
        assert!(h.contains("● bitrouter is running"), "{h:?}");
        assert!(h.contains("  models    42 routable"), "{h:?}");
    }

    #[test]
    fn status_stopped_omits_optional_fields() {
        let r = StatusReport::stopped("/x.sock".into());
        assert_eq!(
            json(&r),
            serde_json::json!({"running": false, "socket": "/x.sock"})
        );
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
