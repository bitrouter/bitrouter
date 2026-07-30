use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::Human;

/// Transport-neutral eval exchange output.
#[derive(Debug, Clone, Serialize)]
pub struct EvalReport {
    pub action: String,
    pub data: serde_json::Value,
}

impl CliReport for EvalReport {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        human.line(&format!("eval {}", self.action))?;
        let rendered = serde_json::to_string_pretty(&self.data).map_err(std::io::Error::other)?;
        for line in rendered.lines() {
            human.line(line)?;
        }
        Ok(())
    }
}
