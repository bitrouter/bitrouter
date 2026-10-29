//! Persisted policy allocation contracts and deterministic serving draws.

use crate::storage::evolution::digest;
use anyhow::{Result, ensure};
use rand::{RngExt, SeedableRng, rngs::StdRng};
use serde::{Deserialize, Serialize};

pub const LEARNER_VERSION: &str = "checkpoint-normal-inverse-gamma-ts-v3";

pub const PPM: u32 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arm {
    Baseline,
    Challenger,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeanPrior {
    pub mean: f64,
    /// Prior precision expressed relative to one likelihood observation.
    /// This is never included in the reported observed-family count.
    pub strength: f64,
    /// Prior expected observation variance. The inverse-gamma prior has fixed
    /// shape 2 and scale equal to this value; variance is learned from observed
    /// family outcomes rather than treated as known measurement noise.
    pub observation_variance: f64,
}

impl MeanPrior {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            self.mean.is_finite()
                && self.strength.is_finite()
                && self.strength > 0.0
                && self.observation_variance.is_finite()
                && self.observation_variance > 0.0,
            "normal prior parameters must be finite with positive precision and variance"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeanPosterior {
    pub mean: f64,
    pub variance: f64,
    pub degrees_of_freedom: f64,
    pub student_scale: f64,
    pub observation_variance_mean: f64,
    pub observed_families: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArmPrior {
    pub quality: MeanPrior,
    /// ln(1 + total session cost in micro-USD), not per-token price.
    pub log_cost: MeanPrior,
    /// ln(1 + session active execution milliseconds).
    pub log_latency: MeanPrior,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BanditConfig {
    pub prior_version: String,
    pub baseline: ArmPrior,
    pub challenger: ArmPrior,
    pub minimum_quality_ppm: u32,
    pub noninferiority_margin_ppm: u32,
    pub maximum_latency_increase_ppm: u32,
    pub promotion_probability_ppm: u32,
    pub minimum_families_per_arm: usize,
    pub initial_exposure_ppm: u32,
    pub maximum_exposure_ppm: u32,
    pub minimum_exposure_ppm: u32,
    pub maximum_pending_challenger: usize,
    pub maximum_challenger_sessions: usize,
    pub posterior_draws: usize,
    pub recent_guard_families: usize,
    pub recent_guard_minimum_families: usize,
}

impl Default for BanditConfig {
    fn default() -> Self {
        let prior = ArmPrior {
            quality: MeanPrior {
                mean: 0.8,
                strength: 1.0,
                observation_variance: 0.25,
            },
            log_cost: MeanPrior {
                mean: 1_000_000_f64.ln_1p(),
                strength: 1.0,
                observation_variance: 4.0,
            },
            log_latency: MeanPrior {
                mean: 60_000_f64.ln_1p(),
                strength: 1.0,
                observation_variance: 4.0,
            },
        };
        Self {
            prior_version: "weak-normal-inverse-gamma-v1".into(),
            baseline: prior.clone(),
            challenger: prior,
            minimum_quality_ppm: 800_000,
            noninferiority_margin_ppm: 50_000,
            maximum_latency_increase_ppm: 100_000,
            promotion_probability_ppm: 950_000,
            minimum_families_per_arm: 20,
            initial_exposure_ppm: 100_000,
            maximum_exposure_ppm: 500_000,
            minimum_exposure_ppm: 20_000,
            maximum_pending_challenger: 4,
            maximum_challenger_sessions: 200,
            posterior_draws: 8192,
            recent_guard_families: 32,
            recent_guard_minimum_families: 8,
        }
    }
}

impl BanditConfig {
    pub fn digest(&self) -> Result<String> {
        digest(&(LEARNER_VERSION, self))
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.prior_version.trim().is_empty(),
            "prior provenance is required"
        );
        for prior in [&self.baseline, &self.challenger] {
            prior.quality.validate()?;
            prior.log_cost.validate()?;
            prior.log_latency.validate()?;
            ensure!(
                (0.0..=1.0).contains(&prior.quality.mean),
                "quality prior mean is outside [0, 1]"
            );
        }
        ensure!(
            self.minimum_quality_ppm <= PPM
                && self.noninferiority_margin_ppm <= PPM
                && self.promotion_probability_ppm > PPM / 2
                && self.promotion_probability_ppm < PPM,
            "invalid quality or promotion threshold"
        );
        ensure!(
            0 < self.minimum_exposure_ppm
                && self.minimum_exposure_ppm <= self.initial_exposure_ppm
                && self.initial_exposure_ppm <= self.maximum_exposure_ppm
                && self.maximum_exposure_ppm < PPM,
            "exposure must retain both baseline and trial coverage"
        );
        ensure!(
            self.minimum_families_per_arm > 0
                && self.maximum_pending_challenger > 0
                && self.maximum_challenger_sessions > 0
                && self.recent_guard_minimum_families > 0
                && self.recent_guard_minimum_families <= self.recent_guard_families
                && (128..=1_000_000).contains(&self.posterior_draws),
            "invalid observation or simulation limits"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArmPosterior {
    pub quality: MeanPosterior,
    pub log_cost: MeanPosterior,
    pub log_latency: MeanPosterior,
    pub assigned_sessions: usize,
    pub incomplete_sessions: usize,
    pub severe_sessions: usize,
    pub arithmetic_mean_cost_micro_usd: Option<f64>,
    pub recent_quality: MeanPosterior,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recommendation {
    Explore,
    Hold,
    Promote,
    Rollback,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchPlan {
    pub learner_version: String,
    pub plan_id: String,
    pub evidence_digest: String,
    pub measurement_contract: String,
    pub config_digest: String,
    pub seed: u64,
    pub challenger_propensity_ppm: u32,
    /// Monte Carlo estimate under the declared working posterior model.
    pub joint_benefit_probability_ppm: u32,
    /// Lower bound for Monte Carlo sampling error ONLY, not label/model error.
    pub monte_carlo_lower_ppm: u32,
    pub recent_quality_harm_probability_ppm: u32,
    pub baseline: ArmPosterior,
    pub challenger: ArmPosterior,
    pub excluded_mixed_families: usize,
    pub recommendation: Recommendation,
    pub reason: String,
}

/// The service persists this outcome once. Reconnects read the stored arm;
/// they never call this again for an already assigned session.
pub fn draw_assignment(challenger_propensity_ppm: u32, seed: u64) -> Result<Arm> {
    ensure!(
        challenger_propensity_ppm <= PPM,
        "invalid assignment probability"
    );
    let mut rng = StdRng::seed_from_u64(seed);
    Ok(if rng.random_range(0..PPM) < challenger_propensity_ppm {
        Arm::Challenger
    } else {
        Arm::Baseline
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bandit_json_round_trip_preserves_review_and_persistence_fingerprints() -> Result<()> {
        let config = BanditConfig::default();
        let restored: BanditConfig = serde_json::from_str(&serde_json::to_string(&config)?)?;
        assert_eq!(
            config.baseline.log_cost.mean.to_bits(),
            restored.baseline.log_cost.mean.to_bits()
        );
        assert_eq!(
            config.baseline.log_latency.mean.to_bits(),
            restored.baseline.log_latency.mean.to_bits()
        );
        assert_eq!(config.digest()?, restored.digest()?);
        Ok(())
    }
}
