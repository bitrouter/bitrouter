//! Durable model work and estimates; missing monetary evidence stays unknown.

use serde::{Deserialize, Serialize};

use super::workflow::ArtifactRef;
use super::{GoalError, GoalSnapshot, identifier, invalid};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accounting {
    pub work_started: u64,
    pub work_observed: u64,
    pub provider_attempts: u64,
    pub estimated_model_tokens_micro_usd: u64,
    pub unpriced_outcomes: u64,
    pub in_flight: Vec<Work>,
    pub recent: Vec<WorkOutcome>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkKind {
    Preparation,
    /// Waiting for other current workers' quotes, before provider dispatch.
    ResourceWait,
    InputCount,
    ContextValidation,
    ProviderAttempt,
    ProviderIntegration,
    TypedDecision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Work {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Box<crate::thread::GoalWork>>,
    pub id: String,
    pub kind: WorkKind,
    pub requirements_revision: u64,
    pub request_id: Option<String>,
    pub intent: ArtifactRef,
    #[serde(default)]
    pub token_reservation:
        Option<bitrouter_sdk::language_model::native_accounting::NativeTokenReservation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkOutcome {
    pub work: Work,
    pub report: ArtifactRef,
    pub estimated_model_tokens_micro_usd: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Begin {
        work: Work,
    },
    Observe {
        id: String,
        report: ArtifactRef,
        estimated_model_tokens_micro_usd: Option<u64>,
        #[serde(default)]
        reservation_verified: bool,
    },
}

pub(super) fn apply(goal: &mut GoalSnapshot, event: &Event, now_ms: u64) -> Result<(), GoalError> {
    match event {
        Event::Begin { work } => {
            if let Some(source) = &work.source {
                source.validate().map_err(|error| invalid(&error))?;
            }
            identifier(&work.id)?;
            work.intent.validate()?;
            super::budget::reserve(
                goal,
                &work.id,
                work.token_reservation.as_ref(),
                matches!(
                    work.kind,
                    WorkKind::ProviderAttempt | WorkKind::TypedDecision
                ),
                now_ms,
            )?;
            if work.requirements_revision == 0
                || work.requirements_revision > goal.requirements_revision
                || work.source.as_ref().map_or(
                    work.requirements_revision != goal.requirements_revision,
                    |source| !super::workflow::worker_requested(goal, source),
                )
                || goal.accounting.in_flight.len() >= 128
                || goal
                    .accounting
                    .in_flight
                    .iter()
                    .any(|active| active.id == work.id)
                || goal
                    .accounting
                    .recent
                    .iter()
                    .any(|prior| prior.work.id == work.id)
            {
                return Err(GoalError::Conflict);
            }
            goal.accounting.work_started = goal
                .accounting
                .work_started
                .checked_add(1)
                .ok_or_else(|| invalid("model work counter exhausted"))?;
            if work.kind == WorkKind::ProviderAttempt {
                goal.accounting.provider_attempts = goal
                    .accounting
                    .provider_attempts
                    .checked_add(1)
                    .ok_or_else(|| invalid("provider attempt counter exhausted"))?;
            }
            goal.accounting.in_flight.push(work.clone());
        }
        Event::Observe {
            id,
            report,
            estimated_model_tokens_micro_usd,
            reservation_verified,
        } => {
            report.validate()?;
            let index = goal
                .accounting
                .in_flight
                .iter()
                .position(|work| &work.id == id)
                .ok_or(GoalError::NotFound)?;
            let work = goal.accounting.in_flight.remove(index);
            super::budget::settle(
                goal,
                id,
                *estimated_model_tokens_micro_usd,
                *reservation_verified,
            );
            goal.accounting.work_observed = goal
                .accounting
                .work_observed
                .checked_add(1)
                .ok_or_else(|| invalid("model outcome counter exhausted"))?;
            if let Some(amount) = estimated_model_tokens_micro_usd {
                goal.accounting.estimated_model_tokens_micro_usd = goal
                    .accounting
                    .estimated_model_tokens_micro_usd
                    .checked_add(*amount)
                    .ok_or_else(|| invalid("model estimate total exhausted"))?;
            } else {
                goal.accounting.unpriced_outcomes = goal
                    .accounting
                    .unpriced_outcomes
                    .checked_add(1)
                    .ok_or_else(|| invalid("unpriced work counter exhausted"))?;
            }
            goal.accounting.recent.push(WorkOutcome {
                work,
                report: report.clone(),
                estimated_model_tokens_micro_usd: *estimated_model_tokens_micro_usd,
            });
            if goal.accounting.recent.len() > 8 {
                goal.accounting.recent.remove(0);
            }
        }
    }
    Ok(())
}
