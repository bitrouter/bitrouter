//! Reproducible controlled experiments. These are synthetic potential outcomes,
//! never measurements of real users, historical ACP sessions or a judge's skill.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use bitrouter::evolution::bandit::{
    Arm, BanditConfig, EffectiveObservations, Observation, Recommendation, draw_assignment, plan,
};
use clap::Parser;
use rand::{RngExt, SeedableRng, rngs::StdRng};
use serde::Serialize;

#[derive(Parser)]
struct Args {
    #[arg(long, default_value_t = 16)]
    seeds: u64,
    #[arg(long, default_value_t = 1200)]
    sessions: usize,
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum Scenario {
    Beneficial,
    Harmful,
    Delayed,
    Revised,
    Drift,
}

#[derive(Serialize)]
struct Run {
    scenario: Scenario,
    strategy: &'static str,
    seed: u64,
    sessions: usize,
    challenger_sessions: usize,
    total_cost_micro_usd: u64,
    mean_final_quality: f64,
    cumulative_quality_loss_vs_baseline: f64,
    promotions: usize,
    harmful_promotions: usize,
    rollbacks: usize,
    holds: usize,
    replacements: usize,
    final_observed_families: usize,
    maximum_pending_challenger: usize,
    curve: Vec<Point>,
}

#[derive(Serialize)]
struct Point {
    sessions: usize,
    quality: f64,
    cost_micro_usd: u64,
    exposure_ppm: u32,
}

#[derive(Clone)]
struct Feedback {
    due: usize,
    observation: Observation,
}

fn run(scenario: Scenario, strategy: &'static str, seed: u64, sessions: usize) -> Result<Run> {
    let config = BanditConfig {
        posterior_draws: 2048,
        maximum_challenger_sessions: sessions,
        ..BanditConfig::default()
    };
    let mut outcomes = StdRng::seed_from_u64(seed);
    let mut assignment_rng = StdRng::seed_from_u64(seed.wrapping_add(91_213));
    let mut data = EffectiveObservations::default();
    let mut feedback: Vec<Feedback> = Vec::new();
    let mut exposure = config.initial_exposure_ppm;
    let mut previous_recommendation = Recommendation::Explore;
    let mut withdrawn = false;
    let mut result = Run {
        scenario,
        strategy,
        seed,
        sessions,
        challenger_sessions: 0,
        total_cost_micro_usd: 0,
        mean_final_quality: 0.0,
        cumulative_quality_loss_vs_baseline: 0.0,
        promotions: 0,
        harmful_promotions: 0,
        rollbacks: 0,
        holds: 0,
        replacements: 0,
        final_observed_families: 0,
        maximum_pending_challenger: 0,
        curve: vec![],
    };
    let mut quality_sum = 0.0;
    for t in 0..sessions {
        for item in feedback.iter().filter(|f| f.due == t) {
            result.replacements += usize::from(
                data.sessions
                    .get(&item.observation.session_key)
                    .is_some_and(|o| o.quality.is_some()),
            );
            data.replace(item.observation.clone())?;
        }
        feedback.retain(|f| f.due > t);
        let candidate_good = match scenario {
            Scenario::Harmful | Scenario::Revised => false,
            Scenario::Drift => t < sessions / 2,
            _ => true,
        };
        if t % 16 == 0 {
            let batch = plan(
                &config,
                &data,
                "simulation-v1",
                Some(exposure),
                seed.wrapping_add(t as u64),
            )?;
            if strategy != "baseline" && !withdrawn {
                if strategy == "thompson" {
                    exposure = batch.challenger_propensity_ppm;
                }
                // Fixed allocation and TS share the same quality withdrawal
                // rule. Otherwise the experiment would conflate adaptation
                // with the mere presence of a safety guard.
                if batch.recommendation == Recommendation::Rollback {
                    withdrawn = true;
                    exposure = 0;
                }
                if batch.recommendation != previous_recommendation {
                    match batch.recommendation {
                        Recommendation::Promote => {
                            result.promotions += 1;
                            result.harmful_promotions += usize::from(!candidate_good);
                        }
                        Recommendation::Rollback => result.rollbacks += 1,
                        Recommendation::Hold => result.holds += 1,
                        Recommendation::Explore => {}
                    }
                }
                // This experiment measures the protected exploration allocator.
                // Promotion recommendations are recorded, not enacted; a serving
                // publication experiment is a separate integration acceptance.
                previous_recommendation = batch.recommendation;
            }
            result.final_observed_families = batch.baseline.quality.observed_families
                + batch.challenger.quality.observed_families;
        }
        // Coupled potential outcomes make paired seeds compare the same workload.
        // No unselected outcome is ever passed into the learner.
        let baseline_quality = 0.94 + outcomes.random_range(-0.04..0.04);
        let candidate_quality =
            if candidate_good { 0.94 } else { 0.55 } + outcomes.random_range(-0.04..0.04);
        let noise = outcomes.random_range(0.8..1.2);
        let selected_exposure = if strategy == "baseline" || withdrawn {
            0
        } else if strategy == "fixed_10_percent" {
            config.initial_exposure_ppm
        } else {
            exposure
        };
        let pending = data
            .sessions
            .values()
            .filter(|o| o.arm == Arm::Challenger && o.quality.is_none())
            .count();
        let actual_exposure = if pending >= config.maximum_pending_challenger {
            0
        } else {
            selected_exposure
        };
        let arm = draw_assignment(actual_exposure, assignment_rng.random())?;
        let quality = if arm == Arm::Baseline {
            baseline_quality
        } else {
            candidate_quality
        };
        let cost = (if arm == Arm::Baseline {
            1_000_000.0
        } else {
            350_000.0
        } * noise) as u64;
        let latency = (if arm == Arm::Baseline {
            60_000.0
        } else {
            40_000.0
        } * noise) as u64;
        let key = format!("session-{t}");
        let observed = Observation {
            session_key: key.clone(),
            assignment_sequence: t as u64,
            family_id: key,
            revision: format!("final-{t}"),
            measurement_contract: "simulation-v1".into(),
            arm,
            quality: Some(quality),
            total_cost_micro_usd: Some(cost),
            latency_ms: Some(latency),
            severe_violation: false,
        };
        let delay = if matches!(scenario, Scenario::Delayed) {
            48
        } else {
            1
        };
        data.replace(Observation {
            quality: None,
            total_cost_micro_usd: None,
            latency_ms: None,
            revision: format!("pending-{t}"),
            ..observed.clone()
        })?;
        if matches!(scenario, Scenario::Revised) && arm == Arm::Challenger {
            feedback.push(Feedback {
                due: t + 1,
                observation: Observation {
                    quality: Some(0.98),
                    revision: format!("provisional-{t}"),
                    ..observed.clone()
                },
            });
            feedback.push(Feedback {
                due: t + 64,
                observation: observed,
            });
        } else {
            feedback.push(Feedback {
                due: t + delay,
                observation: observed,
            });
        }
        result.challenger_sessions += usize::from(arm == Arm::Challenger);
        result.maximum_pending_challenger = result
            .maximum_pending_challenger
            .max(pending + usize::from(arm == Arm::Challenger));
        result.total_cost_micro_usd += cost;
        quality_sum += quality;
        result.cumulative_quality_loss_vs_baseline += baseline_quality - quality;
        if (t + 1) % 100 == 0 {
            result.curve.push(Point {
                sessions: t + 1,
                quality: quality_sum / (t + 1) as f64,
                cost_micro_usd: result.total_cost_micro_usd,
                exposure_ppm: actual_exposure,
            });
        }
    }
    // Drain delayed/revised labels without generating additional requests.
    feedback.sort_by_key(|f| f.due);
    for item in feedback {
        result.replacements += usize::from(
            data.sessions
                .get(&item.observation.session_key)
                .is_some_and(|o| o.quality.is_some()),
        );
        data.replace(item.observation)?;
    }
    let final_plan = plan(&config, &data, "simulation-v1", Some(exposure), seed)?;
    result.final_observed_families = final_plan.baseline.quality.observed_families
        + final_plan.challenger.quality.observed_families;
    result.mean_final_quality = quality_sum / sessions as f64;
    Ok(result)
}

fn main() -> Result<()> {
    let args = Args::parse();
    anyhow::ensure!(
        args.seeds > 0 && args.sessions > 0,
        "seeds and sessions must be positive"
    );
    let mut runs = Vec::new();
    for scenario in [
        Scenario::Beneficial,
        Scenario::Harmful,
        Scenario::Delayed,
        Scenario::Revised,
        Scenario::Drift,
    ] {
        for seed in 0..args.seeds {
            for strategy in ["baseline", "fixed_10_percent", "thompson"] {
                runs.push(run(scenario, strategy, seed, args.sessions)?);
            }
        }
    }
    let mut groups: BTreeMap<String, Vec<&Run>> = BTreeMap::new();
    for run in &runs {
        groups
            .entry(format!("{:?}/{}", run.scenario, run.strategy))
            .or_default()
            .push(run);
    }
    let summary: BTreeMap<_, _> = groups.into_iter().map(|(key, group)| {
        let n = group.len() as f64;
        let quality = group.iter().map(|r| r.mean_final_quality).sum::<f64>() / n;
        let variance = if group.len() > 1 { group.iter().map(|r| (r.mean_final_quality - quality).powi(2)).sum::<f64>() / (n - 1.0) } else { 0.0 };
        (key, serde_json::json!({"runs":group.len(), "mean_quality":quality,
            "quality_standard_error_across_seeds":(variance / n).sqrt(),
            "mean_cost_usd":group.iter().map(|r| r.total_cost_micro_usd as f64 / 1_000_000.0).sum::<f64>() / n,
            "mean_challenger_sessions":group.iter().map(|r| r.challenger_sessions as f64).sum::<f64>() / n,
            "harmful_promotion_recommendations":group.iter().map(|r| r.harmful_promotions).sum::<usize>(),
            "maximum_pending":group.iter().map(|r| r.maximum_pending_challenger).max(),
            "effective_count_matches_sessions":group.iter().all(|r| r.final_observed_families == r.sessions)}))
    }).collect();
    let report = serde_json::json!({"schema_version":1,"source":"synthetic_controlled_experiment",
        "scope":"allocator_only; promotion is not enacted; fixed allocation and TS share irreversible quality withdrawal; no live routing or judge accuracy claim",
        "likelihood":"normal_inverse_gamma; Student-t mean sampling; independent quality/log-cost/log-latency",
        "learner_config":BanditConfig {posterior_draws:2048,maximum_challenger_sessions:args.sessions,..BanditConfig::default()},
        "learner_source_digest":bitrouter::evolution::rubric::digest(&include_str!("../src/evolution/bandit.rs"))?,
        "seeds":args.seeds,"sessions_per_run":args.sessions,"summary":summary,"runs":runs});
    let serialized = serde_json::to_string_pretty(&report)?;
    if let Some(path) = args.output {
        std::fs::write(path, serialized)?;
    } else {
        println!("{serialized}");
    }
    Ok(())
}
