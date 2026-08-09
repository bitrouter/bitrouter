use serde::Serialize;

use crate::optimization::OptimizationVerdict;
use crate::optimization::orchestrator::OptimizationReport;
use crate::optimization::{EvaluatorLock, OptimizationPreference, OptimizationRunLock};
use crate::output::CliReport;
use crate::output::human::{Health, Human};

#[derive(Debug, Clone, Serialize)]
pub struct OptimizationSetupReport {
    pub action: &'static str,
    pub model: &'static str,
    pub intent: String,
    pub lock: String,
    pub contract: String,
    pub workflow: Vec<String>,
    pub strong: String,
    pub economy: String,
    pub evaluator: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evaluator_lock: Option<EvaluatorLock>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub normalized_price_overrides: Vec<String>,
    pub preference: OptimizationPreference,
    pub active_policy_digest: String,
    pub latency: &'static str,
}

impl CliReport for OptimizationSetupReport {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        human.status_block(Health::Up, "bitrouter/auto optimization is configured")?;
        human.field("workflow", self.workflow.join(" "))?;
        human.field("routes", format!("{} → {}", self.strong, self.economy))?;
        human.field("judge", &self.evaluator)?;
        human.field("intent", &self.intent)?;
        human.field("lock", &self.lock)?;
        human.field("contract", &self.contract)?;
        human.blank()?;
        human.note(&format!(
            "next: edit {} if needed, then run `bitrouter optimize run --human`",
            self.contract
        ))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OptimizationStatusReport {
    pub action: &'static str,
    pub model: &'static str,
    pub intent: String,
    pub intent_digest: String,
    pub lock_active_policy_digest: String,
    pub actual_active_policy_digest: String,
    pub policy_mode: String,
    pub lineage_consistent: bool,
    pub latest_candidate_active: bool,
    pub rolled_back: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repair_hint: Option<String>,
    pub preference: OptimizationPreference,
    pub evaluator: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evaluator_lock: Option<EvaluatorLock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_run: Option<OptimizationRunLock>,
    pub latency: &'static str,
}

impl CliReport for OptimizationStatusReport {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        let (health, headline) = if !self.lineage_consistent {
            (
                Health::Down,
                "bitrouter/auto optimization lineage needs repair",
            )
        } else if self.latest_candidate_active {
            (Health::Up, "bitrouter/auto optimized candidate is active")
        } else if self.rolled_back {
            (Health::Unknown, "bitrouter/auto is active after rollback")
        } else if self.latest_run.as_ref().is_some_and(|run| run.publishable) {
            (Health::Up, "bitrouter/auto candidate is ready for review")
        } else if self.latest_run.is_some() {
            (
                Health::Down,
                "bitrouter/auto latest candidate is not publishable",
            )
        } else {
            (Health::Up, "bitrouter/auto is ready to measure")
        };
        human.status_block(health, headline)?;
        human.field(
            "lineage",
            if self.lineage_consistent {
                "consistent"
            } else {
                "diverged"
            },
        )?;
        human.field("mode", &self.policy_mode)?;
        human.field("judge", &self.evaluator)?;
        human.field("intent", &self.intent)?;
        if let Some(latest) = &self.latest_run {
            human.field("run", &latest.run_id)?;
        }
        human.blank()?;
        if let Some(hint) = &self.repair_hint {
            human.note(&format!("repair: {hint}"))?;
        } else if let Some(latest) = &self.latest_run {
            if self.latest_candidate_active || self.rolled_back {
                human.note("next: bitrouter optimize run --human")?;
            } else {
                human.note(&format!(
                    "next: bitrouter optimize review --run {} --human",
                    latest.run_id
                ))?;
            }
        } else {
            human.note("next: bitrouter optimize run --human")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OptimizationReviewReport {
    pub action: &'static str,
    pub model: &'static str,
    #[serde(flatten)]
    pub report: OptimizationReport,
    pub active: bool,
    pub rolled_back: bool,
    pub publication_requires_enable_adaptive: bool,
}

impl OptimizationReviewReport {
    pub fn for_run(report: OptimizationReport, publication_requires_enable_adaptive: bool) -> Self {
        Self {
            action: "optimize.run",
            model: "bitrouter/auto",
            report,
            active: false,
            rolled_back: false,
            publication_requires_enable_adaptive,
        }
    }

    pub fn new(
        report: OptimizationReport,
        active: bool,
        rolled_back: bool,
        publication_requires_enable_adaptive: bool,
    ) -> Self {
        Self {
            action: "optimize.review",
            model: "bitrouter/auto",
            report,
            active,
            rolled_back,
            publication_requires_enable_adaptive,
        }
    }
}

impl CliReport for OptimizationReviewReport {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        let (health, headline) = if self.active {
            (Health::Up, "bitrouter/auto candidate is active")
        } else if self.rolled_back {
            (Health::Unknown, "bitrouter/auto candidate was rolled back")
        } else if self.report.publishable {
            (Health::Up, "bitrouter/auto candidate is ready for review")
        } else {
            (Health::Down, "bitrouter/auto candidate is not publishable")
        };
        human.status_block(health, headline)?;
        human.field("run", &self.report.run_id)?;
        human.field("route", &self.report.target_request_key)?;
        human.field(
            "quality",
            format!(
                "{} → {}",
                verdict(self.report.baseline.verdict),
                verdict(self.report.candidate.verdict)
            ),
        )?;
        human.field(
            "cost",
            format!(
                "{} → {}{}",
                dollars(self.report.baseline.normalized_cost_micro_usd),
                dollars(self.report.candidate.normalized_cost_micro_usd),
                savings(self.report.normalized_cost_delta_ppm)
            ),
        )?;
        human.field(
            "latency",
            format!(
                "{} ms → {} ms (latency is observe-only)",
                self.report.baseline.observed_latency_ms, self.report.candidate.observed_latency_ms
            ),
        )?;
        human.field("candidate", &self.report.candidate_digest)?;
        for caveat in &self.report.caveats {
            human.note(caveat)?;
        }
        if self.action == "optimize.run" {
            human.blank()?;
            human.note(&format!(
                "next: bitrouter optimize review --run {} --human",
                self.report.run_id
            ))?;
        } else if !self.active && self.report.publishable {
            human.blank()?;
            let adaptive = if self.publication_requires_enable_adaptive {
                " --enable-adaptive"
            } else {
                ""
            };
            human.note(&format!(
                "next: bitrouter optimize publish --run {}{adaptive}",
                self.report.run_id
            ))?;
        }
        Ok(())
    }
}

fn verdict(value: OptimizationVerdict) -> &'static str {
    match value {
        OptimizationVerdict::Pass => "pass",
        OptimizationVerdict::Fail => "fail",
        OptimizationVerdict::Inconclusive => "inconclusive",
    }
}

fn dollars(micro_usd: u64) -> String {
    format!("${}.{:06}", micro_usd / 1_000_000, micro_usd % 1_000_000)
}

fn savings(delta_ppm: Option<i64>) -> String {
    match delta_ppm {
        Some(delta) if delta < 0 => {
            format!(" ({:.1}% lower)", (delta.unsigned_abs() as f64) / 10_000.0)
        }
        Some(delta) if delta > 0 => format!(" ({:.1}% higher)", (delta as f64) / 10_000.0),
        Some(_) => " (unchanged)".into(),
        None => " (baseline cost was zero)".into(),
    }
}
