use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::optimization::exploration::{OptimizationGate, RouteExploration};

pub(crate) const HISTORY_OPTIMIZER_ID: &str = "bitrouter-history-optimizer";
pub(crate) const HISTORY_OPTIMIZER_VERSION: u32 = 1;

pub(crate) fn treatment_context_digest(
    policy_name: &str,
    request_key: &str,
    champion_tier: &str,
    challenger_tier: &str,
    challenger_exposure_ppm: u32,
    gate: &OptimizationGate,
) -> Result<String> {
    #[derive(Serialize)]
    struct TreatmentContext<'a> {
        policy_name: &'a str,
        request_key: &'a str,
        champion_tier: &'a str,
        challenger_tier: &'a str,
        challenger_exposure_ppm: u32,
        gate: &'a OptimizationGate,
    }
    canonical_digest(&TreatmentContext {
        policy_name,
        request_key,
        champion_tier,
        challenger_tier,
        challenger_exposure_ppm,
        gate,
    })
}

pub(crate) fn experiment_id(
    parent_policy_digest: &str,
    treatment_context_digest: &str,
) -> Result<String> {
    #[derive(Serialize)]
    struct ExperimentIdentity<'a> {
        domain: &'a str,
        parent_policy_digest: &'a str,
        treatment_context_digest: &'a str,
    }
    canonical_digest(&ExperimentIdentity {
        domain: "bitrouter.history-optimizer.experiment.v1",
        parent_policy_digest,
        treatment_context_digest,
    })
}

pub(crate) fn explore_compiler_config_digest(
    policy_name: &str,
    challenger_tier: &str,
    challenger_exposure_ppm: u32,
    gate: &OptimizationGate,
) -> Result<String> {
    #[derive(Serialize)]
    struct CompilerConfig<'a> {
        id: &'a str,
        version: u32,
        options: CompilerOptions<'a>,
    }
    #[derive(Serialize)]
    struct CompilerOptions<'a> {
        policy: &'a str,
        candidate_tier: Option<&'a str>,
        challenger_exposure_ppm: u32,
        minimum_tasks_per_arm: u32,
        maximum_challenger_tasks: u32,
        minimum_pass_rate_ppm: u32,
        evaluator_config_digest: Option<&'a str>,
    }
    canonical_digest(&CompilerConfig {
        id: HISTORY_OPTIMIZER_ID,
        version: HISTORY_OPTIMIZER_VERSION,
        options: CompilerOptions {
            policy: policy_name,
            candidate_tier: Some(challenger_tier),
            challenger_exposure_ppm,
            minimum_tasks_per_arm: gate.minimum_tasks_per_arm,
            maximum_challenger_tasks: gate.maximum_challenger_tasks,
            minimum_pass_rate_ppm: gate.minimum_pass_rate_ppm,
            evaluator_config_digest: gate.evaluator_config_digest.as_deref(),
        },
    })
}

pub(crate) fn retreat_compiler_config_digest(exploration: &RouteExploration) -> Result<String> {
    #[derive(Serialize)]
    struct ActiveCompilerConfig<'a> {
        id: &'a str,
        version: u32,
        exploration: &'a RouteExploration,
    }
    canonical_digest(&ActiveCompilerConfig {
        id: HISTORY_OPTIMIZER_ID,
        version: HISTORY_OPTIMIZER_VERSION,
        exploration,
    })
}

pub(crate) fn canonical_digest<T: Serialize>(value: &T) -> Result<String> {
    let canonical = serde_json::to_vec(value).context("serializing optimizer digest input")?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(canonical))))
}
