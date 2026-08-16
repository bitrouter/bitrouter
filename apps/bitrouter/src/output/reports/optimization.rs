use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::{Health, Human};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TreatmentReport {
    pub target_request_key: String,
    pub champion_tier: String,
    pub challenger_tier: String,
    pub challenger_exposure_ppm: u32,
    pub minimum_tasks_per_arm: u32,
    pub maximum_challenger_tasks: u32,
    pub minimum_pass_rate_ppm: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArmReport {
    pub observed: u32,
    pub eligible: u32,
    pub excluded: u32,
    pub conclusive: u32,
    pub pass: u32,
    pub fail: u32,
    pub hard_violations: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pass_rate_ppm: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean_cost_micro_usd: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OptimizationControllerReport {
    pub action: &'static str,
    pub policy: String,
    pub decision: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_policy_digest: Option<String>,
    pub active_policy_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub eval_snapshot_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_subject_digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub treatment: Option<TreatmentReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control: Option<ArmReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub challenger: Option<ArmReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_delta_micro_usd: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evaluator_config_digest: Option<String>,
    pub published: bool,
    pub reload_attempted: bool,
}

impl CliReport for OptimizationControllerReport {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        human.status_block(
            Health::Up,
            &format!(
                "{}/{} optimization is {}",
                self.policy,
                self.action,
                gerund(self.decision)
            ),
        )?;
        human.field("decision", self.decision)?;
        human.field("policy", &self.active_policy_digest)?;
        if let Some(treatment) = &self.treatment {
            human.field("route", &treatment.target_request_key)?;
            human.field(
                "tiers",
                format!(
                    "{} → {}",
                    treatment.champion_tier, treatment.challenger_tier
                ),
            )?;
        }
        human.field("published", self.published)?;
        human.field("reload", self.reload_attempted)
    }
}

fn gerund(decision: &str) -> &str {
    match decision {
        "explore" => "exploring",
        "promote" => "promoting",
        "retreat" => "retreating",
        "hold" => "holding",
        "converged" => "converged",
        _ => decision,
    }
}
