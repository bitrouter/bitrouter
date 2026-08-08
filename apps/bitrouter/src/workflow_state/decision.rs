use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use bitrouter_sdk::{BitrouterError, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::workflow_state::ir::WorkflowIdentity;

pub const POLICY_DECISION_JSONL_ENV: &str = "BITROUTER_POLICY_DECISION_JSONL";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecisionRecord {
    #[serde(default)]
    pub captured_at: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
    pub input_model: String,
    pub key_strategy: String,
    pub request_key: String,
    /// Database key for adequacy state. Named policies namespace this while
    /// keeping `request_key` human-readable. Old records omit it.
    #[serde(default)]
    pub ledger_key: Option<String>,
    /// Named lock policy that made the decision.
    #[serde(default)]
    pub policy: Option<String>,
    /// Semantic digest of the active policy lock.
    #[serde(default)]
    pub policy_digest: Option<String>,
    /// Effective preset variant, when known.
    #[serde(default)]
    pub preset_variant: Option<String>,
    /// Strong/default comparison tier used by evaluators.
    #[serde(default)]
    pub baseline_tier: Option<String>,
    pub legacy_fingerprint: String,
    #[serde(rename = "trace_state", alias = "workflow_state")]
    pub workflow_state: String,
    #[serde(rename = "trace_identity", default, alias = "workflow_identity")]
    pub workflow_identity: WorkflowIdentity,
    #[serde(default)]
    pub static_tier: Option<String>,
    #[serde(default)]
    pub static_model: Option<String>,
    #[serde(default)]
    pub selected_tier: Option<String>,
    #[serde(default)]
    pub selected_model: Option<String>,
    #[serde(default)]
    pub continuation_proposed_tier: Option<String>,
    #[serde(default)]
    pub continuation_proposed_model: Option<String>,
    #[serde(default)]
    pub continuation_adjustment: Option<String>,
    #[serde(default)]
    pub predicted_role: Option<String>,
    #[serde(default)]
    pub predicted_action: Option<String>,
    #[serde(default)]
    pub prediction_confidence_ppm: Option<u32>,
    #[serde(default)]
    pub prediction_reason_codes: Vec<String>,
    #[serde(default)]
    pub observed_route_projection: Option<String>,
    #[serde(default)]
    pub trajectory_episode_id: Option<String>,
    #[serde(default)]
    pub trajectory_sequence: Option<u64>,
    #[serde(default)]
    pub trajectory_completeness: Option<String>,
    #[serde(default)]
    pub trajectory_health_digest: Option<String>,
    #[serde(default)]
    pub candidate_tier: Option<String>,
    #[serde(default)]
    pub progress_clause_ids: Vec<String>,
    pub reason: String,
    pub pinned: bool,
    #[serde(default)]
    pub request_qualified: bool,
    #[serde(default)]
    pub semantic_successes: u32,
    #[serde(default)]
    pub semantic_success_threshold: u32,
    pub locked: bool,
    pub trialed: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDecisionSummary {
    pub total: usize,
    pub routed_count: usize,
    pub pinned_count: usize,
    pub locked_count: usize,
    pub trialed_count: usize,
    pub by_selected_tier: BTreeMap<String, usize>,
    pub by_selected_model: BTreeMap<String, usize>,
    #[serde(default)]
    pub by_predicted_role: BTreeMap<String, usize>,
    #[serde(default)]
    pub by_predicted_action: BTreeMap<String, usize>,
    pub static_tier_replaced_count: usize,
    pub static_model_replaced_count: usize,
    pub by_tier_transition: BTreeMap<String, usize>,
    pub by_model_transition: BTreeMap<String, usize>,
    pub replacement_by_reason: BTreeMap<String, usize>,
    pub by_reason: BTreeMap<String, usize>,
    #[serde(rename = "by_trace_state", alias = "by_workflow_state")]
    pub by_workflow_state: BTreeMap<String, usize>,
    pub by_agent_role: BTreeMap<String, usize>,
    pub by_context_epoch: BTreeMap<u32, usize>,
}

pub struct PolicyDecisionJsonlRecorder {
    path: PathBuf,
    lock: Mutex<()>,
}

impl PolicyDecisionRecord {
    /// Canonical JSON calls this field `trace_state`; this accessor lets newer
    /// Rust callers use the canonical terminology without breaking older field
    /// access or struct literals.
    pub fn trace_state(&self) -> &str {
        &self.workflow_state
    }

    /// Canonical JSON calls this field `trace_identity`; this accessor lets
    /// newer Rust callers use that terminology without breaking older callers.
    pub fn trace_identity(&self) -> &WorkflowIdentity {
        &self.workflow_identity
    }

    pub fn captured_now(mut self) -> Self {
        self.captured_at = Some(Utc::now().to_rfc3339());
        self
    }

    pub fn load_jsonl(path: impl AsRef<Path>) -> Result<Vec<Self>> {
        let file = File::open(path.as_ref()).map_err(|e| {
            BitrouterError::internal(format!(
                "policy decision jsonl open {}: {e}",
                path.as_ref().display()
            ))
        })?;
        let reader = BufReader::new(file);
        let mut records = Vec::new();
        for (idx, line) in reader.lines().enumerate() {
            let line = line.map_err(|e| {
                BitrouterError::internal(format!(
                    "policy decision jsonl read {}: {e}",
                    path.as_ref().display()
                ))
            })?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            records.push(serde_json::from_str(trimmed).map_err(|e| {
                BitrouterError::bad_request(format!(
                    "policy decision jsonl parse {} line {}: {e}",
                    path.as_ref().display(),
                    idx + 1
                ))
            })?);
        }
        Ok(records)
    }

    pub fn write_jsonl(path: impl AsRef<Path>, records: &[Self]) -> Result<()> {
        if let Some(parent) = path.as_ref().parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|e| {
                BitrouterError::internal(format!(
                    "policy decision jsonl mkdir {}: {e}",
                    parent.display()
                ))
            })?;
        }
        let file = File::create(path.as_ref()).map_err(|e| {
            BitrouterError::internal(format!(
                "policy decision jsonl create {}: {e}",
                path.as_ref().display()
            ))
        })?;
        let mut writer = BufWriter::new(file);
        for record in records {
            serde_json::to_writer(&mut writer, record).map_err(|e| {
                BitrouterError::internal(format!("policy decision jsonl serialize: {e}"))
            })?;
            writer.write_all(b"\n").map_err(|e| {
                BitrouterError::internal(format!("policy decision jsonl write: {e}"))
            })?;
        }
        writer
            .flush()
            .map_err(|e| BitrouterError::internal(format!("policy decision jsonl flush: {e}")))
    }
}

impl PolicyDecisionSummary {
    pub fn from_records(records: &[PolicyDecisionRecord]) -> Self {
        let mut summary = Self {
            total: records.len(),
            ..Self::default()
        };
        for record in records {
            if record.selected_model.is_some() {
                summary.routed_count += 1;
            }
            if record.pinned {
                summary.pinned_count += 1;
            }
            if record.locked {
                summary.locked_count += 1;
            }
            if record.trialed {
                summary.trialed_count += 1;
            }
            if let Some(tier) = record.selected_tier.as_deref() {
                *summary
                    .by_selected_tier
                    .entry(tier.to_string())
                    .or_insert(0) += 1;
            }
            if let (Some(static_tier), Some(selected_tier)) = (
                record.static_tier.as_deref(),
                record.selected_tier.as_deref(),
            ) {
                *summary
                    .by_tier_transition
                    .entry(transition_key(static_tier, selected_tier))
                    .or_insert(0) += 1;
                if static_tier != selected_tier {
                    summary.static_tier_replaced_count += 1;
                    *summary
                        .replacement_by_reason
                        .entry(record.reason.clone())
                        .or_insert(0) += 1;
                }
            }
            if let Some(model) = record.selected_model.as_deref() {
                *summary
                    .by_selected_model
                    .entry(model.to_string())
                    .or_insert(0) += 1;
            }
            if let Some(role) = record.predicted_role.as_deref() {
                *summary
                    .by_predicted_role
                    .entry(role.to_string())
                    .or_insert(0) += 1;
            }
            if let Some(action) = record.predicted_action.as_deref() {
                *summary
                    .by_predicted_action
                    .entry(action.to_string())
                    .or_insert(0) += 1;
            }
            if let (Some(static_model), Some(selected_model)) = (
                record.static_model.as_deref(),
                record.selected_model.as_deref(),
            ) {
                *summary
                    .by_model_transition
                    .entry(transition_key(static_model, selected_model))
                    .or_insert(0) += 1;
                if static_model != selected_model {
                    summary.static_model_replaced_count += 1;
                }
            }
            *summary.by_reason.entry(record.reason.clone()).or_insert(0) += 1;
            *summary
                .by_workflow_state
                .entry(record.workflow_state.clone())
                .or_insert(0) += 1;
            *summary
                .by_agent_role
                .entry(record.workflow_identity.role.as_str().to_string())
                .or_insert(0) += 1;
            *summary
                .by_context_epoch
                .entry(record.workflow_identity.context_epoch)
                .or_insert(0) += 1;
        }
        summary
    }
}

fn transition_key(from: &str, to: &str) -> String {
    format!("{from} -> {to}")
}

impl PolicyDecisionJsonlRecorder {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|e| {
                BitrouterError::internal(format!(
                    "policy decision recorder mkdir {}: {e}",
                    parent.display()
                ))
            })?;
        }
        Ok(Self {
            path,
            lock: Mutex::new(()),
        })
    }

    pub fn from_env() -> Result<Option<Self>> {
        let Some(path) =
            std::env::var_os(POLICY_DECISION_JSONL_ENV).filter(|value| !value.is_empty())
        else {
            return Ok(None);
        };
        Self::new(PathBuf::from(path)).map(Some)
    }

    pub fn record(&self, record: &PolicyDecisionRecord) -> Result<()> {
        let _guard = self.lock.lock().map_err(|_| {
            BitrouterError::internal("policy decision recorder lock poisoned".to_string())
        })?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| {
                BitrouterError::internal(format!(
                    "policy decision jsonl append {}: {e}",
                    self.path.display()
                ))
            })?;
        serde_json::to_writer(&mut file, record).map_err(|e| {
            BitrouterError::internal(format!("policy decision jsonl serialize: {e}"))
        })?;
        file.write_all(b"\n")
            .map_err(|e| BitrouterError::internal(format!("policy decision jsonl write: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> PolicyDecisionRecord {
        PolicyDecisionRecord {
            captured_at: None,
            request_id: Some("request-1".to_string()),
            input_model: "inbound".to_string(),
            key_strategy: "agent_trace".to_string(),
            request_key: "agent_trace/v1|tool_followup|normal".to_string(),
            ledger_key: Some("agent_trace/v1|tool_followup|normal".to_string()),
            policy: None,
            policy_digest: None,
            preset_variant: None,
            baseline_tier: Some("capable".to_string()),
            legacy_fingerprint: "after_read_file".to_string(),
            workflow_state: "tool_followup".to_string(),
            workflow_identity: WorkflowIdentity::default(),
            static_tier: Some("capable".to_string()),
            static_model: Some("vendor/capable".to_string()),
            selected_tier: Some("cheap".to_string()),
            selected_model: Some("vendor/cheap".to_string()),
            continuation_proposed_tier: None,
            continuation_proposed_model: None,
            continuation_adjustment: None,
            predicted_role: None,
            predicted_action: None,
            prediction_confidence_ppm: None,
            prediction_reason_codes: Vec::new(),
            observed_route_projection: None,
            trajectory_episode_id: None,
            trajectory_sequence: None,
            trajectory_completeness: None,
            trajectory_health_digest: None,
            candidate_tier: None,
            progress_clause_ids: Vec::new(),
            reason: "static_table".to_string(),
            pinned: false,
            request_qualified: true,
            semantic_successes: 1,
            semantic_success_threshold: 0,
            locked: false,
            trialed: false,
        }
    }

    #[test]
    fn decision_records_emit_trace_names_and_read_legacy_workflow_names() {
        let value = serde_json::to_value(record()).unwrap();
        assert_eq!(value["trace_state"], "tool_followup");
        assert!(value.get("workflow_state").is_none());
        assert!(value.get("trace_identity").is_some());
        assert!(value.get("workflow_identity").is_none());

        let mut legacy = serde_json::to_value(record()).unwrap();
        let object = legacy.as_object_mut().unwrap();
        let state = object.remove("trace_state").unwrap();
        let identity = object.remove("trace_identity").unwrap();
        object.insert("workflow_state".to_string(), state);
        object.insert("workflow_identity".to_string(), identity);
        let parsed: PolicyDecisionRecord = serde_json::from_value(legacy).unwrap();
        assert_eq!(parsed.workflow_state, "tool_followup");
        assert_eq!(parsed.workflow_identity, WorkflowIdentity::default());
    }

    #[test]
    fn decision_record_reads_legacy_jsonl_without_predictive_fields() {
        let legacy = r#"{
            "input_model":"inbound",
            "key_strategy":"agent_trace",
            "request_key":"agent_trace/v1|tool_followup|normal",
            "legacy_fingerprint":"after_read_file",
            "trace_state":"tool_followup",
            "trace_identity":{"role":"unknown","context_epoch":0,"transition":"none","fingerprint":"","source":"","confidence":"none"},
            "reason":"static_table",
            "pinned":false,
            "locked":false,
            "trialed":false
        }"#;

        let parsed: PolicyDecisionRecord = serde_json::from_str(legacy).unwrap();

        assert_eq!(parsed.predicted_role, None);
        assert_eq!(parsed.predicted_action, None);
        assert_eq!(parsed.prediction_confidence_ppm, None);
        assert_eq!(parsed.prediction_reason_codes, Vec::<String>::new());
        assert_eq!(parsed.observed_route_projection, None);
    }

    #[test]
    fn summary_emits_by_trace_state_and_reads_legacy_workflow_summary() {
        let summary = PolicyDecisionSummary::from_records(&[record()]);
        assert_eq!(summary.by_workflow_state["tool_followup"], 1);
        let value = serde_json::to_value(&summary).unwrap();
        assert!(value.get("by_trace_state").is_some());
        assert!(value.get("by_workflow_state").is_none());

        let mut legacy = serde_json::to_value(&summary).unwrap();
        let object = legacy.as_object_mut().unwrap();
        let states = object.remove("by_trace_state").unwrap();
        object.insert("by_workflow_state".to_string(), states);
        let parsed: PolicyDecisionSummary = serde_json::from_value(legacy).unwrap();
        assert_eq!(parsed.by_workflow_state["tool_followup"], 1);
    }

    #[test]
    fn summary_reads_old_json_without_predictive_dimensions() {
        let legacy = r#"{
            "total":1,
            "routed_count":1,
            "pinned_count":0,
            "locked_count":0,
            "trialed_count":0,
            "by_selected_tier":{"cheap":1},
            "by_selected_model":{"vendor/cheap":1},
            "static_tier_replaced_count":0,
            "static_model_replaced_count":0,
            "by_tier_transition":{"cheap->cheap":1},
            "by_model_transition":{"vendor/cheap->vendor/cheap":1},
            "replacement_by_reason":{},
            "by_reason":{"static_table":1},
            "by_trace_state":{"tool_followup":1},
            "by_agent_role":{"unknown":1},
            "by_context_epoch":{"0":1}
        }"#;

        let parsed: PolicyDecisionSummary = serde_json::from_str(legacy).unwrap();

        assert_eq!(parsed.by_predicted_role, BTreeMap::new());
        assert_eq!(parsed.by_predicted_action, BTreeMap::new());
    }

    #[test]
    fn summary_counts_predictions_and_selected_model_exposure() {
        let mut predicted = record();
        predicted.predicted_role = Some("implement".to_string());
        predicted.predicted_action = Some("mutate".to_string());
        predicted.selected_model = Some("vendor/cheap".to_string());

        let mut unpredicted = record();
        unpredicted.selected_model = Some("vendor/flagship".to_string());

        let summary = PolicyDecisionSummary::from_records(&[predicted, unpredicted]);

        assert_eq!(
            summary.by_predicted_role,
            BTreeMap::from([("implement".to_string(), 1)])
        );
        assert_eq!(
            summary.by_predicted_action,
            BTreeMap::from([("mutate".to_string(), 1)])
        );
        assert_eq!(
            summary.by_selected_model,
            BTreeMap::from([
                ("vendor/cheap".to_string(), 1),
                ("vendor/flagship".to_string(), 1),
            ])
        );
    }
}
