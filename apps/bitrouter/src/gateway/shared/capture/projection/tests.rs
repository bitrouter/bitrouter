use super::*;
use crate::metering::shared::tests::{fixture, no_dispatch, resolve};

pub(in crate::gateway::shared) fn selected(
    record: &Record,
    decision: EvalDecisionRef,
    guarded: bool,
) -> Result<Inputs> {
    let mut requirements = Requirements::fixture();
    requirements.policy_lock_digest = Some(decision.policy_digest.clone());
    if guarded {
        requirements.trajectory = TrajectoryRequirement::GuardedPolicies;
    }
    let inputs = Inputs {
        requirements,
        execution_id: record.event.execution.execution_id.clone(),
        owner: record.event.tenant_id.clone(),
        operation: record.event.execution.operation,
        phase: Phase::Terminal { route_ready: true },
        policy: Policy::Evaluated {
            policy: decision.policy.clone(),
            digest: decision.policy_digest.clone(),
            guarded,
            treatment: Some(Box::new(Treatment {
                decision,
                preset: None,
                holdout: false,
                attributes: BTreeMap::new(),
                observed_at: "1970-01-01T00:00:01Z".into(),
            })),
        },
        routing: vec![],
        response_observation: None,
    };
    inputs.validate()?;
    Ok(inputs)
}

pub(in crate::gateway::shared) fn untracked(record: &Record) -> Result<Inputs> {
    selected(
        record,
        EvalDecisionRef {
            decision_id: format!("decision:{}", record.event.execution.execution_id),
            policy: "auto:cost".into(),
            policy_digest: format!("sha256:{}", "a".repeat(64)),
            route_projection: "opening".into(),
            request_key: "opening".into(),
            selected_tier: "economy".into(),
            selected_effort: None,
            baseline_tier: Some("strong".into()),
            baseline_effort: None,
            experiment: None,
            route_measurement: None,
        },
        false,
    )
}

#[test]
fn request_subject_preserves_attempt_attribution_and_unknown_coverage() -> Result<()> {
    let mut record = fixture()?;
    record.request_id = None;
    let inputs = untracked(&record)?;
    let EvaluationInput::Request(subject) = inputs.evaluation(&record, false)? else {
        anyhow::bail!("missing subject");
    };
    validate_subject(&subject)?;
    ensure!(
        subject.evidence[0]
            .attributes
            .get("cost_micro_usd")
            .map(String::as_str)
            == Some("209")
    );
    ensure!(
        subject.evidence[1]
            .attributes
            .get("provider")
            .map(String::as_str)
            == Some("provider-0")
    );
    ensure!(
        subject.evidence[2]
            .attributes
            .get("provider")
            .map(String::as_str)
            == Some("provider-1")
    );
    ensure!(
        !subject.evidence[0].attributes.contains_key("provider")
            && !subject.evidence[0]
                .attributes
                .contains_key("request_duration_ms")
    );
    resolve(
        &mut record,
        bitrouter_ai::gateway::execution::ResolutionOutcome::Terminated,
    )?;
    let EvaluationInput::Request(subject) = inputs.evaluation(&record, false)? else {
        anyhow::bail!("missing resolved subject");
    };
    let summary = &subject.evidence[0].attributes;
    ensure!(summary.get("outcome").map(String::as_str) == Some("operator_resolved"));
    ensure!(summary.get("known_subtotal_micro_usd").map(String::as_str) == Some("19"));
    ensure!(summary.get("unknown_attempts").map(String::as_str) == Some("1"));
    ensure!(!summary.contains_key("cost_micro_usd") && !summary.contains_key("success"));
    ensure!(
        subject.evidence[2]
            .attributes
            .get("disposition")
            .map(String::as_str)
            == Some("operator_resolved")
    );
    ensure!(
        !subject.evidence[2]
            .attributes
            .contains_key("cost_micro_usd")
    );
    Ok(())
}

#[test]
fn incomplete_selection_is_not_a_successful_bypass_or_dispatched_treatment() -> Result<()> {
    let mut record = fixture()?;
    no_dispatch(&mut record)?;
    record.request_id = None;
    let mut inputs = Inputs::new(
        Requirements::fixture(),
        &record.event.execution.execution_id,
        &record.event.tenant_id,
        record.event.execution.operation,
    )?;
    inputs.seal()?;
    ensure!(matches!(
        inputs.evaluation(&record, false)?,
        EvaluationInput::None(NoEvaluation::SelectionNotReached)
    ));
    let mut dispatched = fixture()?;
    dispatched
        .event
        .execution
        .execution_id
        .clone_from(&record.event.execution.execution_id);
    ensure!(inputs.evaluation(&dispatched, false).is_err());
    let mut selected = untracked(&record)?;
    selected.phase = Phase::Terminal { route_ready: false };
    if let Policy::Evaluated { treatment, .. } = &mut selected.policy {
        *treatment = None;
    }
    ensure!(matches!(
        selected.evaluation(&record, false)?,
        EvaluationInput::None(NoEvaluation::NoSelectedTier)
    ));
    ensure!(selected.evaluation(&record, true).is_err());
    Ok(())
}
