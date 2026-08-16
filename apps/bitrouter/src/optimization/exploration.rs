//! Signed, bounded routing-experiment state and deterministic assignment.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::eval::types::{EvalExperimentRef, ExperimentArm, ExperimentAssignmentUnit};
use crate::workflow_state::ir::WorkflowIdentity;

const ASSIGNMENT_DOMAIN: &str = "bitrouter.route-exploration.assignment.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptimizationGate {
    pub minimum_tasks_per_arm: u32,
    pub maximum_challenger_tasks: u32,
    pub minimum_pass_rate_ppm: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluator_config_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteExploration {
    pub experiment_id: String,
    pub target_request_key: String,
    pub champion_tier: String,
    pub challenger_tier: String,
    pub challenger_exposure_ppm: u32,
    pub gate: OptimizationGate,
}

impl RouteExploration {
    /// Assign one stable task or episode to the control or challenger arm.
    /// Missing stable identity deliberately produces no evidence and lets the
    /// caller retain the champion route.
    pub fn assignment(&self, identity: &WorkflowIdentity) -> Result<Option<EvalExperimentRef>> {
        let (assignment_unit, assignment_id) = match (
            identity.benchmark_run_id.as_deref(),
            identity.trial_id.as_deref(),
            identity.parent_session_id.as_deref(),
        ) {
            (Some(run), Some(trial), _) if !run.is_empty() && !trial.is_empty() => {
                (ExperimentAssignmentUnit::Task, format!("{run}\0{trial}"))
            }
            (_, None, Some(parent)) if !parent.is_empty() => {
                (ExperimentAssignmentUnit::Episode, parent.to_owned())
            }
            _ => return Ok(None),
        };
        let assignment_id_digest = digest(&assignment_id);
        let bucket_digest = Sha256::digest(
            format!(
                "{ASSIGNMENT_DOMAIN}\0{}\0{}\0{}",
                self.experiment_id,
                assignment_unit_name(assignment_unit),
                assignment_id
            )
            .as_bytes(),
        );
        let mut bucket_bytes = [0_u8; 8];
        bucket_bytes.copy_from_slice(&bucket_digest[..8]);
        let bucket = u64::from_be_bytes(bucket_bytes) % 1_000_000;
        Ok(Some(EvalExperimentRef {
            experiment_id: self.experiment_id.clone(),
            arm: if bucket < u64::from(self.challenger_exposure_ppm) {
                ExperimentArm::Challenger
            } else {
                ExperimentArm::Control
            },
            assignment_unit,
            assignment_id_digest,
            challenger_propensity_ppm: self.challenger_exposure_ppm,
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteRejection {
    pub experiment_id: String,
    pub treatment_context_digest: String,
    pub evidence_root: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyOptimizationState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<RouteExploration>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejections: Vec<RouteRejection>,
}

impl PolicyOptimizationState {
    pub fn is_empty(&self) -> bool {
        self.active.is_none() && self.rejections.is_empty()
    }
}

fn assignment_unit_name(unit: ExperimentAssignmentUnit) -> &'static str {
    match unit {
        ExperimentAssignmentUnit::Task => "task",
        ExperimentAssignmentUnit::Episode => "episode",
    }
}

fn digest(value: &str) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(value.as_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXPERIMENT_ID: &str =
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn experiment() -> RouteExploration {
        RouteExploration {
            experiment_id: EXPERIMENT_ID.into(),
            target_request_key: "agent_trace/v2|edit|normal".into(),
            champion_tier: "strong".into(),
            challenger_tier: "economy".into(),
            challenger_exposure_ppm: 700_000,
            gate: OptimizationGate {
                minimum_tasks_per_arm: 3,
                maximum_challenger_tasks: 20,
                minimum_pass_rate_ppm: 900_000,
                evaluator_config_digest: None,
            },
        }
    }

    #[test]
    fn assignment_is_stable_for_a_task_and_uses_a_redacted_digest() -> Result<()> {
        let same_task_request_a = WorkflowIdentity {
            benchmark_run_id: Some("run-1".into()),
            trial_id: Some("trial-1".into()),
            ..WorkflowIdentity::default()
        };
        let same_task_request_b = same_task_request_a.clone();
        let assignment_a = experiment()
            .assignment(&same_task_request_a)?
            .ok_or_else(|| {
                anyhow::anyhow!("a benchmark run and trial must provide a stable task identity")
            })?;
        let assignment_b = experiment()
            .assignment(&same_task_request_b)?
            .ok_or_else(|| {
                anyhow::anyhow!("a benchmark run and trial must provide a stable task identity")
            })?;

        // SHA-256 of `run-1\\0trial-1`, with a hand-derived assignment bucket
        // of 669455 at 700000 ppm, is challenger.
        assert_eq!(assignment_a.arm, ExperimentArm::Challenger);
        assert_eq!(assignment_a, assignment_b);
        assert_eq!(assignment_a.assignment_unit, ExperimentAssignmentUnit::Task);
        assert_eq!(
            assignment_a.assignment_id_digest,
            "sha256:4a5ea7dc2692681fb41b00cf50a34656ee45727cfdabd1d7a77ef8221df1dcc5"
        );
        Ok(())
    }

    #[test]
    fn assignment_falls_back_to_control_without_a_stable_unit() -> Result<()> {
        let identity_without_trial_or_parent = WorkflowIdentity {
            benchmark_run_id: Some("run-1".into()),
            ..WorkflowIdentity::default()
        };
        assert!(
            experiment()
                .assignment(&identity_without_trial_or_parent)?
                .is_none()
        );
        Ok(())
    }
}
