//! A frozen Eval root is accepted only with its actual owned producer closure.
pub(in crate::tenancy) mod input;

use super::*;
use crate::{
    core::{
        eval::{compiler::EvalEvidenceSnapshot, store::EvalStore},
        optimization::cohort::{CohortAssessment, assess_cohort},
    },
    tenancy::execution::session::Binding,
    trajectory::store::owned::{OwnedStore, io::Budget},
};
use bitrouter_ai::tenancy::operation::ErrorCode;

fn unqualified() -> Error {
    Error::Rejected(ErrorCode::InvalidRequest)
}

pub(in crate::tenancy) async fn read(
    tx: &DatabaseTransaction,
    binding: &Binding,
    profile: &Profile,
    root: &str,
    shared: &OperationBudget<'_>,
) -> Result<CohortAssessment> {
    let input = acquire(tx, binding, profile, root, shared).await?;
    let (snapshot, fence) = input.into_parts();
    fence.check_selection(binding, profile, &snapshot)?;
    assess_cohort(&snapshot, &profile.policy_digest, &profile.treatment)
}

/// Acquire data and exact source fences under the caller's original H transaction.
/// This is evidence, not a grant: publication must reacquire and compare the fence
/// with current authority after computation. No caller can construct this input.
pub(in crate::tenancy) async fn acquire(
    tx: &DatabaseTransaction,
    binding: &Binding,
    profile: &Profile,
    root: &str,
    shared: &OperationBudget<'_>,
) -> Result<input::ValidatedInput> {
    acquire_reader(tx, Reader::Session(binding), profile, root, shared).await
}

pub(in crate::tenancy) async fn acquire_learner(
    tx: &DatabaseTransaction,
    identity: &crate::tenancy::learner::identity::ReadIdentity,
    profile: &Profile,
    root: &str,
    shared: &OperationBudget<'_>,
) -> Result<input::ValidatedInput> {
    acquire_reader(tx, Reader::Learner(identity), profile, root, shared).await
}

enum Reader<'a> {
    Session(&'a Binding),
    Learner(&'a crate::tenancy::learner::identity::ReadIdentity),
}
impl Reader<'_> {
    fn check(&self) -> Result<()> {
        match self {
            Self::Session(binding) => crate::tenancy::experiment::queue::check_reader(binding),
            Self::Learner(identity) => identity.check(&[]),
        }
        .map_err(Error::Rejected)?;
        Ok(())
    }
    fn scope(&self) -> Scope {
        match self {
            Self::Session(binding) => Scope::for_session(binding.authority()).0,
            Self::Learner(identity) => Scope::for_learner(identity).0,
        }
    }
    fn eval(&self) -> EvalStore {
        match self {
            Self::Session(binding) => EvalStore::for_native_session(binding.authority()),
            Self::Learner(identity) => EvalStore::for_learner(identity),
        }
    }
    fn trajectory(&self) -> OwnedStore {
        match self {
            Self::Session(binding) => OwnedStore::for_native_session(binding.authority()),
            Self::Learner(identity) => OwnedStore::for_learner(identity),
        }
    }
}
async fn acquire_reader(
    tx: &DatabaseTransaction,
    reader: Reader<'_>,
    profile: &Profile,
    root: &str,
    shared: &OperationBudget<'_>,
) -> Result<input::ValidatedInput> {
    reader.check()?;
    shared.check()?;
    let eval = reader.eval();
    let stored = eval
        .snapshot_records_native_in(tx, root, shared)
        .await?
        .ok_or(Error::Rejected(ErrorCode::NotFound))?;
    let mut snapshot = EvalEvidenceSnapshot::from_records(stored)?;
    // Other experiments/policies in a tenant-wide root are irrelevant. A copied
    // target label remains untrusted until the exact private producer is proved.
    snapshot.records.retain(|record| {
        record.subject.decisions.iter().any(|decision| {
            decision.route_projection == profile.treatment.target_request_key
                && decision.experiment.as_ref().is_some_and(|assignment| {
                    assignment.experiment_id == profile.treatment.experiment_id
                })
        })
    });
    let scope = reader.scope();
    let trajectory = reader.trajectory();
    let mut trajectory_budget = Budget::owned(shared);
    let mut units = std::collections::BTreeMap::new();
    let mut sources = input::Sources::default();
    for record in &snapshot.records {
        // Manual Generic records are valid Eval inputs, not authority for this
        // report. Reject them as input without poisoning H or a healthy Service.
        crate::trajectory::evaluation::validate_builtin_trajectory_result(&record.result)
            .map_err(|_| unqualified())?;
        let key = record
            .subject
            .subject_id
            .strip_prefix("episode_")
            .ok_or_else(unqualified)?;
        ensure!(
            key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit()),
            unqualified()
        );
        if !units.contains_key(key) {
            ensure!(units.len() < MAX_UNITS as usize, Error::Capacity);
            let row = records::point(tx, &scope, key, &[UNIT, UNIT_PENDING], shared)
                .await?
                .ok_or_else(unqualified)?;
            sources.record(&row, shared)?;
            let unit: Unit = records::decode(
                &row,
                &scope.physical_key(KeyDomain::NativeExperimentPending, "", &[])?,
                row.session_key.as_deref(),
            )?;
            ensure!(
                unit.profile == *profile && unit.episode == record.subject.subject_id,
                unqualified()
            );
            let session = row
                .session_key
                .as_deref()
                .context("native unit has no session")?;
            let steps = records::steps(tx, &scope, key, session, shared).await?;
            for (row, _) in &steps {
                sources.record(row, shared)?;
            }
            if let Reader::Learner(identity) = &reader {
                let loaded =
                    crate::managed_store::owned::load_for_learner_in(tx, identity, session, shared)
                        .await?
                        .ok_or_else(unqualified)?;
                ensure!(
                    loaded.descriptor.principal.as_str() == unit.principal
                        && loaded.descriptor.harness == unit.harness
                        && loaded.descriptor.session == unit.session,
                    unqualified()
                );
                identity.check_source(&loaded).map_err(Error::Rejected)?;
                let readback = crate::harness::resources::managed::store::model::Readback::stopped(
                    &loaded, &unit.run,
                )
                .map_err(|_| unqualified())?;
                for (_, step) in &steps {
                    let expected = step.ack.as_ref().ok_or_else(unqualified)?;
                    let current = readback
                        .settled(&step.facts, step.execution.as_str())
                        .map_err(|_| unqualified())?
                        .ok_or_else(unqualified)?;
                    ensure!(
                        current.terminal_step == expected.terminal_step
                            && current.planned == expected.planned
                            && current.interrupted == expected.interrupted
                            && current.head.execution_epoch == expected.head.execution_epoch
                            && current.head.state_revision >= expected.head.state_revision
                            && current.head.event_seq >= expected.head.event_seq,
                        unqualified()
                    );
                }
                sources.external(
                    "native_journal",
                    session,
                    crate::storage::evaluation::canonical_digest(&(
                        loaded.revision,
                        &loaded.descriptor,
                        &loaded.metadata,
                        &loaded.journal,
                    ))?,
                    shared,
                )?;
            }
            ensure!(
                steps.len() == unit.steps as usize
                    && !steps.is_empty()
                    && steps.iter().filter(|(_, step)| !step.complete()).count()
                        == unit.pending as usize,
                "native cohort unit count mismatch"
            );
            let counter_row = records::point(tx, &scope, &unit.counter, &[COUNTER], shared)
                .await?
                .context("native cohort counter absent")?;
            sources.record(&counter_row, shared)?;
            let counter: Counter = records::decode(&counter_row, &row.scope_id, None)?;
            ensure!(
                counter.version == 1
                    && counter.definition == profile.definition_digest
                    && counter.experiment == profile.treatment.experiment_id
                    && counter.accepted > 0
                    && counter.accepted <= MAX_UNITS
                    && counter.challenger <= counter.accepted,
                "native cohort counter closure mismatch"
            );
            units.insert(key.to_owned(), (unit, steps));
        }
        let (unit, steps) = units.get(key).context("native cohort unit disappeared")?;
        let mut matching = steps.iter().filter(|(_, step)| {
            matches!(&step.publication,
            Publication::Published { subject, result, .. }
                if subject == &record.subject.eval_id && result == &record.content_digest)
        });
        let (row, step) = matching
            .next()
            .context("built-in native publication lost its step")?;
        ensure!(
            matching.next().is_none(),
            "native publication belongs to two steps"
        );
        records::linked_unit(tx, &scope, row, step, shared).await?;
        ensure!(
            step.complete(),
            Error::Rejected(ErrorCode::AuthorityUnavailable)
        );
        let expected_receipt = step
            .receipt
            .as_ref()
            .context("native terminal receipt missing")?;
        let candidate = match &reader {
            Reader::Session(binding) => {
                crate::tenancy::receipts::store::native_candidate_in(
                    tx,
                    binding.authority(),
                    &step.execution,
                    shared,
                )
                .await?
            }
            Reader::Learner(identity) => {
                crate::tenancy::receipts::store::learner_candidate_in(
                    tx,
                    identity,
                    &step.execution,
                    shared,
                )
                .await?
            }
        }
        .context("native receipt absent")?;
        let (digest, dispatch) = candidate.experiment_disposition()?;
        ensure!(
            digest == expected_receipt.digest
                && dispatch == expected_receipt.dispatch
                && candidate.receipt.state == expected_receipt.state
                && candidate.receipt.execution_revision == profile.revision
                && candidate.receipt.principal_id.as_str() == unit.principal,
            "native cohort receipt changed"
        );
        for decision in &record.subject.decisions {
            if let Some(assignment) = &decision.experiment {
                ensure!(
                    assignment == &unit.assignment
                        && decision.policy_digest == profile.policy_digest,
                    "native cohort treatment changed"
                );
            }
        }
        let Publication::Published { event, outbox, .. } = &step.publication else {
            anyhow::bail!("native cohort publication disappeared");
        };
        let request = step
            .request
            .as_deref()
            .context("native cohort request missing")?;
        trajectory
            .check_native_publication_in(
                tx,
                &crate::tenancy::experiment::PublicationRef {
                    request,
                    event,
                    outbox,
                },
                record,
                &mut trajectory_budget,
            )
            .await?;
        shared.check()?;
    }
    let mut conclusive = 0usize;
    // The frozen operational root remains immutable. Current checked judgement
    // is a separate derived contribution, fenced by its exact effective head.
    for unit in units.keys() {
        let assessment_reader = match &reader {
            Reader::Session(binding) => super::completed::Reader::Session(binding),
            Reader::Learner(identity) => super::completed::Reader::Learner(identity),
        };
        let contribution = crate::tenancy::session::assessment::contribution::acquire(
            tx,
            &scope,
            unit,
            assessment_reader,
            shared,
        )
        .await?;
        sources.external(
            "native_assessment_head_v1",
            unit,
            contribution.head_digest,
            shared,
        )?;
        if contribution.conclusive {
            conclusive = conclusive.checked_add(1).ok_or(Error::Capacity)?;
        }
        if let Some(record) = contribution.record {
            snapshot.records.push(record);
        }
    }
    snapshot
        .records
        .sort_by(|left, right| left.result_id.cmp(&right.result_id));
    reader.check()?;
    input::ValidatedInput::new(
        &scope,
        profile,
        snapshot,
        sources,
        units.len(),
        conclusive,
        shared,
    )
}
