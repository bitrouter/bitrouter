use std::collections::BTreeMap;

use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::Human;

/// Source and outcome for explicit routing-policy file operations.
#[derive(Debug, Clone, Serialize)]
pub struct PolicyReport {
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// Explicit provenance of the inspected or written source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub policies: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bindings: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<bitrouter_ai::policy::Document>,
    pub applied: bool,
    /// Present when this command changed `bitrouter.yaml`. `saved_only` avoids
    /// implying that a running daemon consumed the edit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_activation: Option<super::config::ConfigActivation>,
}

impl CliReport for PolicyReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.line(&format!("policy {}", self.action))?;
        if let Some(path) = &self.path {
            h.line(&format!("  file: {path}"))?;
        }
        if let Some(path) = &self.candidate_path {
            h.line(&format!("  candidate: {path}"))?;
        }
        if let Some(digest) = &self.digest {
            h.line(&format!("  digest: {digest}"))?;
        }
        if let Some(source) = &self.source {
            h.line(&format!("  source: {source}"))?;
        }
        if !self.policies.is_empty() {
            h.line(&format!("  policies: {}", self.policies.join(", ")))?;
        }
        for (router, policy) in &self.bindings {
            h.line(&format!("  bitrouter/{router} -> {policy}"))?;
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
        if matches!(
            self.config_activation,
            Some(super::config::ConfigActivation::SavedOnly)
        ) {
            h.line("  configuration: saved only")?;
            h.line(&format!(
                "  inspect `{} status` before assuming the running daemon changed",
                crate::commands::invocation::name()
            ))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{Format, Output};

    #[test]
    fn disk_source_is_resubmittable_and_marks_its_provenance() -> anyhow::Result<()> {
        let report = PolicyReport {
            action: "show".to_string(),
            path: Some("/tmp/policy.yaml".to_string()),
            candidate_path: None,
            digest: Some("digest".to_string()),
            source: Some("disk".to_string()),
            policies: vec!["default".to_string()],
            bindings: BTreeMap::new(),
            changes: Vec::new(),
            policy: Some(serde_json::from_value(serde_json::json!({
                "spec_version":"policy/1.0.0-beta.1", "policies":{"default":{
                    "tiers":{"fast":{"model":"fixture/fast"}},"default_tier":"fast"
                }}
            }))?),
            applied: false,
            config_activation: None,
        };

        let value = serde_json::to_value(report)?;
        assert_eq!(value["action"], "show");
        assert_eq!(value["path"], "/tmp/policy.yaml");
        let source: bitrouter_ai::policy::Document =
            serde_json::from_value(value["policy"].clone())?;
        assert_eq!(
            source.policies["default"].tiers["fast"].model,
            "fixture/fast"
        );
        assert_eq!(value["applied"], false);
        assert_eq!(value["source"], "disk");
        Ok(())
    }

    #[test]
    fn init_marks_config_edits_as_saved_only() -> anyhow::Result<()> {
        let report = PolicyReport {
            action: "init".to_string(),
            path: Some("/tmp/policy.yaml".to_string()),
            candidate_path: None,
            digest: Some("digest".to_string()),
            source: Some("disk".to_string()),
            policies: vec!["default".to_string()],
            bindings: BTreeMap::from([("coding".to_string(), "default".to_string())]),
            changes: vec!["saved named policy binding".to_string()],
            policy: None,
            applied: true,
            config_activation: Some(super::super::config::ConfigActivation::SavedOnly),
        };

        let value = serde_json::to_value(&report)?;
        assert_eq!(value["config_activation"], "saved_only");
        let human = String::from_utf8(Output::new(Format::Human).render_to_vec(&report))?;
        assert!(human.contains("configuration: saved only"), "{human}");
        assert!(human.contains("status"), "{human}");
        assert!(human.contains("bitrouter/coding -> default"), "{human}");
        assert!(
            human.contains("before assuming the running daemon changed"),
            "{human}"
        );
        Ok(())
    }
}
