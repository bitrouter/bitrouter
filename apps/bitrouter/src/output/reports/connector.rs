//! Shared JSON/human output for standalone connector observation.

use crate::connector::observer::PoolSnapshot;
use crate::connector::report::{DryRunReport, render_dry_run_human, render_human};
use crate::output::CliReport;
use crate::output::human::Human;

#[derive(serde::Serialize)]
#[serde(transparent)]
pub struct ControlReport(pub serde_json::Value);

impl CliReport for ControlReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        if let Some(id) = self.0.get("request_id").and_then(serde_json::Value::as_str) {
            let state = self
                .0
                .get("state")
                .and_then(serde_json::Value::as_str)
                .map_or("unknown", |value| value);
            return h.line(&format!("Request {id}: {state}"));
        }
        // Control reports contain only controller metadata and bounded reasons;
        // prompt/response bodies and credentials never enter this report type.
        h.line(&serde_json::to_string_pretty(&self.0).map_err(std::io::Error::other)?)
    }
}

impl CliReport for DryRunReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.line(&render_dry_run_human(self))
    }
}

impl CliReport for PoolSnapshot {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.line(&render_human(self))
    }

    fn exit_code(&self) -> i32 {
        if self.is_ready() { 0 } else { 1 }
    }
}
