//! Batched, baseline-protected Thompson sampling with replaceable observations.
//!
//! Continuous quality and log-resource observations use a normal likelihood and
//! a normal-inverse-gamma prior over the mean and unknown observation variance.
//! Mean sampling uses its marginal Student-t posterior. This is a working
//! statistical model, not a distribution-free quality guarantee.
//! Unknown observations do not become fractional Bernoulli successes.
//! Reference: <https://proceedings.mlr.press/v28/agrawal13.html>
//! Conjugate update: <https://www.cs.ubc.ca/~murphyk/Papers/bayesGauss.pdf>

use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use rand::{RngExt, SeedableRng, rngs::StdRng};
use rand_distr::{Distribution, StudentT};
use serde::{Deserialize, Serialize};

use super::rubric::{PPM, digest};

pub const LEARNER_VERSION: &str = "checkpoint-normal-inverse-gamma-ts-v3";

/// Common numerical draws for an experiment. Source revisions still belong to
/// the plan's evidence digest and publication fences; they must not reroll an
/// unchanged posterior across a promotion or withdrawal threshold. This seed
/// does not draw native-session assignments, which have independent randomness.
pub fn trial_seed(experiment_id: &str) -> Result<u64> {
    ensure!(!experiment_id.is_empty(), "experiment identity is required");
    let value = digest(&(LEARNER_VERSION, experiment_id))?;
    Ok(u64::from_str_radix(
        value.get(..16).context("invalid trial seed digest")?,
        16,
    )?)
}

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
    fn validate(&self) -> Result<()> {
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

    fn posterior(&self, samples: &[f64]) -> Result<MeanPosterior> {
        self.validate()?;
        ensure!(
            samples.iter().all(|v| v.is_finite()),
            "nonfinite observation"
        );
        let n = samples.len() as f64;
        let precision = self.strength + n;
        let sample_mean = if samples.is_empty() {
            self.mean
        } else {
            samples.iter().sum::<f64>() / n
        };
        let scatter = samples
            .iter()
            .map(|value| (value - sample_mean).powi(2))
            .sum::<f64>();
        let shape = 2.0 + n / 2.0;
        let scale = self.observation_variance
            + scatter / 2.0
            + self.strength * n * (sample_mean - self.mean).powi(2) / (2.0 * precision);
        Ok(MeanPosterior {
            mean: (self.strength * self.mean + n * sample_mean) / precision,
            variance: scale / ((shape - 1.0) * precision),
            degrees_of_freedom: 2.0 * shape,
            student_scale: (scale / (shape * precision)).sqrt(),
            observation_variance_mean: scale / (shape - 1.0),
            observed_families: samples.len(),
        })
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

impl MeanPosterior {
    fn sample(&self, rng: &mut StdRng) -> Result<f64> {
        Ok(self.mean + self.student_scale * StudentT::new(self.degrees_of_freedom)?.sample(rng))
    }
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
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub session_key: String,
    /// Monotone assignment order, fixed before observing an outcome.
    pub assignment_sequence: u64,
    pub family_id: String,
    /// Includes assessment and resource revisions, not just a checkpoint ID.
    pub revision: String,
    pub measurement_contract: String,
    pub arm: Arm,
    /// None includes pending, unknown, stale and retracted quality.
    pub quality: Option<f64>,
    pub total_cost_micro_usd: Option<u64>,
    pub latency_ms: Option<u64>,
    pub severe_violation: bool,
}

/// A caller replaces this map from current effective state before planning.
/// Upsert by session means that one session can never add a second observation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EffectiveObservations {
    pub sessions: BTreeMap<String, Observation>,
}

impl EffectiveObservations {
    pub fn replace(&mut self, observation: Observation) -> Result<()> {
        ensure!(
            !observation.session_key.is_empty()
                && !observation.family_id.is_empty()
                && !observation.revision.is_empty()
                && !observation.measurement_contract.is_empty(),
            "observation identity and measurement contract are required"
        );
        ensure!(
            observation
                .quality
                .is_none_or(|q| q.is_finite() && (0.0..=1.0).contains(&q)),
            "quality observation is outside [0, 1]"
        );
        self.sessions
            .insert(observation.session_key.clone(), observation);
        Ok(())
    }

    pub fn remove(&mut self, session_key: &str) {
        self.sessions.remove(session_key);
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

pub const MONITOR_VERSION: &str = "checkpoint-adoption-quality-monitor-v1";

/// Descriptive quality monitoring after adoption. These observations cannot
/// establish a new cost benefit or serve as randomized challenger-arm samples.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitoringPlan {
    pub version: String,
    pub evidence_digest: String,
    pub config_digest: String,
    pub measurement_contract: String,
    pub sessions: usize,
    pub recent_families: usize,
    pub incomplete_recent_families: usize,
    pub recent_quality: MeanPosterior,
    pub severe_sessions: usize,
    pub probability_below_floor_ppm: u32,
    /// Numerical integration error only, not a guarantee about judge accuracy.
    pub monte_carlo_lower_ppm: u32,
    pub rollback: bool,
    pub reason: String,
}

pub fn monitor(
    config: &BanditConfig,
    data: &EffectiveObservations,
    contract: &str,
    seed: u64,
) -> Result<MonitoringPlan> {
    config.validate()?;
    ensure!(
        !contract.is_empty(),
        "monitoring measurement contract is required"
    );
    let mut families: BTreeMap<&str, Vec<&Observation>> = BTreeMap::new();
    for (key, observation) in &data.sessions {
        ensure!(
            key == &observation.session_key && observation.arm == Arm::Challenger,
            "invalid adoption monitoring membership"
        );
        let mut validation = EffectiveObservations::default();
        validation.replace(observation.clone())?;
        families
            .entry(&observation.family_id)
            .or_default()
            .push(observation);
    }
    let mut ordered: Vec<_> = families.into_iter().collect();
    ordered.sort_by_key(|(id, members)| {
        (
            members
                .iter()
                .map(|o| o.assignment_sequence)
                .min()
                .unwrap_or(0),
            *id,
        )
    });
    // Choose the recent window before removing unavailable observations. New
    // missing feedback cannot silently be replaced by older positive samples.
    let recent: Vec<_> = ordered
        .iter()
        .rev()
        .take(config.recent_guard_families)
        .collect();
    let samples: Vec<f64> = recent
        .iter()
        .filter(|(_, members)| {
            members
                .iter()
                .all(|o| o.quality.is_some() && o.measurement_contract == contract)
        })
        .map(|(_, members)| {
            members.iter().filter_map(|o| o.quality).sum::<f64>() / members.len() as f64
        })
        .collect();
    let posterior = config.challenger.quality.posterior(&samples)?;
    let mut rng = StdRng::seed_from_u64(seed);
    let mut harmful = 0;
    for _ in 0..config.posterior_draws {
        harmful += usize::from(
            posterior.sample(&mut rng)? < f64::from(config.minimum_quality_ppm) / f64::from(PPM),
        );
    }
    let probability = harmful as f64 / config.posterior_draws as f64;
    let radius = (100_f64.ln() / (2.0 * config.posterior_draws as f64)).sqrt();
    let lower = (probability - radius).max(0.0);
    // A supported material violation is a veto, including a human correction
    // from another evaluator. It does not pool numeric quality across contracts.
    let severe = data
        .sessions
        .values()
        .filter(|o| o.severe_violation)
        .count();
    let rollback = severe > 0
        || (samples.len() >= config.recent_guard_minimum_families
            && lower >= f64::from(config.promotion_probability_ppm) / f64::from(PPM));
    let reason = if severe > 0 {
        "adopted_policy_recorded_severe_violation"
    } else if rollback {
        "adopted_policy_recent_quality_below_floor"
    } else if samples.len() < config.recent_guard_minimum_families {
        "adoption_monitoring_insufficient_feedback"
    } else if samples.len() != recent.len() {
        "adoption_monitoring_feedback_incomplete"
    } else {
        "adoption_monitoring_no_quality_alarm"
    };
    Ok(MonitoringPlan {
        version: MONITOR_VERSION.into(),
        evidence_digest: digest(data)?,
        config_digest: config.digest()?,
        measurement_contract: contract.into(),
        sessions: data.sessions.len(),
        recent_families: recent.len(),
        incomplete_recent_families: recent.len() - samples.len(),
        recent_quality: posterior,
        severe_sessions: severe,
        probability_below_floor_ppm: (probability * f64::from(PPM)).round() as u32,
        monte_carlo_lower_ppm: (lower * f64::from(PPM)).round() as u32,
        rollback,
        reason: reason.into(),
    })
}

fn summary(
    data: &EffectiveObservations,
    arm: Arm,
    contract: &str,
    prior: &ArmPrior,
    recent_guard_families: usize,
) -> Result<(ArmPosterior, usize)> {
    let mut families: BTreeMap<&str, Vec<&Observation>> = BTreeMap::new();
    for observation in data.sessions.values() {
        families
            .entry(&observation.family_id)
            .or_default()
            .push(observation);
    }
    let mut quality = Vec::new();
    let mut cost = Vec::new();
    let mut latency = Vec::new();
    let mut arithmetic_costs = Vec::new();
    let mut ordered_quality = Vec::new();
    let mut excluded = 0;
    for members in families.values() {
        if members.iter().any(|o| o.arm != members[0].arm) {
            excluded += 1;
            continue;
        }
        if members[0].arm != arm {
            continue;
        }
        // Keep one cluster contribution. An unknown/corrected member must not
        // be discarded to retain only favorable outcomes from its family.
        if members
            .iter()
            .any(|o| o.measurement_contract != contract || o.quality.is_none())
        {
            continue;
        }
        let n = members.len() as f64;
        quality.push(members.iter().filter_map(|o| o.quality).sum::<f64>() / n);
        ordered_quality.push((
            members
                .iter()
                .map(|o| o.assignment_sequence)
                .min()
                .unwrap_or(0),
            members.iter().filter_map(|o| o.quality).sum::<f64>() / n,
        ));
        if members.iter().all(|o| o.total_cost_micro_usd.is_some()) {
            let mean = members
                .iter()
                .filter_map(|o| o.total_cost_micro_usd)
                .map(|v| v as f64)
                .sum::<f64>()
                / n;
            arithmetic_costs.push(mean);
            cost.push(mean.ln_1p());
        }
        if members.iter().all(|o| o.latency_ms.is_some()) {
            latency.push(
                (members
                    .iter()
                    .filter_map(|o| o.latency_ms)
                    .map(|v| v as f64)
                    .sum::<f64>()
                    / n)
                    .ln_1p(),
            );
        }
    }
    ordered_quality.sort_by_key(|(sequence, _)| *sequence);
    let recent: Vec<_> = ordered_quality
        .iter()
        .rev()
        .take(recent_guard_families)
        .map(|(_, q)| *q)
        .collect();
    let assigned: Vec<_> = data.sessions.values().filter(|o| o.arm == arm).collect();
    Ok((
        ArmPosterior {
            quality: prior.quality.posterior(&quality)?,
            log_cost: prior.log_cost.posterior(&cost)?,
            log_latency: prior.log_latency.posterior(&latency)?,
            assigned_sessions: assigned.len(),
            incomplete_sessions: assigned
                .iter()
                .filter(|o| {
                    o.quality.is_none()
                        || o.measurement_contract != contract
                        || o.total_cost_micro_usd.is_none()
                        || o.latency_ms.is_none()
                })
                .count(),
            severe_sessions: assigned.iter().filter(|o| o.severe_violation).count(),
            arithmetic_mean_cost_micro_usd: (!arithmetic_costs.is_empty())
                .then(|| arithmetic_costs.iter().sum::<f64>() / arithmetic_costs.len() as f64),
            recent_quality: prior.quality.posterior(&recent)?,
        },
        excluded,
    ))
}

pub fn plan(
    config: &BanditConfig,
    data: &EffectiveObservations,
    contract: &str,
    previous_exposure_ppm: Option<u32>,
    seed: u64,
) -> Result<BatchPlan> {
    config.validate()?;
    ensure!(!contract.is_empty(), "measurement contract is required");
    for (key, observation) in &data.sessions {
        ensure!(
            key == &observation.session_key,
            "observation map key mismatch"
        );
        let mut validation = EffectiveObservations::default();
        validation.replace(observation.clone())?;
    }
    let (baseline, excluded) = summary(
        data,
        Arm::Baseline,
        contract,
        &config.baseline,
        config.recent_guard_families,
    )?;
    let (challenger, _) = summary(
        data,
        Arm::Challenger,
        contract,
        &config.challenger,
        config.recent_guard_families,
    )?;
    let mut rng = StdRng::seed_from_u64(seed);
    let mut favorable = 0;
    let mut harmful = 0;
    let mut recent_harmful = 0;
    for _ in 0..config.posterior_draws {
        let bq = baseline.quality.sample(&mut rng)?;
        let cq = challenger.quality.sample(&mut rng)?;
        let quality_ok = cq >= bq - f64::from(config.noninferiority_margin_ppm) / f64::from(PPM)
            && cq >= f64::from(config.minimum_quality_ppm) / f64::from(PPM);
        harmful += usize::from(!quality_ok);
        // Independent metric likelihoods are an explicit working approximation.
        // Sampling compares latent log means; it does not claim arithmetic-mean
        // dollar savings. Promotion also requires observed mean cost reduction.
        let cheaper = challenger.log_cost.sample(&mut rng)? < baseline.log_cost.sample(&mut rng)?;
        let latency_ok = challenger.log_latency.sample(&mut rng)?
            <= baseline.log_latency.sample(&mut rng)?
                + (f64::from(config.maximum_latency_increase_ppm) / f64::from(PPM)).ln_1p();
        favorable += usize::from(quality_ok && cheaper && latency_ok);
        recent_harmful += usize::from(
            challenger.recent_quality.sample(&mut rng)?
                < f64::from(config.minimum_quality_ppm) / f64::from(PPM),
        );
    }
    let probability = favorable as f64 / config.posterior_draws as f64;
    // Hoeffding bound for iid posterior draws, delta=0.01. It only quantifies
    // numerical integration error and must not be shown as outcome confidence.
    let mc_radius = (100_f64.ln() / (2.0 * config.posterior_draws as f64)).sqrt();
    let lower = (probability - mc_radius).max(0.0);
    let observed = |p: &ArmPosterior| {
        p.quality.observed_families >= config.minimum_families_per_arm
            && p.log_cost.observed_families >= config.minimum_families_per_arm
            && p.log_latency.observed_families >= config.minimum_families_per_arm
    };
    let all_complete =
        baseline.incomplete_sessions == 0 && challenger.incomplete_sessions == 0 && excluded == 0;
    let previous = previous_exposure_ppm
        .filter(|value| *value > 0)
        .unwrap_or(config.initial_exposure_ppm)
        .clamp(config.minimum_exposure_ppm, config.maximum_exposure_ppm);
    let probability_ppm = (probability * f64::from(PPM)).round() as u32;
    let mut exposure =
        probability_ppm.clamp(config.minimum_exposure_ppm, config.maximum_exposure_ppm);
    if !observed(&baseline) || !observed(&challenger) {
        // A resource prior on a different scale must not starve an unmeasured
        // arm after the other arm's first result. Keep bounded warm-up exposure
        // until both arms have independent metric evidence. Never restore a
        // previously reduced rate or bypass the quality/pending guards below.
        exposure = previous.min(config.initial_exposure_ppm);
    } else if !all_complete {
        exposure = exposure.min(previous);
    }
    let (recommendation, reason) = if challenger.severe_sessions > 0 {
        exposure = 0;
        (Recommendation::Rollback, "recorded_severe_violation")
    } else if challenger.recent_quality.observed_families >= config.recent_guard_minimum_families
        && recent_harmful as f64 / config.posterior_draws as f64 - mc_radius
            >= f64::from(config.promotion_probability_ppm) / f64::from(PPM)
    {
        // A guard need not wait for every pending/resource observation to
        // finish. This is a model-based protection rule, not a regret guarantee.
        exposure = 0;
        (Recommendation::Rollback, "recent_quality_below_floor")
    } else if observed(&baseline)
        && observed(&challenger)
        && all_complete
        && harmful as f64 / config.posterior_draws as f64 - mc_radius
            >= f64::from(config.promotion_probability_ppm) / f64::from(PPM)
    {
        exposure = 0;
        (Recommendation::Rollback, "posterior_quality_degradation")
    } else if observed(&baseline)
        && observed(&challenger)
        && all_complete
        && lower >= f64::from(config.promotion_probability_ppm) / f64::from(PPM)
        && challenger
            .arithmetic_mean_cost_micro_usd
            .zip(baseline.arithmetic_mean_cost_micro_usd)
            .is_some_and(|(candidate, control)| candidate < control)
    {
        (
            Recommendation::Promote,
            "comparable_quality_and_resource_evidence",
        )
    } else if challenger.incomplete_sessions >= config.maximum_pending_challenger
        || challenger.assigned_sessions >= config.maximum_challenger_sessions
    {
        exposure = 0;
        (Recommendation::Hold, "trial_exposure_or_pending_limit")
    } else {
        (Recommendation::Explore, "collect_comparable_outcomes")
    };
    let evidence_digest = digest(data)?;
    let config_digest = config.digest()?;
    let plan_id = digest(&(
        &evidence_digest,
        &config_digest,
        contract,
        previous_exposure_ppm,
        seed,
    ))?;
    Ok(BatchPlan {
        learner_version: LEARNER_VERSION.into(),
        plan_id,
        evidence_digest,
        measurement_contract: contract.into(),
        config_digest,
        seed,
        challenger_propensity_ppm: exposure,
        joint_benefit_probability_ppm: probability_ppm,
        monte_carlo_lower_ppm: (lower * f64::from(PPM)).round() as u32,
        recent_quality_harm_probability_ppm: ((recent_harmful as f64
            / config.posterior_draws as f64)
            * f64::from(PPM))
        .round() as u32,
        baseline,
        challenger,
        excluded_mixed_families: excluded,
        recommendation,
        reason: reason.into(),
    })
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
    use anyhow::Context;

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

    fn observation(id: &str, arm: Arm, quality: Option<f64>) -> Observation {
        Observation {
            session_key: id.into(),
            assignment_sequence: 0,
            family_id: id.into(),
            revision: "v1".into(),
            measurement_contract: "contract".into(),
            arm,
            quality,
            total_cost_micro_usd: Some(100),
            latency_ms: Some(100),
            severe_violation: false,
        }
    }

    #[test]
    fn adoption_monitor_retains_missing_recent_families_and_replaces_revisions() -> Result<()> {
        let config = BanditConfig::default();
        let mut data = EffectiveObservations::default();
        for i in 0..100 {
            let mut observed = observation(&format!("old-{i}"), Arm::Challenger, Some(0.98));
            observed.assignment_sequence = i;
            data.replace(observed)?;
        }
        for i in 0..config.recent_guard_families {
            let mut observed = observation(&format!("recent-{i}"), Arm::Challenger, None);
            observed.assignment_sequence = 100 + i as u64;
            data.replace(observed)?;
        }
        let unknown = monitor(&config, &data, "contract", 71)?;
        assert_eq!(unknown.recent_quality.observed_families, 0);
        assert_eq!(
            unknown.incomplete_recent_families,
            config.recent_guard_families
        );
        assert!(!unknown.rollback);
        for i in 0..config.recent_guard_families {
            let mut observed = data
                .sessions
                .get(&format!("recent-{i}"))
                .context("missing observation")?
                .clone();
            observed.quality = Some(0.2);
            observed.revision = "completed".into();
            data.replace(observed)?;
        }
        let bad = monitor(&config, &data, "contract", 71)?;
        assert!(bad.rollback);
        assert_eq!(
            bad.recent_quality.observed_families,
            config.recent_guard_families
        );
        for i in 0..config.recent_guard_families {
            let mut observed = data
                .sessions
                .get(&format!("recent-{i}"))
                .context("missing observation")?
                .clone();
            observed.quality = Some(0.96);
            observed.revision = "corrected".into();
            data.replace(observed)?;
        }
        let corrected = monitor(&config, &data, "contract", 71)?;
        assert!(!corrected.rollback);
        assert_eq!(corrected.sessions, bad.sessions);
        assert_eq!(
            corrected.recent_quality.observed_families,
            bad.recent_quality.observed_families
        );
        Ok(())
    }

    #[test]
    fn adoption_monitor_clusters_forks_and_honors_supported_human_vetoes() -> Result<()> {
        let config = BanditConfig::default();
        let mut data = EffectiveObservations::default();
        for i in 0..32 {
            let mut observed = observation(&i.to_string(), Arm::Challenger, Some(0.1));
            observed.family_id = "shared-lineage".into();
            observed.assignment_sequence = i;
            data.replace(observed)?;
        }
        let report = monitor(&config, &data, "contract", 3)?;
        assert_eq!(report.recent_quality.observed_families, 1);
        assert!(!report.rollback);
        let mut missing = data.sessions.get("0").context("missing fork")?.clone();
        missing.quality = None;
        data.replace(missing.clone())?;
        assert_eq!(
            monitor(&config, &data, "contract", 3)?
                .recent_quality
                .observed_families,
            0
        );
        missing.measurement_contract = "manual-correction".into();
        missing.severe_violation = true;
        data.replace(missing)?;
        let veto = monitor(&config, &data, "contract", 3)?;
        assert!(veto.rollback);
        assert_eq!(veto.recent_quality.observed_families, 0);
        Ok(())
    }

    #[test]
    fn conjugate_update_matches_hand_calculation_without_counting_prior() -> Result<()> {
        let prior = MeanPrior {
            mean: 0.5,
            strength: 2.0,
            observation_variance: 0.25,
        };
        let posterior = prior.posterior(&[1.0, 0.0])?;
        assert!((posterior.mean - 0.5).abs() < 1e-12);
        assert!((posterior.variance - 0.0625).abs() < 1e-12);
        assert_eq!(posterior.degrees_of_freedom, 6.0);
        assert!((posterior.student_scale.powi(2) - 1.0 / 24.0).abs() < 1e-12);
        assert!((posterior.observation_variance_mean - 0.25).abs() < 1e-12);
        assert_eq!(posterior.observed_families, 2);
        assert!(
            MeanPrior {
                strength: 0.0,
                ..prior
            }
            .posterior(&[])
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn corrections_replace_samples_and_stale_labels_widen_uncertainty() -> Result<()> {
        let config = BanditConfig::default();
        let mut data = EffectiveObservations::default();
        data.replace(observation("s", Arm::Challenger, Some(1.0)))?;
        let before = plan(&config, &data, "contract", None, 1)?;
        let mut correction = observation("s", Arm::Challenger, Some(0.0));
        correction.revision = "v2".into();
        data.replace(correction.clone())?;
        let after = plan(&config, &data, "contract", None, 1)?;
        assert_eq!(after.challenger.quality.observed_families, 1);
        assert!(after.challenger.quality.mean < before.challenger.quality.mean);
        let recomputed = config.challenger.quality.posterior(&[0.0])?;
        assert_eq!(after.challenger.quality.variance, recomputed.variance);
        correction.quality = None;
        data.replace(correction)?;
        let stale = plan(&config, &data, "contract", None, 1)?;
        assert_eq!(stale.challenger.quality.observed_families, 0);
        assert!(stale.challenger.quality.variance > after.challenger.quality.variance);
        data.remove("s");
        assert!(data.sessions.is_empty());
        Ok(())
    }

    #[test]
    fn forks_are_one_cluster_and_mixed_arm_families_cannot_promote() -> Result<()> {
        let config = BanditConfig::default();
        let mut data = EffectiveObservations::default();
        for i in 0..50 {
            let mut item = observation(&i.to_string(), Arm::Challenger, Some(1.0));
            item.family_id = "shared".into();
            data.replace(item)?;
        }
        let result = plan(&config, &data, "contract", None, 2)?;
        assert_eq!(result.challenger.quality.observed_families, 1);
        let mut other = observation("control", Arm::Baseline, Some(0.9));
        other.family_id = "shared".into();
        data.replace(other)?;
        let result = plan(&config, &data, "contract", None, 2)?;
        assert_eq!(result.excluded_mixed_families, 1);
        assert_eq!(result.challenger.quality.observed_families, 0);
        assert_ne!(result.recommendation, Recommendation::Promote);
        Ok(())
    }

    #[test]
    fn pending_limits_and_severe_failures_stop_exposure_without_erasing_history() -> Result<()> {
        let config = BanditConfig::default();
        let mut data = EffectiveObservations::default();
        for i in 0..config.maximum_pending_challenger {
            data.replace(observation(&i.to_string(), Arm::Challenger, None))?;
        }
        let result = plan(&config, &data, "contract", None, 3)?;
        assert_eq!(result.recommendation, Recommendation::Hold);
        assert_eq!(result.challenger_propensity_ppm, 0);
        let mut severe = observation("failure", Arm::Challenger, Some(0.0));
        severe.severe_violation = true;
        data.replace(severe)?;
        let result = plan(&config, &data, "contract", None, 3)?;
        assert_eq!(result.recommendation, Recommendation::Rollback);
        assert_eq!(result.challenger_propensity_ppm, 0);
        assert_eq!(data.sessions.len(), config.maximum_pending_challenger + 1);
        Ok(())
    }

    #[test]
    fn resuming_a_pending_hold_does_not_reset_exposure_to_the_minimum() -> Result<()> {
        let config = BanditConfig::default();
        let mut data = EffectiveObservations::default();
        for i in 0..config.maximum_pending_challenger {
            data.replace(observation(&i.to_string(), Arm::Challenger, None))?;
        }
        let held = plan(
            &config,
            &data,
            "contract",
            Some(config.initial_exposure_ppm),
            31,
        )?;
        assert_eq!(held.challenger_propensity_ppm, 0);
        for i in 1..config.maximum_pending_challenger {
            data.replace(observation(&i.to_string(), Arm::Challenger, Some(0.99)))?;
        }
        let resumed = plan(
            &config,
            &data,
            "contract",
            Some(held.challenger_propensity_ppm),
            31,
        )?;
        assert_eq!(resumed.recommendation, Recommendation::Explore);
        assert_eq!(
            resumed.challenger_propensity_ppm,
            config.initial_exposure_ppm
        );
        Ok(())
    }

    #[test]
    fn a_consistently_good_candidate_can_promote_within_the_default_trial_quota() -> Result<()> {
        let config = BanditConfig::default();
        let mut data = EffectiveObservations::default();
        for i in 0..160 {
            let arm = if i % 2 == 0 {
                Arm::Baseline
            } else {
                Arm::Challenger
            };
            let mut sample = observation(
                &i.to_string(),
                arm,
                Some(0.94 + if i % 4 < 2 { 0.01 } else { -0.01 }),
            );
            sample.total_cost_micro_usd = Some(if arm == Arm::Baseline {
                1_000_000
            } else {
                350_000
            });
            sample.latency_ms = Some(if arm == Arm::Baseline { 60_000 } else { 40_000 });
            sample.assignment_sequence = i;
            data.replace(sample)?;
        }
        let outcome = plan(&config, &data, "contract", Some(500_000), 25)?;
        assert!(outcome.challenger.assigned_sessions < config.maximum_challenger_sessions);
        assert_eq!(outcome.recommendation, Recommendation::Promote);
        assert!(
            outcome.challenger.quality.observation_variance_mean
                < config.challenger.quality.observation_variance
        );
        Ok(())
    }

    #[test]
    fn recent_degradation_overrides_old_success_and_pending_feedback() -> Result<()> {
        let config = BanditConfig::default();
        let mut data = EffectiveObservations::default();
        for i in 0..500 {
            let mut item = observation(&i.to_string(), Arm::Challenger, Some(0.98));
            item.assignment_sequence = i;
            data.replace(item)?;
        }
        for i in 500..532 {
            let mut item = observation(&i.to_string(), Arm::Challenger, Some(0.1));
            item.assignment_sequence = i;
            data.replace(item)?;
        }
        data.replace(observation("pending", Arm::Challenger, None))?;
        let outcome = plan(&config, &data, "contract", Some(500_000), 42)?;
        assert!(
            outcome.challenger.quality.mean > 0.9,
            "cumulative history conceals the recent change"
        );
        assert_eq!(outcome.recommendation, Recommendation::Rollback);
        assert_eq!(outcome.reason, "recent_quality_below_floor");
        assert_eq!(outcome.challenger_propensity_ppm, 0);
        Ok(())
    }

    #[test]
    fn cold_start_does_not_starve_an_unmeasured_arm_after_cheap_baseline_feedback() -> Result<()> {
        let config = BanditConfig::default();
        let mut data = EffectiveObservations::default();
        let mut baseline = observation("baseline-first", Arm::Baseline, Some(0.95));
        baseline.total_cost_micro_usd = Some(300);
        baseline.latency_ms = Some(98);
        data.replace(baseline)?;
        let result = plan(&config, &data, "contract", None, 31)?;
        assert_eq!(result.recommendation, Recommendation::Explore);
        assert_eq!(result.challenger.assigned_sessions, 0);
        assert_eq!(
            result.challenger_propensity_ppm,
            config.initial_exposure_ppm
        );
        Ok(())
    }

    #[test]
    fn warm_up_preserves_reduced_exposure_and_requires_each_metric() -> Result<()> {
        let config = BanditConfig::default();
        for first in [Arm::Baseline, Arm::Challenger] {
            let mut data = EffectiveObservations::default();
            for i in 0..config.minimum_families_per_arm {
                let mut item = observation(&format!("{first:?}-{i}"), first, Some(0.95));
                item.total_cost_micro_usd = Some(30);
                item.latency_ms = Some(5);
                data.replace(item)?;
            }
            let result = plan(&config, &data, "contract", Some(50_000), 31)?;
            assert_eq!(result.challenger_propensity_ppm, 50_000);
            let other = if first == Arm::Baseline {
                Arm::Challenger
            } else {
                Arm::Baseline
            };
            for i in 0..config.minimum_families_per_arm {
                let mut item = observation(&format!("{other:?}-{i}"), other, Some(0.95));
                item.total_cost_micro_usd = Some(30);
                item.latency_ms = if i == 0 { None } else { Some(5) };
                data.replace(item)?;
            }
            let result = plan(&config, &data, "contract", Some(50_000), 31)?;
            assert_eq!(result.recommendation, Recommendation::Explore);
            assert_eq!(result.challenger_propensity_ppm, 50_000);
        }
        Ok(())
    }

    #[test]
    fn cold_start_retains_a_trial_and_explicit_probabilities_match_draws() -> Result<()> {
        let config = BanditConfig::default();
        let result = plan(
            &config,
            &EffectiveObservations::default(),
            "contract",
            None,
            4,
        )?;
        assert_eq!(result.recommendation, Recommendation::Explore);
        assert!(result.challenger_propensity_ppm > 0);
        assert!(result.challenger_propensity_ppm <= config.initial_exposure_ppm);
        let mut chosen = 0;
        for seed in 0..10_000 {
            chosen += usize::from(draw_assignment(200_000, seed)? == Arm::Challenger);
        }
        assert!((1800..=2200).contains(&chosen));
        Ok(())
    }
}
