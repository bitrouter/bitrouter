//! One immutable execution, retaining each attempt's original accounting basis.

use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::execution::{
    AttemptState, ExecutionState, ExecutionUsage, GatewayTerminalEvent, TerminalOutcome,
};
use serde::{Deserialize, Serialize};

use super::MAX_ATTEMPTS;
use super::evidence::{
    Attribution, MAX_ATTRIBUTION_BYTES, MAX_TERMINAL_BYTES, ObservedTerminal, TerminalFacts,
    aggregate_usage, validate_outcome,
};

// Each entry contains the two admitted evidence envelopes plus a bounded public
// attempt. The event repeats only the public attempt list, never the tariffs.
pub(super) const MAX_RECORD_BYTES: usize =
    MAX_ATTEMPTS * (MAX_ATTRIBUTION_BYTES + MAX_TERMINAL_BYTES + 8192) + 16 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttemptEvidence {
    pub execution_id: String,
    pub attempt_id: String,
    pub attribution: Option<Box<Attribution>>,
    pub terminal: Option<Box<TerminalFacts>>,
}

/// Trusted gateway persistence constructs this input, never an HTTP caller.
/// Missing legacy terminal evidence fails validation and leaves projection pending.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    format: u8,
    pub event: GatewayTerminalEvent,
    pub principal_id: String,
    /// Exact trajectory binding, when present. Caller correlation is not a key.
    pub request_id: Option<String>,
    pub attempts: Vec<AttemptEvidence>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Accounting {
    pub usage: ExecutionUsage,
    pub known_subtotal_micro_usd: u64,
    pub known_attempts: u32,
    pub unknown_attempts: u32,
    pub not_dispatched_attempts: u32,
}

impl Record {
    pub fn new(
        event: GatewayTerminalEvent,
        principal_id: String,
        request_id: Option<String>,
        attempts: Vec<AttemptEvidence>,
    ) -> Result<Self> {
        let record = Self {
            format: 1,
            event,
            principal_id,
            request_id,
            attempts,
        };
        record.accounting()?;
        Ok(record)
    }

    /// All writes and exact reads use this same checked fold. Costs are added
    /// from frozen per-attempt evidence, never from aggregated tokens or repricing.
    pub fn accounting(&self) -> Result<Accounting> {
        ensure!(self.format == 1, "unsupported shared metering format");
        let execution = &self.event.execution;
        for id in [
            &self.event.event_id,
            &self.event.tenant_id,
            &execution.execution_id,
            &self.principal_id,
        ] {
            validate_id(id)?;
        }
        if let Some(id) = &self.request_id {
            validate_id(id)?;
        }
        ensure!(
            execution.revision > 0
                && execution.config_generation > 0
                && execution.accepted_at_ms >= 0
                && execution.updated_at_ms >= execution.accepted_at_ms,
            "invalid metering source revision or times"
        );
        ensure!(
            self.attempts.len() <= MAX_ATTEMPTS && self.attempts.len() == execution.attempts.len(),
            "metering attempt membership differs from execution"
        );
        let mut ids = BTreeSet::new();
        let mut usages = Vec::new();
        let unknown_usage = aggregate_usage(&[])?;
        let mut result = Accounting {
            usage: unknown_usage.clone(),
            known_subtotal_micro_usd: 0,
            known_attempts: 0,
            unknown_attempts: 0,
            not_dispatched_attempts: 0,
        };
        for (facts, attempt) in self.attempts.iter().zip(&execution.attempts) {
            validate_id(&attempt.attempt_id)?;
            validate_id(&attempt.resource_pool_id)?;
            ensure!(
                ids.insert(&attempt.attempt_id)
                    && facts.attempt_id == attempt.attempt_id
                    && facts.execution_id == execution.execution_id
                    && attempt.credential_revision > 0,
                "invalid metering attempt identity"
            );
            if let Some(attribution) = &facts.attribution {
                attribution.validate()?;
            }
            match &attempt.state {
                AttemptState::Terminal { result: observed } => {
                    validate_outcome(&observed.outcome)?;
                    ensure!(
                        (execution.accepted_at_ms..=execution.updated_at_ms)
                            .contains(&observed.recorded_at_ms),
                        "attempt observation time is outside execution"
                    );
                    let evidence = ObservedTerminal {
                        observation: observed.clone(),
                        facts: *facts.terminal.clone().context("missing terminal facts")?,
                    };
                    evidence.validate(
                        facts
                            .attribution
                            .as_deref()
                            .context("missing attribution")?,
                    )?;
                    usages.push(&observed.usage);
                    if let Some(amount) = observed.usage.cost_micro_usd {
                        result.known_subtotal_micro_usd = result
                            .known_subtotal_micro_usd
                            .checked_add(amount)
                            .context("shared metering cost overflow")?;
                        result.known_attempts += 1;
                    } else {
                        result.unknown_attempts += 1;
                    }
                }
                AttemptState::NotDispatched => {
                    ensure!(
                        facts.terminal.is_none(),
                        "unsent attempt has terminal facts"
                    );
                    result.not_dispatched_attempts += 1;
                }
                AttemptState::Resolved { resolution } => {
                    ensure!(
                        facts.terminal.is_none(),
                        "operator assertion has provider facts"
                    );
                    validate_resolution(resolution, execution)?;
                    // Neither operator outcome contains monetary evidence, even
                    // NotAccepted. Do not fabricate NotCharged or a zero receipt.
                    usages.push(&unknown_usage);
                    result.unknown_attempts += 1;
                }
                _ => anyhow::bail!("unresolved attempt cannot enter metering projection"),
            }
        }
        result.usage = aggregate_usage(&usages)?;
        match &execution.state {
            ExecutionState::Terminal { result: terminal } => {
                validate_outcome(&terminal.outcome)?;
                ensure!(
                    terminal.recorded_at_ms == execution.updated_at_ms
                        && terminal.usage == result.usage
                        && execution
                            .attempts
                            .iter()
                            .all(|attempt| !matches!(attempt.state, AttemptState::Resolved { .. })),
                    "terminal metering aggregate differs from execution"
                );
                if terminal.outcome == TerminalOutcome::Succeeded {
                    ensure!(
                        execution
                            .attempts
                            .iter()
                            .rev()
                            .find_map(|attempt| match &attempt.state {
                                AttemptState::Terminal { result } => Some(&result.outcome),
                                _ => None,
                            })
                            == Some(&TerminalOutcome::Succeeded),
                        "successful execution has no successful final observation"
                    );
                }
            }
            ExecutionState::Resolved { resolution } => {
                validate_resolution(resolution, execution)?;
                ensure!(
                    resolution.recorded_at_ms == execution.updated_at_ms
                        && execution
                            .attempts
                            .iter()
                            .filter(|attempt| matches!(
                                &attempt.state,
                                AttemptState::Resolved { resolution: other } if other == resolution
                            ))
                            .count()
                            == 1
                        && execution
                            .attempts
                            .iter()
                            .filter(|attempt| matches!(
                                attempt.state,
                                AttemptState::Resolved { .. }
                            ))
                            .count()
                            == 1,
                    "execution resolution differs from attempt"
                );
            }
            _ => anyhow::bail!("metering requires a terminal execution"),
        }
        let bytes = serde_json::to_vec(self)?;
        ensure!(
            bytes.len() <= MAX_RECORD_BYTES,
            "metering record exceeds byte limit"
        );
        ensure!(
            serde_json::from_slice::<Self>(&bytes)? == *self,
            "metering record is not lossless JSON"
        );
        Ok(result)
    }
}

fn validate_resolution(
    resolution: &bitrouter_ai::gateway::execution::OperatorResolution,
    execution: &bitrouter_ai::gateway::execution::GatewayExecution,
) -> Result<()> {
    validate_id(&resolution.operator_id)?;
    ensure!(
        !resolution.evidence_reference.is_empty()
            && resolution.evidence_reference.len() <= 1024
            && !resolution.evidence_reference.chars().any(char::is_control)
            && (execution.accepted_at_ms..=execution.updated_at_ms)
                .contains(&resolution.recorded_at_ms),
        "invalid operator resolution evidence"
    );
    Ok(())
}

pub(super) fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control),
        "invalid shared metering identity"
    );
    Ok(())
}
