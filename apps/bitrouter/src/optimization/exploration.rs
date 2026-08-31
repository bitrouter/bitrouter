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
        let benchmark_identity = identity
            .benchmark_run_id
            .as_deref()
            .filter(|run| !run.is_empty())
            .zip(
                identity
                    .trial_id
                    .as_deref()
                    .filter(|trial| !trial.is_empty()),
            )
            .map(|(run, trial)| (ExperimentAssignmentUnit::Task, format!("{run}\0{trial}")));
        let parent_identity = identity
            .parent_session_id
            .as_deref()
            .filter(|parent| !parent.is_empty())
            .map(|parent| (ExperimentAssignmentUnit::Episode, parent.to_owned()));
        let Some((assignment_unit, assignment_id)) = benchmark_identity.or(parent_identity) else {
            return Ok(None);
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
    /// Canonical route rejected by current optimizer locks. Older locks remain
    /// readable without it, but cannot authorize a route-less certificate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_request_key: Option<String>,
    /// Absent only in legacy locks. Because their rejected treatment cannot be
    /// reconstructed safely, cold-start selection treats such a ledger as no
    /// new opportunity until an operator changes or replaces the policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub treatment_context_digest: Option<String>,
    /// Complete rejected treatment for exact validation of current optimizer
    /// locks. Legacy rejections omit it and remain readable but fail closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub treatment: Option<RouteExploration>,
    /// Policy digest from which the rejected experiment identity was derived.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experiment_parent_digest: Option<String>,
    /// Active exploration policy that the Retreat successor replaced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_policy_digest: Option<String>,
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
            target_request_key: "agent_route/v1|unknown|implement|normal".into(),
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

    #[test]
    fn partial_benchmark_identity_falls_back_to_a_non_empty_parent_episode() -> Result<()> {
        for benchmark_run_id in [None, Some(String::new())] {
            let identity = WorkflowIdentity {
                benchmark_run_id,
                trial_id: Some("orphan-trial".into()),
                parent_session_id: Some("parent-session".into()),
                ..WorkflowIdentity::default()
            };

            let assignment = experiment()
                .assignment(&identity)?
                .ok_or_else(|| anyhow::anyhow!("the parent episode identity must be usable"))?;

            assert_eq!(
                assignment.assignment_unit,
                ExperimentAssignmentUnit::Episode
            );
            assert_eq!(
                assignment.assignment_id_digest,
                "sha256:a1bdc27ac7582a7459555e91884123666722003a729ba3fbfc29676fc4f724c7"
            );
        }

        let unusable = WorkflowIdentity {
            trial_id: Some("orphan-trial".into()),
            parent_session_id: Some(String::new()),
            ..WorkflowIdentity::default()
        };
        assert!(experiment().assignment(&unusable)?.is_none());
        Ok(())
    }

    #[test]
    fn rejection_round_trips_full_treatment_and_policy_provenance() -> Result<()> {
        let raw = r#"{
            "experiment_id":"sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "target_request_key":"agent_route/v1|unknown|implement|normal",
            "treatment_context_digest":"sha256:123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0",
            "treatment":{
                "experiment_id":"sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "target_request_key":"agent_route/v1|unknown|implement|normal",
                "champion_tier":"strong",
                "challenger_tier":"economy",
                "challenger_exposure_ppm":100000,
                "gate":{
                    "minimum_tasks_per_arm":3,
                    "maximum_challenger_tasks":20,
                    "minimum_pass_rate_ppm":900000
                }
            },
            "experiment_parent_digest":"sha256:23456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef01",
            "source_policy_digest":"sha256:3456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef012",
            "evidence_root":"sha256:456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123",
            "reason":"challenger hard violation"
        }"#;

        let parsed = serde_json::from_str::<RouteRejection>(raw);
        assert!(
            parsed.is_ok(),
            "full rejection provenance must deserialize: {:?}",
            parsed.err()
        );
        let rejection = serde_json::from_str::<RouteRejection>(raw)?;
        let rendered = serde_json::to_string(&rejection)?;

        assert_eq!(
            serde_json::from_str::<RouteRejection>(&rendered)?,
            rejection
        );
        Ok(())
    }
}
