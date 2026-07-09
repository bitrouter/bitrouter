use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use bitrouter_sdk::{BitrouterError, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

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
    pub legacy_fingerprint: String,
    pub workflow_state: String,
    #[serde(default)]
    pub static_tier: Option<String>,
    #[serde(default)]
    pub selected_tier: Option<String>,
    #[serde(default)]
    pub selected_model: Option<String>,
    pub reason: String,
    pub pinned: bool,
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
    pub by_reason: BTreeMap<String, usize>,
    pub by_workflow_state: BTreeMap<String, usize>,
}

pub struct PolicyDecisionJsonlRecorder {
    path: PathBuf,
    lock: Mutex<()>,
}

impl PolicyDecisionRecord {
    pub fn now(
        request_id: Option<String>,
        input_model: impl Into<String>,
        key_strategy: impl Into<String>,
        request_key: impl Into<String>,
        legacy_fingerprint: impl Into<String>,
        workflow_state: impl Into<String>,
        static_tier: Option<String>,
        selected_tier: Option<String>,
        selected_model: Option<String>,
        reason: impl Into<String>,
        pinned: bool,
        locked: bool,
        trialed: bool,
    ) -> Self {
        Self {
            captured_at: Some(Utc::now().to_rfc3339()),
            request_id,
            input_model: input_model.into(),
            key_strategy: key_strategy.into(),
            request_key: request_key.into(),
            legacy_fingerprint: legacy_fingerprint.into(),
            workflow_state: workflow_state.into(),
            static_tier,
            selected_tier,
            selected_model,
            reason: reason.into(),
            pinned,
            locked,
            trialed,
        }
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
            if let Some(model) = record.selected_model.as_deref() {
                *summary
                    .by_selected_model
                    .entry(model.to_string())
                    .or_insert(0) += 1;
            }
            *summary.by_reason.entry(record.reason.clone()).or_insert(0) += 1;
            *summary
                .by_workflow_state
                .entry(record.workflow_state.clone())
                .or_insert(0) += 1;
        }
        summary
    }
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
