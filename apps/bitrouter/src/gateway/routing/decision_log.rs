//! Append-only model-selection diagnostics with opaque invocation identity.
use crate::gateway::error::{BitrouterError, Result};
use bitrouter_ai::{eval::RouteDecisionMeasurement, types::ReasoningEffort};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

pub const POLICY_DECISION_JSONL_ENV: &str = "BITROUTER_POLICY_DECISION_JSONL";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyDecisionRecord {
    #[serde(default)]
    pub captured_at: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
    /// Domain-separated commitment to the ingress HTTP request identity.
    /// External evidence joins the ingress request through this commitment
    /// without persisting the raw request identity.
    #[serde(default)]
    pub ingress_request_id_sha256: Option<String>,
    pub input_model: String,
    #[serde(default)]
    pub input_effort: Option<ReasoningEffort>,
    /// Primary canonical policy projection considered before sparse fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_projection: Option<String>,
    pub request_key: String,
    /// Named model policy that made the decision.
    #[serde(default)]
    pub policy: Option<String>,
    /// Semantic digest of the active policy snapshot.
    #[serde(default)]
    pub policy_digest: Option<String>,
    /// Effective preset variant, when known.
    #[serde(default)]
    pub preset_variant: Option<String>,
    /// Strong/default comparison tier used by evaluators.
    #[serde(default)]
    pub baseline_tier: Option<String>,
    #[serde(default)]
    pub baseline_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub static_tier: Option<String>,
    #[serde(default)]
    pub static_model: Option<String>,
    #[serde(default)]
    pub static_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub selected_tier: Option<String>,
    #[serde(default)]
    pub selected_model: Option<String>,
    #[serde(default)]
    pub selected_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub continuation_proposed_tier: Option<String>,
    #[serde(default)]
    pub continuation_proposed_model: Option<String>,
    #[serde(default)]
    pub continuation_proposed_effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub continuation_adjustment: Option<String>,
    #[serde(default)]
    pub predicted_role: Option<String>,
    #[serde(default)]
    pub predicted_task_family: Option<String>,
    #[serde(default)]
    pub prediction_confidence_ppm: Option<u32>,
    #[serde(default)]
    pub task_family_confidence_ppm: Option<u32>,
    #[serde(default)]
    pub predictor_contract_digest: Option<String>,
    #[serde(default)]
    pub prediction_confidence_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route_measurement: Option<RouteDecisionMeasurement>,
    pub reason: String,
}

pub fn ingress_request_id_sha256(request_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"bitrouter.ingress-request-id.v1\0");
    digest.update(request_id.as_bytes());
    format!("sha256:{}", hex::encode(digest.finalize()))
}

pub struct PolicyDecisionJsonlRecorder {
    path: PathBuf,
    lock: Mutex<()>,
}
impl PolicyDecisionRecord {
    pub fn captured_now(mut self) -> Self {
        self.captured_at = Some(Utc::now().to_rfc3339());
        self
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
