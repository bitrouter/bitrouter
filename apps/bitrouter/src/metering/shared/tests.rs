//! Domain evidence tests independent of a database or running gateway.

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::execution::{
    AttemptState, ExecutionState, GatewayAttempt, GatewayExecution, GatewayTerminalEvent,
    OperatorResolution, ResolutionOutcome, TerminalObservation, TerminalOutcome,
};
use bitrouter_ai::types::{ModelOperation, Usage, UsageOrigin};

use super::evidence::{Attribution, ObservedTerminal, aggregate_usage};
use super::record::{AttemptEvidence, Record};
use crate::metering::{pricing::ModelPricing, tariff::FrozenTariff};

pub(crate) fn fixture() -> Result<Record> {
    let mut attempts = Vec::new();
    let mut facts = Vec::new();
    for (index, multiplier) in [1.0, 10.0].into_iter().enumerate() {
        let mut attribution = Attribution::fixture();
        attribution.provider_id = format!("provider-{index}");
        attribution.model_id = format!("model-{index}");
        attribution.tariff = Some(FrozenTariff {
            registry_price: None,
            protocol: attribution.protocol.clone(),
            endpoint_profile: "fixture".into(),
            tariff_profile: Some("fixture".into()),
            pricing: Some(ModelPricing::cache_aware(
                Some(2.0 * multiplier),
                Some(0.5 * multiplier),
                Some(multiplier),
                Some(3.0 * multiplier),
            )),
            pricing_version: format!("original-{index}"),
            billing_basis: "normalized-disjoint-usage-v1".into(),
            unavailable_reason: None,
        });
        let mut observed = ObservedTerminal::capture(
            TerminalOutcome::Succeeded,
            Some(&Usage {
                prompt_tokens: 7,
                completion_tokens: 3,
                reasoning_tokens: 1,
                cache_read_tokens: 2,
                cache_write_tokens: 1,
                origin: UsageOrigin::ProviderReported,
                ..Usage::default()
            }),
            &attribution,
            Some(11),
        );
        observed.observation.recorded_at_ms = 1500;
        let id = format!("attempt-{index}");
        attempts.push(GatewayAttempt {
            attempt_id: id.clone(),
            resource_pool_id: "pool".into(),
            credential_revision: 1,
            state: AttemptState::Terminal {
                result: observed.observation,
            },
        });
        facts.push(AttemptEvidence {
            execution_id: "execution".into(),
            attempt_id: id,
            attribution: Some(Box::new(attribution)),
            terminal: Some(Box::new(observed.facts)),
        });
    }
    let mut record = Record::new(
        GatewayTerminalEvent {
            event_id: "event".into(),
            tenant_id: "tenant".into(),
            execution: GatewayExecution {
                execution_id: "execution".into(),
                correlation_id: "correlation".into(),
                revision: 1,
                operation: ModelOperation::Generation,
                config_generation: 1,
                accepted_at_ms: 1000,
                updated_at_ms: 2000,
                state: ExecutionState::Terminal {
                    result: TerminalObservation {
                        outcome: TerminalOutcome::Succeeded,
                        usage: aggregate_usage(
                            &attempts
                                .iter()
                                .filter_map(|attempt| match &attempt.state {
                                    AttemptState::Terminal { result } => Some(&result.usage),
                                    _ => None,
                                })
                                .collect::<Vec<_>>(),
                        )?,
                        recorded_at_ms: 2000,
                    },
                },
                attempts,
            },
        },
        "principal".into(),
        Some("request".into()),
        facts,
    )?;
    rename(&mut record, &format!("execution-{}", uuid::Uuid::new_v4()));
    Ok(record)
}

pub(crate) fn rename(record: &mut Record, id: &str) {
    record.event.execution.execution_id = id.into();
    record.event.event_id = format!("event-{id}");
    record.request_id = Some(format!("request-{id}"));
    for fact in &mut record.attempts {
        fact.execution_id = id.into();
    }
}

pub(crate) fn resolve(record: &mut Record, outcome: ResolutionOutcome) -> Result<()> {
    let resolution = OperatorResolution {
        operator_id: "operator".into(),
        outcome,
        evidence_reference: "case-123".into(),
        recorded_at_ms: 2000,
    };
    record
        .event
        .execution
        .attempts
        .last_mut()
        .context("missing attempt")?
        .state = AttemptState::Resolved {
        resolution: resolution.clone(),
    };
    record
        .attempts
        .last_mut()
        .context("missing facts")?
        .terminal = None;
    record.event.execution.state = ExecutionState::Resolved { resolution };
    Ok(())
}

#[test]
fn original_models_and_unknown_resolution_keep_independent_costs() -> Result<()> {
    let record = fixture()?;
    let accounting = record.accounting()?;
    ensure!(accounting.known_subtotal_micro_usd == 209);
    ensure!(accounting.usage.cost_micro_usd == Some(209));
    ensure!(
        record.attempts[0]
            .attribution
            .as_ref()
            .context("missing")?
            .provider_id
            == "provider-0"
    );
    ensure!(
        record.attempts[1]
            .attribution
            .as_ref()
            .context("missing")?
            .model_id
            == "model-1"
    );
    for outcome in [
        ResolutionOutcome::NotAccepted,
        ResolutionOutcome::Terminated,
    ] {
        let mut partial = record.clone();
        resolve(&mut partial, outcome)?;
        let accounting = partial.accounting()?;
        ensure!(accounting.known_subtotal_micro_usd == 19);
        ensure!(accounting.known_attempts == 1 && accounting.unknown_attempts == 1);
        ensure!(accounting.usage.cost_micro_usd.is_none());
        ensure!(accounting.usage.prompt_tokens.is_none());
    }
    Ok(())
}

#[test]
fn missing_prices_preserve_usage_and_known_subtotal() -> Result<()> {
    let mut record = fixture()?;
    let facts = record.attempts.last_mut().context("missing")?;
    let attribution = facts.attribution.as_mut().context("missing")?;
    attribution.tariff = None;
    let mut evidence = ObservedTerminal::capture(
        TerminalOutcome::Succeeded,
        Some(&Usage {
            prompt_tokens: 7,
            completion_tokens: 3,
            origin: UsageOrigin::Estimated,
            ..Usage::default()
        }),
        attribution,
        Some(13),
    );
    evidence.observation.recorded_at_ms = 1500;
    record
        .event
        .execution
        .attempts
        .last_mut()
        .context("missing")?
        .state = AttemptState::Terminal {
        result: evidence.observation,
    };
    facts.terminal = Some(Box::new(evidence.facts));
    reset_aggregate(&mut record)?;
    let accounting = record.accounting()?;
    ensure!(accounting.usage.prompt_tokens == Some(14));
    ensure!(accounting.usage.origin == UsageOrigin::Unknown); // Mixed provenance.
    ensure!(accounting.usage.cost_micro_usd.is_none());
    ensure!(accounting.known_subtotal_micro_usd == 19 && accounting.unknown_attempts == 1);
    Ok(())
}

#[test]
fn no_dispatch_is_present_without_fabricated_charge() -> Result<()> {
    let mut record = fixture()?;
    no_dispatch(&mut record)?;
    ensure!(record.accounting()?.usage.cost_micro_usd.is_none());
    ensure!(record.accounting()?.known_attempts == 0);
    let actual_id = "unsent-attempt";
    record.attempts.push(AttemptEvidence {
        execution_id: record.event.execution.execution_id.clone(),
        attempt_id: actual_id.into(),
        attribution: None,
        terminal: None,
    });
    record.event.execution.attempts.push(GatewayAttempt {
        attempt_id: actual_id.into(),
        resource_pool_id: "pool".into(),
        credential_revision: 1,
        state: AttemptState::NotDispatched,
    });
    let accounting = record.accounting()?;
    ensure!(accounting.not_dispatched_attempts == 1 && accounting.unknown_attempts == 0);
    ensure!(accounting.usage.cost_micro_usd.is_none());
    Ok(())
}

#[test]
fn malformed_membership_missing_facts_and_conflicting_aggregate_reject() -> Result<()> {
    let record = fixture()?;
    let mut bad = record.clone();
    bad.attempts[0].execution_id = "foreign".into();
    ensure!(bad.accounting().is_err());
    let mut bad = record.clone();
    bad.attempts[1] = bad.attempts[0].clone();
    bad.event.execution.attempts[1] = bad.event.execution.attempts[0].clone();
    ensure!(bad.accounting().is_err());
    let mut bad = record.clone();
    bad.attempts[0].terminal = None;
    ensure!(bad.accounting().is_err());
    let mut bad = record.clone();
    if let ExecutionState::Terminal { result } = &mut bad.event.execution.state {
        result.usage.cost_micro_usd = Some(0);
    }
    ensure!(bad.accounting().is_err());
    let mut bad = record.clone();
    for index in 2..=super::MAX_ATTEMPTS {
        let mut fact = record.attempts[0].clone();
        let mut attempt = record.event.execution.attempts[0].clone();
        attempt.attempt_id = format!("attempt-{index}");
        fact.attempt_id = attempt.attempt_id.clone();
        bad.attempts.push(fact);
        bad.event.execution.attempts.push(attempt);
    }
    ensure!(bad.accounting().is_err());
    Ok(())
}

#[test]
fn checked_fold_does_not_saturate_overflow() -> Result<()> {
    let mut a = aggregate_usage(&[])?;
    a.cost_micro_usd = Some(u64::MAX);
    let mut b = a.clone();
    b.cost_micro_usd = Some(1);
    ensure!(aggregate_usage(&[&a, &b]).is_err());
    Ok(())
}

fn reset_aggregate(record: &mut Record) -> Result<()> {
    let usage = aggregate_usage(
        &record
            .event
            .execution
            .attempts
            .iter()
            .filter_map(|attempt| match &attempt.state {
                AttemptState::Terminal { result } => Some(&result.usage),
                _ => None,
            })
            .collect::<Vec<_>>(),
    )?;
    record.event.execution.state = ExecutionState::Terminal {
        result: TerminalObservation {
            outcome: TerminalOutcome::Failed {
                code: "fixture".into(),
            },
            usage,
            recorded_at_ms: 2000,
        },
    };
    Ok(())
}

pub(crate) fn no_dispatch(record: &mut Record) -> Result<()> {
    record.attempts.clear();
    record.event.execution.attempts.clear();
    reset_aggregate(record)
}

#[test]
fn terminal_disposition_requires_matching_observations() -> Result<()> {
    let mut record = fixture()?;
    no_dispatch(&mut record)?;
    if let ExecutionState::Terminal { result } = &mut record.event.execution.state {
        result.outcome = TerminalOutcome::Succeeded;
    }
    ensure!(record.accounting().is_err());
    let mut record = fixture()?;
    resolve(&mut record, ResolutionOutcome::Terminated)?;
    let mut other = match &record.event.execution.state {
        ExecutionState::Resolved { resolution } => resolution.clone(),
        _ => anyhow::bail!("fixture not resolved"),
    };
    other.evidence_reference = "different-case".into();
    record.attempts[0].terminal = None;
    record.event.execution.attempts[0].state = AttemptState::Resolved { resolution: other };
    ensure!(record.accounting().is_err());
    Ok(())
}

pub(crate) fn signed_zero(record: &mut Record, rate: f64) -> Result<()> {
    let facts = record
        .attempts
        .first_mut()
        .context("missing attempt facts")?;
    let attribution = facts.attribution.as_mut().context("missing attribution")?;
    attribution
        .tariff
        .as_mut()
        .context("missing tariff")?
        .pricing
        .as_mut()
        .context("missing price")?
        .input_micro_usd_per_token = Some(rate);
    let mut observed = ObservedTerminal::capture(
        TerminalOutcome::Succeeded,
        Some(&Usage {
            prompt_tokens: 7,
            completion_tokens: 3,
            reasoning_tokens: 1,
            cache_read_tokens: 2,
            cache_write_tokens: 1,
            origin: UsageOrigin::ProviderReported,
            ..Usage::default()
        }),
        attribution,
        Some(11),
    );
    observed.observation.recorded_at_ms = 1500;
    facts.terminal = Some(Box::new(observed.facts));
    record
        .event
        .execution
        .attempts
        .first_mut()
        .context("missing attempt")?
        .state = AttemptState::Terminal {
        result: observed.observation,
    };
    reset_aggregate(record)
}

pub(crate) fn boundary_record() -> Result<Record> {
    let mut record = fixture()?;
    record.attempts.clear();
    record.event.execution.attempts.clear();
    let attribution = Attribution::boundary_fixture()?;
    let mut observed = ObservedTerminal::capture(
        TerminalOutcome::Succeeded,
        Some(&Usage {
            prompt_tokens: 7,
            completion_tokens: 3,
            origin: UsageOrigin::ProviderReported,
            ..Usage::default()
        }),
        &attribution,
        Some(11),
    );
    observed.observation.recorded_at_ms = 1500;
    ensure!(serde_json::to_vec(&observed.facts)?.len() > 64 * 1024);
    for index in 0..super::MAX_ATTEMPTS {
        let id = format!("attempt-{index}");
        record.attempts.push(AttemptEvidence {
            execution_id: record.event.execution.execution_id.clone(),
            attempt_id: id.clone(),
            attribution: Some(Box::new(attribution.clone())),
            terminal: Some(Box::new(observed.facts.clone())),
        });
        record.event.execution.attempts.push(GatewayAttempt {
            attempt_id: id,
            resource_pool_id: "pool".into(),
            credential_revision: 1,
            state: AttemptState::Terminal {
                result: observed.observation.clone(),
            },
        });
    }
    reset_aggregate(&mut record)?;
    record.accounting()?;
    ensure!(serde_json::to_vec(&record)?.len() > 32 * 128 * 1024);
    Ok(record)
}
