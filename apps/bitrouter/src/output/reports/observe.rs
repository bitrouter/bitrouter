//! Report for `bitrouter observe status`.

use serde::Serialize;

use crate::daemon::ObserveStatusPayload;
use crate::output::CliReport;
use crate::output::human::{Health, Human};

/// Result of `bitrouter observe status` — the OTel exporter snapshot plus
/// whether the daemon was reachable (so "feature off" is distinguishable from
/// "daemon down"). The snapshot fields are flattened to the top level.
#[derive(Serialize)]
pub struct ObserveStatusReport {
    pub daemon_reachable: bool,
    #[serde(flatten)]
    pub snapshot: ObserveStatusPayload,
    pub socket: String,
}

impl CliReport for ObserveStatusReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        let s = &self.snapshot;
        if !self.daemon_reachable {
            h.status_block(Health::Down, "bitrouter observe — daemon stopped")?;
            h.field("compiled", if s.compiled_in { "yes" } else { "no" })?;
            h.field("socket", &self.socket)?;
            return h.note("Run `bitrouter start` to launch the daemon, then re-run this command.");
        }
        let (health, headline) = if s.exporter_wired {
            (Health::Up, "OTel exporter is wired")
        } else if s.compiled_in {
            (
                Health::Down,
                "OTel feature compiled in, exporter not configured",
            )
        } else {
            (Health::Down, "OTel feature not compiled in")
        };
        h.status_block(health, &format!("bitrouter observe — {headline}"))?;
        h.field("compiled", if s.compiled_in { "yes" } else { "no" })?;
        h.field("wired", if s.exporter_wired { "yes" } else { "no" })?;
        if let Some(endpoint) = &s.endpoint {
            h.field("endpoint", endpoint)?;
        }
        if let Some(service) = &s.service_name {
            h.field("service", service)?;
        }
        if let Some(sampler) = &s.sampler {
            let val = match s.sampler_arg {
                Some(arg) => format!("{sampler} (arg={arg})"),
                None => sampler.clone(),
            };
            h.field("sampler", val)?;
        }
        h.field("metrics", if s.metrics_enabled { "on" } else { "off" })?;
        h.field("headers", s.header_count)?;
        h.field("res-attrs", s.resource_attribute_count)?;
        h.field(
            "api-keys",
            format!("{} / {}", s.api_key_count, s.api_key_cap),
        )?;
        h.field("users", format!("{} / {}", s.user_id_count, s.user_id_cap))?;
        h.field("in-flight", s.active_spans)?;
        h.field("socket", &self.socket)
    }
}
