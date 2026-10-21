//! Projection plans derive only from the admitted contract and immutable facts.

#[cfg(test)]
pub(in crate::gateway::shared) mod tests;

use std::collections::BTreeSet;

use bitrouter_ai::eval::{EVAL_SCHEMA_VERSION, EvalScope, EvalSubject};
use bitrouter_ai::gateway::execution::{AttemptState, ExecutionState, TerminalOutcome};

use super::*;
use crate::metering::shared::record::Record;
use crate::storage::evaluation::{canonical_digest, evidence_digest, validate_subject};
use crate::trajectory::types::TrajectoryEvidence;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub(in crate::gateway::shared) enum NoEvaluation {
    Bypassed { bypass: Bypass },
    SelectionNotReached,
    NoSelectedTier,
}

pub(in crate::gateway::shared) enum EvaluationInput {
    None(NoEvaluation),
    Request(Box<EvalSubject>),
    Trajectory,
}

impl Inputs {
    pub fn evaluation(&self, record: &Record, bound: bool) -> Result<EvaluationInput> {
        self.validate()?;
        self.check_identity(
            &record.event.execution.execution_id,
            &record.event.tenant_id,
            record.event.execution.operation,
        )?;
        let Phase::Terminal { route_ready } = self.phase else {
            anyhow::bail!("projection inputs are not terminal");
        };
        ensure!(
            route_ready
                || record
                    .event
                    .execution
                    .attempts
                    .iter()
                    .all(|attempt| { matches!(attempt.state, AttemptState::NotDispatched) }),
            "dispatched execution has no complete original routing evidence"
        );
        if bound {
            ensure!(
                self.requirements.trajectory == TrajectoryRequirement::GuardedPolicies
                    && self.requirements.policy_lock_digest.is_some()
                    && matches!(
                        self.policy,
                        Policy::Pending | Policy::Evaluated { guarded: true, .. }
                    ),
                "trajectory binding contradicts admitted projection requirements"
            );
            return Ok(EvaluationInput::Trajectory);
        }
        match &self.policy {
            Policy::Pending => Ok(EvaluationInput::None(NoEvaluation::SelectionNotReached)),
            Policy::Bypassed { reason } => Ok(EvaluationInput::None(NoEvaluation::Bypassed {
                bypass: *reason,
            })),
            Policy::Evaluated { guarded: true, .. } => {
                anyhow::bail!("guarded selection lost its trajectory binding")
            }
            Policy::Evaluated {
                treatment: None, ..
            } => Ok(EvaluationInput::None(NoEvaluation::NoSelectedTier)),
            Policy::Evaluated {
                treatment: Some(treatment),
                ..
            } => {
                let mut attributes = outcome_attributes(record)?;
                attributes.extend(treatment.attributes.clone());
                if let Some(effort) = treatment.decision.selected_effort {
                    attributes.insert("reasoning_effort".into(), effort.to_string());
                }
                let mut evidence = vec![EvidenceItem {
                    evidence_id: "request-outcome".into(),
                    kind: "request.outcome".into(),
                    digest: canonical_digest(&attributes)?,
                    redacted: true,
                    attributes,
                }];
                evidence.extend(self.routing.clone());
                for (facts, attempt) in record.attempts.iter().zip(&record.event.execution.attempts)
                {
                    let mut attributes = BTreeMap::new();
                    if let Some(attribution) = &facts.attribution {
                        attributes.insert("provider".into(), attribution.provider_id.clone());
                        attributes.insert("model".into(), attribution.model_id.clone());
                        attributes
                            .insert("outbound_protocol".into(), attribution.protocol.to_string());
                    }
                    let disposition = match &attempt.state {
                        AttemptState::Terminal { result } => {
                            for (key, value) in usage_fields(&result.usage) {
                                if let Some(value) = value {
                                    attributes.insert(key.into(), value.to_string());
                                }
                            }
                            if let Some(lifetime) = facts
                                .terminal
                                .as_ref()
                                .and_then(|facts| facts.observed_lifetime_ms)
                            {
                                attributes.insert(
                                    "observed_attempt_lifetime_ms".into(),
                                    lifetime.to_string(),
                                );
                            }
                            match &result.outcome {
                                TerminalOutcome::Succeeded => "succeeded",
                                TerminalOutcome::Failed { .. } => "failed",
                            }
                        }
                        AttemptState::NotDispatched => "not_dispatched",
                        AttemptState::Resolved { .. } => "operator_resolved",
                        _ => anyhow::bail!("request evaluation has unresolved attempt"),
                    };
                    attributes.insert("disposition".into(), disposition.into());
                    evidence.push(EvidenceItem {
                        evidence_id: format!("attempt:{}", attempt.attempt_id),
                        kind: "gateway.attempt".into(),
                        digest: canonical_digest(&(facts, attempt))?,
                        redacted: true,
                        attributes,
                    });
                }
                let subject = EvalSubject {
                    schema_version: EVAL_SCHEMA_VERSION,
                    eval_id: format!("request:{}", self.execution_id),
                    scope: EvalScope::Request,
                    subject_id: self.execution_id.clone(),
                    policy_digest: treatment.decision.policy_digest.clone(),
                    preset: treatment.preset.clone(),
                    cohort: None,
                    holdout: treatment.holdout,
                    decisions: vec![treatment.decision.clone()],
                    requested_dimensions: BTreeSet::from([
                        "cost.usd_micros".into(),
                        "latency.ms".into(),
                        "quality.pass".into(),
                    ]),
                    evidence_digest: evidence_digest(&evidence)?,
                    evidence,
                    observed_at: treatment.observed_at.clone(),
                };
                validate_subject(&subject)?;
                Ok(EvaluationInput::Request(Box::new(subject)))
            }
        }
    }

    /// A guarded route was committed under this original whole-document digest.
    pub fn check_trajectory_decision(&self, decision: &EvalDecisionRef) -> Result<()> {
        ensure!(
            self.requirements.policy_lock_digest.as_deref() == Some(&decision.policy_digest),
            "trajectory policy differs from admitted lock"
        );
        if let Policy::Evaluated {
            policy, treatment, ..
        } = &self.policy
        {
            ensure!(
                *policy == decision.policy,
                "trajectory policy differs from selected policy"
            );
            if let Some(treatment) = treatment {
                ensure!(
                    treatment.decision == *decision,
                    "trajectory route differs from captured treatment"
                );
            }
        }
        Ok(())
    }

    pub fn trajectory_evidence(&self, record: &Record) -> Result<TrajectoryEvidence> {
        let accounting = record.accounting()?;
        let mut structural = BTreeMap::new();
        for (key, value) in usage_fields(&accounting.usage) {
            if let Some(value) = value {
                structural.insert(format!("settlement.{key}"), value);
            }
        }
        structural.insert(
            "settlement.known_attempts".into(),
            u64::from(accounting.known_attempts),
        );
        structural.insert(
            "settlement.unknown_attempts".into(),
            u64::from(accounting.unknown_attempts),
        );
        let mut categorical = BTreeMap::from([
            (
                "settlement.usage_origin".into(),
                accounting.usage.origin.as_str().into(),
            ),
            ("settlement.outcome".into(), outcome(record)?.into()),
        ]);
        if let Policy::Evaluated {
            treatment: Some(treatment),
            ..
        } = &self.policy
        {
            for (key, value) in &treatment.attributes {
                if matches!(
                    key.as_str(),
                    "prediction_confidence_ppm"
                        | "task_family_confidence_ppm"
                        | "observation_confidence_ppm"
                ) {
                    structural.insert(format!("routing.{key}"), value.parse()?);
                } else {
                    categorical.insert(format!("routing.{key}"), value.clone());
                }
            }
        }
        Ok(TrajectoryEvidence {
            structural,
            categorical,
            digests: BTreeMap::new(),
            route_measurement: None,
            routing_evidence: self.routing.clone(),
        })
    }
}

fn usage_fields(
    usage: &bitrouter_ai::gateway::execution::ExecutionUsage,
) -> [(&'static str, Option<u64>); 7] {
    [
        ("prompt_tokens", usage.prompt_tokens),
        ("completion_tokens", usage.completion_tokens),
        ("reasoning_tokens", usage.reasoning_tokens),
        ("cache_read_tokens", usage.cache_read_tokens),
        ("cache_write_tokens", usage.cache_write_tokens),
        ("total_tokens", usage.total_tokens),
        ("cost_micro_usd", usage.cost_micro_usd),
    ]
}

fn outcome(record: &Record) -> Result<&'static str> {
    match &record.event.execution.state {
        ExecutionState::Terminal { result } => Ok(match result.outcome {
            TerminalOutcome::Succeeded => "settled",
            TerminalOutcome::Failed { .. } => "failed",
        }),
        ExecutionState::Resolved { .. } => Ok("operator_resolved"),
        _ => anyhow::bail!("evaluation requires terminal execution"),
    }
}

fn outcome_attributes(record: &Record) -> Result<BTreeMap<String, String>> {
    let accounting = record.accounting()?;
    let mut attributes = BTreeMap::from([
        ("outcome".into(), outcome(record)?.into()),
        (
            "usage_origin".into(),
            accounting.usage.origin.as_str().into(),
        ),
        (
            "known_subtotal_micro_usd".into(),
            accounting.known_subtotal_micro_usd.to_string(),
        ),
        (
            "known_attempts".into(),
            accounting.known_attempts.to_string(),
        ),
        (
            "unknown_attempts".into(),
            accounting.unknown_attempts.to_string(),
        ),
        (
            "not_dispatched_attempts".into(),
            accounting.not_dispatched_attempts.to_string(),
        ),
    ]);
    for (key, value) in usage_fields(&accounting.usage) {
        if let Some(value) = value {
            attributes.insert(key.into(), value.to_string());
        }
    }
    if let ExecutionState::Terminal { result } = &record.event.execution.state {
        attributes.insert(
            "success".into(),
            (result.outcome == TerminalOutcome::Succeeded).to_string(),
        );
        if let TerminalOutcome::Failed { code } = &result.outcome {
            attributes.insert("error_code".into(), code.clone());
        }
    }
    Ok(attributes)
}
