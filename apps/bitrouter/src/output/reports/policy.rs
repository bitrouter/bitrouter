use std::collections::BTreeMap;

use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::Human;

/// Uniform result for routing-policy lock commands.
#[derive(Debug, Clone, Serialize)]
pub struct PolicyReport {
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    pub writeback: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policies: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bindings: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<serde_json::Value>,
    pub applied: bool,
}

impl CliReport for PolicyReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.line(&format!("policy {}", self.action))?;
        if let Some(path) = &self.path {
            h.line(&format!("  file: {path}"))?;
        }
        if let Some(digest) = &self.digest {
            h.line(&format!("  digest: {digest}"))?;
        }
        h.line(&format!("  writeback: {}", self.writeback))?;
        if !self.policies.is_empty() {
            h.line(&format!("  policies: {}", self.policies.join(", ")))?;
        }
        for (preset, policy) in &self.bindings {
            h.line(&format!("  @{preset} -> {policy}"))?;
        }
        for change in &self.changes {
            h.line(&format!("  {change}"))?;
        }
        if let Some(policy) = &self.policy {
            h.blank()?;
            let rendered = serde_saphyr::to_string(policy).map_err(std::io::Error::other)?;
            for line in rendered.lines() {
                h.line(line)?;
            }
        }
        if self.applied {
            h.line("  applied: yes")?;
        }
        Ok(())
    }
}
