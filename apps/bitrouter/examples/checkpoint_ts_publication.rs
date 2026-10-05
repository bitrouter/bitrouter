//! Synthetic potential outcomes exercising production enrollment/publication.
//! No ACP history, judge accuracy, live traffic, or comparative safety claim.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use bitrouter::evolution::bandit::{
    Arm, BanditConfig, EffectiveObservations, Observation, Recommendation, monitor, plan,
    trial_seed,
};
use bitrouter::evolution::control::{
    BlockDefinition, BlockRule, BlockStatus, ControlState, EvolutionMode,
};
use bitrouter::evolution::rubric::digest;
use clap::{Parser, ValueEnum};
use rand::{RngExt, SeedableRng, rngs::StdRng};
use serde::Serialize;

const BLOCK: &str = "coding";
const CONTRACT: &str = "synthetic-publication-v1";
const RECONCILE_EVERY: usize = 16;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value_t = 16)]
    seeds: u64,
    #[arg(long, default_value_t = 1200)]
    sessions: usize,
    #[arg(long, default_value_t = 0)]
    seed_start: u64,
    #[arg(long, value_enum)]
    scenario: Vec<Scenario>,
    #[arg(long, value_enum)]
    strategy: Vec<Strategy>,
    #[arg(long, default_value_t = 16)]
    batch_sessions: usize,
    #[arg(long)]
    trace: bool,
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum Scenario {
    Beneficial,
    Harmful,
    Delayed,
    RevisedShort,
    RevisedLong,
    Drift,
    Missing,
    AssessedUnknown,
    CorrelatedForks,
}

impl Scenario {
    fn candidate_good(self, t: usize, horizon: usize) -> bool {
        match self {
            Self::Harmful | Self::RevisedShort | Self::RevisedLong => false,
            Self::Drift => t < horizon / 2,
            _ => true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum Strategy {
    Baseline,
    GuardedFixed,
    Thompson,
    ThompsonWithoutAdoptionMonitor,
}

#[derive(Clone)]
struct Potential {
    baseline_quality: f64,
    candidate_quality: f64,
    resource_noise: f64,
    delay: usize,
    correction_delay: Option<usize>,
    missing: bool,
    family: String,
}

fn workload(scenario: Scenario, seed: u64, horizon: usize) -> Vec<Potential> {
    let mut rng = StdRng::seed_from_u64(seed);
    let mut baseline_noise = 0.0;
    let mut candidate_noise = 0.0;
    (0..horizon)
        .map(|t| {
            let correlated = matches!(scenario, Scenario::CorrelatedForks);
            if !correlated || t % 4 == 0 {
                baseline_noise = rng.random_range(-0.04..0.04);
                candidate_noise = rng.random_range(-0.04..0.04);
            }
            Potential {
                baseline_quality: 0.94 + baseline_noise,
                candidate_quality: if scenario.candidate_good(t, horizon) {
                    0.94 + candidate_noise
                } else {
                    0.55 + candidate_noise
                },
                resource_noise: rng.random_range(0.8..1.2),
                delay: if matches!(scenario, Scenario::Delayed) {
                    rng.random_range(8..=96)
                } else {
                    1
                },
                correction_delay: match scenario {
                    Scenario::RevisedShort => Some(rng.random_range(8..=64)),
                    Scenario::RevisedLong => Some(rng.random_range(96..=384)),
                    _ => None,
                },
                missing: matches!(scenario, Scenario::Missing | Scenario::AssessedUnknown)
                    && rng.random_bool(0.1),
                family: format!("family-{}", if correlated { t / 4 } else { t }),
            }
        })
        .collect()
}

struct Feedback {
    due: usize,
    monitoring: bool,
    observation: Observation,
}

#[derive(Serialize)]
struct Transition {
    at_arrival: usize,
    action: String,
    status: BlockStatus,
}

#[derive(Serialize)]
struct Point {
    arrivals: usize,
    mean_quality: f64,
    cumulative_cost_micro_usd: u64,
    status: BlockStatus,
    trial_candidate_sessions: usize,
    deployment_sessions: usize,
}

#[derive(Serialize)]
struct PlanPoint {
    at_arrival: usize,
    cohort_members: usize,
    cohort_closed: bool,
    cohort_resolved: bool,
    baseline_quality_families: usize,
    candidate_quality_families: usize,
    baseline_incomplete: usize,
    candidate_incomplete: usize,
    recommended_exposure_ppm: u32,
    joint_benefit_probability_ppm: u32,
    monte_carlo_lower_ppm: u32,
    recommendation: Recommendation,
    reason: String,
}

#[derive(Serialize)]
struct Run {
    scenario: Scenario,
    strategy: Strategy,
    seed: u64,
    sessions: usize,
    total_cost_micro_usd: u64,
    mean_final_quality: f64,
    quality_loss_vs_paired_baseline: f64,
    post_adoption_quality_loss: f64,
    challenger_sessions: usize,
    harmful_deployment_sessions: usize,
    trial_sessions: usize,
    trial_candidate_sessions: usize,
    deployment_sessions: usize,
    unenrolled_baseline_sessions: usize,
    maximum_pending_trial_candidates: usize,
    unresolved_trial_sessions: usize,
    observed_trial_families: usize,
    excluded_mixed_families: usize,
    recent_monitor_families: usize,
    recent_monitor_incomplete_families: usize,
    label_replacements: usize,
    fenced_open_cohort_promotions: usize,
    harmful_adoptions: usize,
    status_at_horizon: BlockStatus,
    final_status: BlockStatus,
    transitions: Vec<Transition>,
    curve: Vec<Point>,
    admission_waits: BTreeMap<String, usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    planning_trace: Vec<PlanPoint>,
}

struct Experiment {
    state: ControlState,
    trial: EffectiveObservations,
    deployment: EffectiveObservations,
    resolved: BTreeSet<String>,
    feedback: Vec<Feedback>,
    last_trial_digest: Option<String>,
    last_monitor_digest: Option<String>,
    result: Run,
    trace: bool,
}

impl Experiment {
    fn new(
        scenario: Scenario,
        strategy: Strategy,
        seed: u64,
        sessions: usize,
        batch_sessions: usize,
        trace: bool,
    ) -> Result<Self> {
        let mut config = BanditConfig::default();
        if strategy == Strategy::GuardedFixed {
            // Fixed exploration uses the same admission, feedback and publication
            // gates. Only the allowed exploratory probability is pinned to 10%.
            config.minimum_exposure_ppm = config.initial_exposure_ppm;
            config.maximum_exposure_ppm = config.initial_exposure_ppm;
        }
        let mut state = ControlState::default();
        state.set_mode(EvolutionMode::Manual, None)?;
        state.register(
            BlockDefinition {
                block_id: BLOCK.into(),
                source: "synthetic".into(),
                rationale: "Synthetic cost-reducing complete policy".into(),
                rules: vec![BlockRule {
                    selector: "baseline".into(),
                    fingerprint: None,
                    baseline_route: "baseline".into(),
                    challenger_route: "candidate".into(),
                }],
                independence_rationale: "Single block experiment".into(),
                dependencies: BTreeMap::new(),
                measurement_contract: CONTRACT.into(),
                batch_sessions,
                bandit: config,
            },
            "synthetic-routes-v1".into(),
        )?;
        Ok(Self {
            trace,
            state,
            trial: EffectiveObservations::default(),
            deployment: EffectiveObservations::default(),
            resolved: BTreeSet::new(),
            feedback: vec![],
            last_trial_digest: None,
            last_monitor_digest: None,
            result: Run {
                scenario,
                strategy,
                seed,
                sessions,
                total_cost_micro_usd: 0,
                mean_final_quality: 0.0,
                quality_loss_vs_paired_baseline: 0.0,
                post_adoption_quality_loss: 0.0,
                challenger_sessions: 0,
                harmful_deployment_sessions: 0,
                trial_sessions: 0,
                trial_candidate_sessions: 0,
                deployment_sessions: 0,
                unenrolled_baseline_sessions: 0,
                maximum_pending_trial_candidates: 0,
                unresolved_trial_sessions: 0,
                observed_trial_families: 0,
                excluded_mixed_families: 0,
                recent_monitor_families: 0,
                recent_monitor_incomplete_families: 0,
                label_replacements: 0,
                fenced_open_cohort_promotions: 0,
                harmful_adoptions: 0,
                status_at_horizon: BlockStatus::Exploring,
                final_status: BlockStatus::Exploring,
                transitions: vec![],
                curve: vec![],
                admission_waits: BTreeMap::new(),
                planning_trace: vec![],
            },
        })
    }

    fn status(&self) -> Result<BlockStatus> {
        Ok(self
            .state
            .blocks
            .get(BLOCK)
            .context("missing block")?
            .status)
    }

    fn deliver(&mut self, time: usize) -> Result<()> {
        let mut pending = Vec::new();
        for feedback in std::mem::take(&mut self.feedback) {
            if feedback.due > time {
                pending.push(feedback);
                continue;
            }
            let data = if feedback.monitoring {
                &mut self.deployment
            } else {
                self.resolved
                    .insert(feedback.observation.session_key.clone());
                &mut self.trial
            };
            self.result.label_replacements += usize::from(
                data.sessions
                    .get(&feedback.observation.session_key)
                    .is_some_and(|o| o.quality.is_some()),
            );
            data.replace(feedback.observation)?;
        }
        self.feedback = pending;
        Ok(())
    }

    fn reconcile(&mut self, time: usize) -> Result<()> {
        if self.result.strategy == Strategy::Baseline || self.status()? == BlockStatus::RolledBack {
            return Ok(());
        }
        let before = self.status()?;
        let block = self.state.blocks.get(BLOCK).context("missing block")?;
        let config = block.definition.bandit.clone();
        let experiment_id = block.experiment_id.clone();
        let mut alarm = false;
        if before == BlockStatus::Adopted
            && self.result.strategy != Strategy::ThompsonWithoutAdoptionMonitor
        {
            let evidence = digest(&self.deployment)?;
            if self.last_monitor_digest.as_ref() != Some(&evidence) {
                let seed = digest(&(
                    bitrouter::evolution::bandit::MONITOR_VERSION,
                    &experiment_id,
                ))?;
                let monitor = monitor(
                    &config,
                    &self.deployment,
                    CONTRACT,
                    u64::from_str_radix(seed.get(..16).context("short monitoring seed")?, 16)?,
                )?;
                alarm = self.state.apply_monitoring(BLOCK, &monitor)?;
                self.last_monitor_digest = Some(evidence);
            }
        }
        if !alarm {
            let evidence = digest(&self.trial)?;
            if self.last_trial_digest.as_ref() != Some(&evidence) {
                let block = self.state.blocks.get(BLOCK).context("missing block")?;
                let resolved = !block.batch.members.is_empty()
                    && block
                        .batch
                        .members
                        .keys()
                        .all(|key| self.resolved.contains(key));
                let proposal = plan(
                    &config,
                    &self.trial,
                    CONTRACT,
                    Some(block.last_exposure_ppm),
                    trial_seed(&experiment_id)?,
                )?;
                if self.trace {
                    self.result.planning_trace.push(PlanPoint {
                        at_arrival: time,
                        cohort_members: block.batch.members.len(),
                        cohort_closed: block.batch.closed,
                        cohort_resolved: resolved,
                        baseline_quality_families: proposal.baseline.quality.observed_families,
                        candidate_quality_families: proposal.challenger.quality.observed_families,
                        baseline_incomplete: proposal.baseline.incomplete_sessions,
                        candidate_incomplete: proposal.challenger.incomplete_sessions,
                        recommended_exposure_ppm: proposal.challenger_propensity_ppm,
                        joint_benefit_probability_ppm: proposal.joint_benefit_probability_ppm,
                        monte_carlo_lower_ppm: proposal.monte_carlo_lower_ppm,
                        recommendation: proposal.recommendation,
                        reason: proposal.reason.clone(),
                    });
                }
                if proposal.recommendation == Recommendation::Promote
                    && !(block.batch.closed && resolved)
                {
                    // Current production code stages the validated allocation
                    // while keeping adoption fenced until the cohort resolves.
                    // Exercise that transition instead of expecting an obsolete
                    // error contract from earlier controller versions.
                    self.state.apply_plan(BLOCK, proposal, resolved)?;
                    ensure!(
                        self.status()? == BlockStatus::Exploring,
                        "unresolved/open cohort changed the serving policy"
                    );
                    self.result.fenced_open_cohort_promotions += 1;
                    self.last_trial_digest = Some(evidence);
                } else {
                    self.state.apply_plan(BLOCK, proposal, resolved)?;
                    self.last_trial_digest = Some(evidence);
                }
            }
        }
        let after = self.status()?;
        if before != after {
            let publication = self
                .state
                .publications
                .last()
                .context("missing publication")?;
            self.result.transitions.push(Transition {
                at_arrival: time,
                action: publication.action.clone(),
                status: after,
            });
            if after == BlockStatus::Adopted
                && !self
                    .result
                    .scenario
                    .candidate_good(time, self.result.sessions)
            {
                self.result.harmful_adoptions += 1;
            }
        }
        Ok(())
    }

    fn admit(&mut self, t: usize, potential: &Potential, seed: u64) -> Result<()> {
        let key = format!("session-{t}");
        let monitoring =
            self.result.strategy != Strategy::Baseline && self.status()? == BlockStatus::Adopted;
        let allocation = if self.result.strategy != Strategy::Baseline && !monitoring {
            self.state.reserve_trial(BLOCK, &key, seed)?
        } else {
            None
        };
        let arm = if monitoring {
            Arm::Challenger
        } else {
            allocation.as_ref().map_or(Arm::Baseline, |a| a.arm)
        };
        let quality = if arm == Arm::Baseline {
            potential.baseline_quality
        } else {
            potential.candidate_quality
        };
        let cost = (if arm == Arm::Baseline {
            1_000_000.0
        } else {
            350_000.0
        } * potential.resource_noise) as u64;
        let latency = (if arm == Arm::Baseline {
            60_000.0
        } else {
            40_000.0
        } * potential.resource_noise) as u64;
        self.result.mean_final_quality += quality;
        self.result.total_cost_micro_usd += cost;
        self.result.quality_loss_vs_paired_baseline += potential.baseline_quality - quality;
        self.result.challenger_sessions += usize::from(arm == Arm::Challenger);
        let sequence = if let Some(allocation) = allocation {
            allocation.assignment_sequence
        } else if monitoring {
            self.state.next_assignment_sequence = self
                .state
                .next_assignment_sequence
                .checked_add(1)
                .context("monitoring sequence overflow")?;
            self.result.post_adoption_quality_loss += potential.baseline_quality - quality;
            self.result.harmful_deployment_sessions +=
                usize::from(!self.result.scenario.candidate_good(t, self.result.sessions));
            self.state.next_assignment_sequence
        } else {
            self.result.unenrolled_baseline_sessions += 1;
            let block = self.state.blocks.get(BLOCK).context("missing block")?;
            let reason = if self.result.strategy == Strategy::Baseline {
                "baseline_only"
            } else if block.status == BlockStatus::RolledBack {
                "withdrawn"
            } else if block.batch.closed {
                if block
                    .batch
                    .members
                    .keys()
                    .all(|key| self.resolved.contains(key))
                {
                    "closed_cohort_awaiting_reconciliation"
                } else {
                    "closed_cohort_awaiting_feedback"
                }
            } else if block
                .plan
                .as_ref()
                .is_some_and(|plan| plan.challenger_propensity_ppm == 0)
            {
                "zero_allocation"
            } else {
                "other_admission_guard"
            };
            *self
                .result
                .admission_waits
                .entry(reason.into())
                .or_default() += 1;
            return Ok(());
        };
        let pending = Observation {
            session_key: key,
            assignment_sequence: sequence,
            family_id: potential.family.clone(),
            revision: "pending".into(),
            measurement_contract: CONTRACT.into(),
            arm,
            quality: None,
            total_cost_micro_usd: None,
            latency_ms: None,
            severe_violation: false,
        };
        let data = if monitoring {
            &mut self.deployment
        } else {
            &mut self.trial
        };
        data.replace(pending.clone())?;
        let observed = Observation {
            revision: "final".into(),
            quality: (!potential.missing).then_some(quality),
            total_cost_micro_usd: Some(cost),
            latency_ms: Some(latency),
            ..pending
        };
        if !potential.missing || matches!(self.result.scenario, Scenario::AssessedUnknown) {
            if let Some(delay) = potential
                .correction_delay
                .filter(|_| arm == Arm::Challenger)
            {
                self.feedback.push(Feedback {
                    due: t + 1,
                    monitoring,
                    observation: Observation {
                        revision: "provisional".into(),
                        quality: Some(0.98),
                        ..observed.clone()
                    },
                });
                self.feedback.push(Feedback {
                    due: t + delay,
                    monitoring,
                    observation: observed,
                });
            } else {
                self.feedback.push(Feedback {
                    due: t + potential.delay,
                    monitoring,
                    observation: observed,
                });
            }
        }
        let pending_count = self
            .trial
            .sessions
            .values()
            .filter(|o| o.arm == Arm::Challenger && o.quality.is_none())
            .count();
        self.result.maximum_pending_trial_candidates = self
            .result
            .maximum_pending_trial_candidates
            .max(pending_count);
        Ok(())
    }

    fn finish(mut self) -> Result<Run> {
        self.result.status_at_horizon = self.status()?;
        while let Some(time) = self.feedback.iter().map(|f| f.due).min() {
            // Feedback can arrive after the workload stops. No additional
            // sessions are generated while corrections and delays drain.
            self.deliver(time)?;
            self.reconcile(time)?;
        }
        let block = self.state.blocks.get(BLOCK).context("missing block")?;
        let summary = plan(
            &block.definition.bandit,
            &self.trial,
            CONTRACT,
            Some(block.last_exposure_ppm),
            self.result.seed,
        )?;
        let monitoring = monitor(
            &block.definition.bandit,
            &self.deployment,
            CONTRACT,
            self.result.seed,
        )?;
        self.result.mean_final_quality /= self.result.sessions as f64;
        self.result.final_status = block.status;
        self.result.trial_sessions = self.trial.sessions.len();
        self.result.trial_candidate_sessions = block.assigned_challenger_sessions;
        self.result.deployment_sessions = self.deployment.sessions.len();
        self.result.unresolved_trial_sessions = self
            .trial
            .sessions
            .keys()
            .filter(|k| !self.resolved.contains(*k))
            .count();
        self.result.observed_trial_families = summary.baseline.quality.observed_families
            + summary.challenger.quality.observed_families;
        self.result.excluded_mixed_families = summary.excluded_mixed_families;
        self.result.recent_monitor_families = monitoring.recent_families;
        self.result.recent_monitor_incomplete_families = monitoring.incomplete_recent_families;
        ensure!(
            self.result.maximum_pending_trial_candidates
                <= block.definition.bandit.maximum_pending_challenger,
            "pending trial quota exceeded"
        );
        ensure!(
            block.assigned_challenger_sessions
                <= block.definition.bandit.maximum_challenger_sessions,
            "total trial quota exceeded"
        );
        ensure!(
            self.result.trial_sessions
                + self.result.deployment_sessions
                + self.result.unenrolled_baseline_sessions
                == self.result.sessions,
            "duplicate or missing session membership"
        );
        ensure!(
            self.trial
                .sessions
                .keys()
                .all(|key| !self.deployment.sessions.contains_key(key)),
            "monitoring contaminated randomized evidence"
        );
        Ok(self.result)
    }
}

fn run(
    scenario: Scenario,
    strategy: Strategy,
    seed: u64,
    potentials: &[Potential],
    batch_sessions: usize,
    trace: bool,
) -> Result<Run> {
    let mut experiment = Experiment::new(
        scenario,
        strategy,
        seed,
        potentials.len(),
        batch_sessions,
        trace,
    )?;
    let mut rng = StdRng::seed_from_u64(seed.wrapping_add(91_213));
    for (t, potential) in potentials.iter().enumerate() {
        experiment.deliver(t)?;
        if t % RECONCILE_EVERY == 0 {
            experiment.reconcile(t)?;
        }
        experiment.admit(t, potential, rng.random())?;
        if (t + 1) % 100 == 0 {
            let block = experiment
                .state
                .blocks
                .get(BLOCK)
                .context("missing block")?;
            experiment.result.curve.push(Point {
                arrivals: t + 1,
                mean_quality: experiment.result.mean_final_quality / (t + 1) as f64,
                cumulative_cost_micro_usd: experiment.result.total_cost_micro_usd,
                status: block.status,
                trial_candidate_sessions: block.assigned_challenger_sessions,
                deployment_sessions: experiment.deployment.sessions.len(),
            });
        }
    }
    experiment.finish()
}

fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        args.seeds > 0 && args.sessions > 0 && args.batch_sessions > 0,
        "seeds, sessions and batch size must be positive"
    );
    let seed_end = args
        .seed_start
        .checked_add(args.seeds)
        .context("seed range overflow")?;
    let mut runs = Vec::new();
    let scenarios = if args.scenario.is_empty() {
        vec![
            Scenario::Beneficial,
            Scenario::Harmful,
            Scenario::Delayed,
            Scenario::RevisedShort,
            Scenario::RevisedLong,
            Scenario::Drift,
            Scenario::Missing,
            Scenario::AssessedUnknown,
            Scenario::CorrelatedForks,
        ]
    } else {
        args.scenario.clone()
    };
    let strategies = if args.strategy.is_empty() {
        vec![
            Strategy::Baseline,
            Strategy::GuardedFixed,
            Strategy::Thompson,
            Strategy::ThompsonWithoutAdoptionMonitor,
        ]
    } else {
        args.strategy.clone()
    };
    for scenario in &scenarios {
        for seed in args.seed_start..seed_end {
            let potentials = workload(*scenario, seed, args.sessions);
            for strategy in &strategies {
                runs.push(
                    run(
                        *scenario,
                        *strategy,
                        seed,
                        &potentials,
                        args.batch_sessions,
                        args.trace,
                    )
                    .with_context(|| format!("{scenario:?}/{strategy:?}/seed-{seed}"))?,
                );
            }
        }
        eprintln!("completed {scenario:?}: {} seeds", args.seeds);
    }
    let mut groups: BTreeMap<String, Vec<&Run>> = BTreeMap::new();
    for run in &runs {
        groups
            .entry(format!("{:?}/{:?}", run.scenario, run.strategy))
            .or_default()
            .push(run);
    }
    let summary: BTreeMap<_, _> = groups.into_iter().map(|(key, group)| {
        let n = group.len() as f64;
        (key, serde_json::json!({
            "runs": group.len(),
            "mean_quality": group.iter().map(|r| r.mean_final_quality).sum::<f64>() / n,
            "mean_cost_usd": group.iter().map(|r| r.total_cost_micro_usd as f64 / 1_000_000.0).sum::<f64>() / n,
            "mean_candidate_sessions": group.iter().map(|r| r.challenger_sessions).sum::<usize>() as f64 / n,
            "mean_trial_candidate_sessions": group.iter().map(|r| r.trial_candidate_sessions).sum::<usize>() as f64 / n,
            "mean_deployment_sessions": group.iter().map(|r| r.deployment_sessions).sum::<usize>() as f64 / n,
            "mean_harmful_deployment_sessions": group.iter().map(|r| r.harmful_deployment_sessions).sum::<usize>() as f64 / n,
            "mean_post_adoption_quality_loss": group.iter().map(|r| r.post_adoption_quality_loss).sum::<f64>() / n,
            "mean_unresolved_trial_sessions": group.iter().map(|r| r.unresolved_trial_sessions).sum::<usize>() as f64 / n,
            "mean_excluded_mixed_families": group.iter().map(|r| r.excluded_mixed_families).sum::<usize>() as f64 / n,
            "adoptions": group.iter().filter(|r| r.transitions.iter().any(|t| t.status == BlockStatus::Adopted)).count(),
            "harmful_adoptions": group.iter().map(|r| r.harmful_adoptions).sum::<usize>(),
            "rollbacks": group.iter().filter(|r| r.final_status == BlockStatus::RolledBack).count(),
            "maximum_pending_trial_candidates": group.iter().map(|r| r.maximum_pending_trial_candidates).max(),
            "maximum_trial_candidate_sessions": group.iter().map(|r| r.trial_candidate_sessions).max()
        }))
    }).collect();
    let report = serde_json::json!({
        "schema_version": 1,
        "source": "synthetic_enacted_publication_experiment",
        "learner_version": bitrouter::evolution::bandit::LEARNER_VERSION,
        "posterior_seed": "production trial_seed(experiment_id); common draws per experiment and learner version, independent of source revisions and session assignment randomness",
        "scope": "production reserve_trial/apply_plan/apply_monitoring and learner; in-memory simulated session outcomes, no canonical capture/DB/daemon/real judge; adoption routes new sessions to candidate; rollback is latched",
        "config": BanditConfig::default(),
        "fixed_comparator_override": "minimum and maximum exploration probability fixed to initial 100000 ppm; all other gates identical",
        "batch_sessions": args.batch_sessions,
        "reconcile_every_arrivals": RECONCILE_EVERY,
        "scenarios": scenarios,
        "strategies": strategies,
        "planning_trace": args.trace,
        "feedback": "normal delay 1; delayed uniform 8..96; revised candidate provisional 0.98 then true label after uniform 8..64 or 96..384; missing 10% never returns; assessed_unknown 10% returns unknown quality with complete resources; correlated forks share family/noise in groups of four",
        "outcomes": "baseline quality 0.94 +/- 0.04; good candidate 0.94 +/- 0.04; harmful 0.55 +/- 0.04; drift at half horizon; baseline cost 1 USD and candidate 0.35 USD times uniform 0.8..1.2, synthetic and excluding judge cost",
        "source_digests": {
            "simulation": digest(&include_str!("checkpoint_ts_publication.rs"))?,
            "bandit": digest(&include_str!("../src/evolution/bandit.rs"))?,
            "control": digest(&include_str!("../src/evolution/control.rs"))?
        },
        "seeds": args.seeds,
        "seed_start": args.seed_start,
        "sessions_per_run": args.sessions,
        "summary": summary,
        "runs": runs
    });
    let text = serde_json::to_string_pretty(&report)?;
    if let Some(output) = args.output {
        std::fs::write(output, text)?;
    } else {
        println!("{text}");
    }
    Ok(())
}
