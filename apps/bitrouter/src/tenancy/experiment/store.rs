//! Strict bounded domain records; the caller owns one RR transaction and budget.
pub(in crate::tenancy) mod bound;
pub(in crate::tenancy) mod cohort;
pub(in crate::tenancy) mod completed;
mod records;
pub(in crate::tenancy) mod terminal;

use super::{Profile, Witness};
use crate::{
    harness::resources::managed::store::model::{Origin, StepFacts},
    harness::trajectory::owned::io,
    storage::{
        operation_budget::{Error, OperationBudget},
        scope::{KeyDomain, Scope},
    },
    tenancy::execution::TenantExecution,
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::{
    eval::{EvalExperimentRef, ExperimentArm, ExperimentAssignmentUnit},
    tenancy::{execution::ReceiptState, identity::ExecutionId, operation::DispatchState},
};
use sea_orm::DatabaseTransaction;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const RECORD_BYTES: usize = 8192;
const MAX_UNITS: u32 = 1024;
const MAX_STEPS: u32 = 128;
const COUNTER: &str = "native_experiment_counter_v1";
const UNIT_PENDING: &str = "native_experiment_unit_pending_v1";
const UNIT: &str = "native_experiment_unit_v1";
const STEP_PENDING: &str = "native_experiment_step_pending_v1";
const STEP: &str = "native_experiment_step_v1";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Counter {
    version: u32,
    experiment: String,
    definition: String,
    accepted: u32,
    challenger: u32,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Unit {
    version: u32,
    key: String,
    counter: String,
    principal: String,
    harness: String,
    session: String,
    run: String,
    pub(super) profile: Profile,
    pub(super) assignment: EvalExperimentRef,
    pub(super) episode: String,
    steps: u32,
    pending: u32,
}
impl Unit {
    pub(super) fn facts_match(&self, facts: &StepFacts) -> bool {
        self.run == facts.run && self.profile.route.resource_id.as_str() == facts.run_model
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptLink {
    digest: String,
    state: ReceiptState,
    dispatch: DispatchState,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum Publication {
    Awaiting,
    NotStarted,
    Published {
        event: String,
        subject: String,
        result: String,
        outbox: String,
    },
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    version: u32,
    unit: String,
    execution: ExecutionId,
    facts: StepFacts,
    request: Option<String>,
    receipt: Option<ReceiptLink>,
    publication: Publication,
    ack: Option<crate::harness::resources::managed::store::model::AckFacts>,
}
impl Step {
    fn complete(&self) -> bool {
        self.receipt.as_ref().is_some_and(|r| {
            r.state == ReceiptState::Committed
                || (r.state == ReceiptState::Incomplete && r.dispatch == DispatchState::NotStarted)
        }) && !matches!(self.publication, Publication::Awaiting)
            && self.ack.as_ref().is_some_and(|ack| {
                ack.planned
                    || self
                        .receipt
                        .as_ref()
                        .is_some_and(|r| r.dispatch == DispatchState::NotStarted)
            })
    }
    fn reserve(&self) -> Result<()> {
        // Bound serialized cells, including JSON escaping, before substituting
        // the full terminal shape. All producer-generated IDs fit this bound.
        let check = |value: &str| -> Result<()> {
            io::json(&value, 258).map_err(|_| Error::Capacity)?;
            Ok(())
        };
        if let Some(value) = &self.request {
            check(value)?;
        }
        if let Publication::Published {
            event,
            subject,
            result,
            outbox,
        } = &self.publication
        {
            for value in [event, subject, result, outbox] {
                check(value)?;
            }
        }
        if let Some(ack) = &self.ack {
            check(&ack.terminal_step)?;
            if let Some(id) = &ack.head.batch_id {
                check(id)?;
            }
        }
        io::json(self, RECORD_BYTES)?;
        let mut complete = self.clone();
        complete.receipt = Some(ReceiptLink {
            digest: "f".repeat(64),
            state: ReceiptState::Incomplete,
            dispatch: DispatchState::NotStarted,
        });
        // All future variable strings are bounded at their producers and checked
        // again by each participant. Reserve each pending step's complete shape.
        complete.publication = Publication::Published {
            event: "x".repeat(256),
            subject: "x".repeat(256),
            result: "x".repeat(256),
            outbox: "x".repeat(256),
        };
        complete.request = Some("x".repeat(256));
        complete.ack = Some(crate::harness::resources::managed::store::model::AckFacts {
            terminal_step: "x".repeat(256),
            planned: false,
            interrupted: false,
            head: bitrouter_ai::core::checkpoint::DurableHead {
                execution_epoch: u64::MAX,
                state_revision: u64::MAX,
                event_seq: u64::MAX,
                batch_id: Some("x".repeat(256)),
                payload_sha256: Some("f".repeat(64)),
            },
        });
        io::json(&complete, RECORD_BYTES).map_err(|_| Error::Capacity)?;
        Ok(())
    }
}

/// Native owner/current row is locked before the shared counter. No standalone
/// commit, second writer or provider work is permitted in this participant.
pub(in crate::tenancy) async fn enroll(
    tx: &DatabaseTransaction,
    execution: &TenantExecution,
    origin: &Arc<Origin>,
    profile: &Profile,
    budget: &OperationBudget<'_>,
) -> Result<Witness> {
    let binding = origin.binding();
    let loaded = crate::managed_store::owned::load_for_model_in(tx, binding.authority(), budget)
        .await?
        .context("native enrollment source is absent")?;
    ensure!(
        &loaded.descriptor == binding.descriptor(),
        "native enrollment source descriptor changed"
    );
    origin
        .validate_loaded(&loaded.journal)
        .map_err(|_| anyhow::anyhow!("native enrollment source is not the preparing ACK"))?;
    let (scope, provenance) = Scope::for_session(binding.authority());
    let partition = scope.physical_key(KeyDomain::NativeExperimentPending, "", &[])?;
    let counter_key = scope.physical_key(
        KeyDomain::NativeExperimentCounter,
        "",
        &[&profile.treatment.experiment_id],
    )?;
    let descriptor = binding.descriptor();
    let facts = origin.facts();
    let unit_key = scope.physical_key(
        KeyDomain::NativeExperimentUnit,
        "",
        &[
            descriptor.principal.as_str(),
            &descriptor.harness,
            &descriptor.session,
            &facts.run,
        ],
    )?;
    let step_key = scope.physical_key(
        KeyDomain::NativeExperimentStep,
        "",
        &[execution.id().as_str()],
    )?;
    let counter_row = records::point(tx, &scope, &counter_key, &[COUNTER], budget).await?;
    let mut counter = match &counter_row {
        Some(row) => records::decode::<Counter>(row, &partition, None)?,
        None => Counter {
            version: 1,
            experiment: profile.treatment.experiment_id.clone(),
            definition: profile.definition_digest.clone(),
            accepted: 0,
            challenger: 0,
        },
    };
    ensure!(
        counter.version == 1
            && counter.experiment == profile.treatment.experiment_id
            && counter.accepted <= MAX_UNITS
            && counter.challenger <= counter.accepted
            && counter.challenger <= profile.treatment.gate.maximum_challenger_tasks,
        "native experiment counter identity or range conflict"
    );
    ensure!(
        counter.definition == profile.definition_digest,
        Error::Rejected(bitrouter_ai::tenancy::operation::ErrorCode::OperationConflict)
    );
    let unit_row = records::point(tx, &scope, &unit_key, &[UNIT, UNIT_PENDING], budget).await?;
    let assignment = profile
        .treatment
        .assignment_for(ExperimentAssignmentUnit::Episode, &unit_key);
    let native_key = scope.physical_key(
        KeyDomain::NativeSession,
        "",
        &[descriptor.principal.as_str(), &descriptor.name],
    )?;
    let session_pending = records::pending_session(tx, &scope, &native_key, budget).await?;
    ensure!(
        session_pending.len() < MAX_STEPS as usize
            || session_pending
                .iter()
                .any(|(_, step)| step.execution == *execution.id()),
        Error::Capacity
    );
    let mut unit = match &unit_row {
        Some(row) => records::decode::<Unit>(row, &partition, Some(&native_key))?,
        None => Unit {
            version: 1,
            key: unit_key.clone(),
            counter: counter_key.clone(),
            principal: descriptor.principal.to_string(),
            harness: descriptor.harness.clone(),
            session: descriptor.session.clone(),
            run: facts.run.clone(),
            profile: profile.clone(),
            assignment: assignment.clone(),
            episode: format!("episode_{unit_key}"),
            steps: 0,
            pending: 0,
        },
    };
    ensure!(
        unit.version == 1
            && unit.key == unit_key
            && unit.counter == counter_key
            && unit.principal == descriptor.principal.as_str()
            && unit.harness == descriptor.harness
            && unit.session == descriptor.session
            && unit.run == facts.run
            && unit.profile == *profile
            && unit.assignment == assignment
            && unit.episode == format!("episode_{unit_key}")
            && unit.steps <= MAX_STEPS
            && unit.pending <= unit.steps,
        "native experiment unit identity conflict"
    );
    if let Some(row) = &unit_row {
        ensure!(
            counter_row.is_some()
                && row.kind
                    == if unit.pending == 0 {
                        UNIT
                    } else {
                        UNIT_PENDING
                    },
            "native experiment unit counter or phase is absent"
        );
    }
    let steps = records::steps(tx, &scope, &unit_key, &native_key, budget).await?;
    ensure!(
        steps.len() == unit.steps as usize
            && steps.iter().filter(|(_, s)| !s.complete()).count() == unit.pending as usize,
        "native experiment unit links are incomplete"
    );
    if let Some((_, previous)) = steps
        .iter()
        .find(|(_, s)| s.facts.agent == facts.agent && s.facts.step == facts.step)
    {
        ensure!(
            previous.execution == *execution.id() && previous.facts == *facts,
            "native model step cannot be claimed by another execution"
        );
        ensure!(
            counter_row.is_some() && unit_row.is_some(),
            "native enrollment parents absent"
        );
        return Ok(Witness {
            execution: execution.id().clone(),
            unit,
            origin: origin.clone(),
        });
    }
    ensure!(unit.steps < MAX_STEPS, Error::Capacity);
    let step = Step {
        version: 1,
        unit: unit_key.clone(),
        execution: execution.id().clone(),
        facts: facts.clone(),
        request: None,
        receipt: None,
        publication: Publication::Awaiting,
        ack: None,
    };
    step.reserve()?;
    for (_, pending) in &steps {
        pending.reserve()?;
    }
    // Fixed 8 KiB reservations for at most128 steps stay within1 MiB as a unit.
    ensure!(
        (unit.steps as usize + 1)
            .checked_mul(RECORD_BYTES)
            .is_some_and(|n| n <= 1024 * 1024),
        Error::Capacity
    );
    if unit_row.is_none() {
        ensure!(counter.accepted < MAX_UNITS, Error::Capacity);
        if assignment.arm == ExperimentArm::Challenger {
            ensure!(
                counter.challenger < profile.treatment.gate.maximum_challenger_tasks,
                Error::Capacity
            );
            counter.challenger += 1;
        }
        counter.accepted += 1;
        records::put(
            tx,
            &scope,
            &provenance,
            records::Slot {
                id: &counter_key,
                partition: &partition,
                kind: COUNTER,
                session: None,
                previous: counter_row.as_ref(),
            },
            &counter,
            budget,
        )
        .await?;
    }
    unit.steps += 1;
    unit.pending += 1;
    records::put(
        tx,
        &scope,
        &provenance,
        records::Slot {
            id: &unit_key,
            partition: &partition,
            kind: UNIT_PENDING,
            session: Some(&native_key),
            previous: unit_row.as_ref(),
        },
        &unit,
        budget,
    )
    .await?;
    records::put(
        tx,
        &scope,
        &provenance,
        records::Slot {
            id: &step_key,
            partition: &unit_key,
            kind: STEP_PENDING,
            session: Some(&native_key),
            previous: None,
        },
        &step,
        budget,
    )
    .await?;
    budget.check()?;
    Ok(Witness {
        execution: execution.id().clone(),
        unit,
        origin: origin.clone(),
    })
}
