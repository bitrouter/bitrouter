//! Participants in the original receipt, trajectory and native journal writes.
use super::*;
use crate::{
    harness::resources::managed::store::model::Readback,
    tenancy::{
        execution::session::Binding,
        receipts::{owner::Owner, store::Candidate},
    },
    trajectory::types::Settlement,
};

pub(in crate::tenancy) async fn receipt(
    tx: &DatabaseTransaction,
    owner: &Owner,
    candidate: &Candidate,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    let (scope, provenance) = Scope::for_receipt(owner);
    let receipt = &candidate.receipt;
    let id = scope.physical_key(
        KeyDomain::NativeExperimentStep,
        "",
        &[receipt.execution_id.as_str()],
    )?;
    // The point key also finds a committed enrollment whose reply was lost.
    let Some(row) = records::point(tx, &scope, &id, &[STEP, STEP_PENDING], budget).await? else {
        return Ok(());
    };
    let previous: Step = records::decode(&row, &row.scope_id, row.session_key.as_deref())?;
    let (_, unit) = records::linked_unit(tx, &scope, &row, &previous, budget).await?;
    ensure!(
        previous.execution == receipt.execution_id
            && unit.principal == receipt.principal_id.as_str()
            && unit.profile.revision == receipt.execution_revision,
        "native receipt source mismatch"
    );
    let (digest, dispatch) = candidate.experiment_disposition()?;
    let link = ReceiptLink {
        digest: digest.to_owned(),
        state: receipt.state,
        dispatch,
    };
    if let Some(old) = &previous.receipt {
        ensure!(old == &link, "native receipt link is immutable");
        return Ok(());
    }
    let mut next = previous.clone();
    next.receipt = Some(link);
    if next.request.is_none() && dispatch == DispatchState::NotStarted {
        ensure!(
            matches!(
                next.publication,
                Publication::Awaiting | Publication::NotStarted
            ),
            "native no-start publication conflict"
        );
        next.publication = Publication::NotStarted;
    }
    records::advance(tx, &scope, &provenance, &row, &previous, &next, budget).await
}

pub(in crate::tenancy) async fn trajectory(
    tx: &DatabaseTransaction,
    execution: &TenantExecution,
    settlement: &Settlement,
    existing: bool,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    if execution.experiment().is_none() {
        return Ok(());
    }
    let (scope, provenance) = Scope::for_execution(execution);
    let id = scope.physical_key(
        KeyDomain::NativeExperimentStep,
        "",
        &[execution.id().as_str()],
    )?;
    let row = records::point(tx, &scope, &id, &[STEP, STEP_PENDING], budget)
        .await?
        .context("native publication enrollment missing")?;
    let previous: Step = records::decode(&row, &row.scope_id, row.session_key.as_deref())?;
    let (_, unit) = records::linked_unit(tx, &scope, &row, &previous, budget).await?;
    ensure!(
        previous.execution == *execution.id()
            && previous.request.is_some()
            && previous.request.as_deref() == settlement.event.request_id.as_deref()
            && unit.episode == settlement.event.episode_id,
        "native publication edge changed"
    );
    let outbox = settlement
        .outbox
        .as_ref()
        .context("native experiment requires actual Eval publication")?;
    let evidence = outbox
        .payload
        .evaluation
        .as_ref()
        .context("native experiment publication missing Eval")?;
    crate::trajectory::evaluation::validate_evaluation_envelope(evidence)?;
    crate::trajectory::evaluation::validate_builtin_trajectory_result(&evidence.result)?;
    let result = crate::storage::evaluation::result_digest(&evidence.result)?;
    for value in [
        &settlement.event.event_id,
        &evidence.subject.eval_id,
        &result,
        &outbox.outbox_id,
    ] {
        ensure!(
            value.len() <= 256,
            "native publication exceeds its admitted link allowance"
        );
    }
    let publication = Publication::Published {
        event: settlement.event.event_id.clone(),
        subject: evidence.subject.eval_id.clone(),
        result,
        outbox: outbox.outbox_id.clone(),
    };
    if !matches!(previous.publication, Publication::Awaiting) {
        ensure!(
            previous.publication == publication,
            "native publication is immutable"
        );
        return Ok(());
    }
    ensure!(
        !existing,
        "committed native trajectory has no atomic experiment publication"
    );
    let mut next = previous.clone();
    next.publication = publication;
    records::advance(tx, &scope, &provenance, &row, &previous, &next, budget).await
}

pub(in crate::tenancy) async fn native_ack(
    tx: &DatabaseTransaction,
    binding: &Binding,
    journal: &[u8],
    budget: &OperationBudget<'_>,
) -> Result<bool> {
    let (scope, provenance) = Scope::for_session(binding.authority());
    let descriptor = binding.descriptor();
    let native = scope.physical_key(
        KeyDomain::NativeSession,
        "",
        &[descriptor.principal.as_str(), &descriptor.name],
    )?;
    let pending = records::pending_session(tx, &scope, &native, budget).await?;
    // A previously committed ACK is immutable. Remaining receipt/outbox work
    // retains its original owner; it does not require decoding every later run.
    let pending = pending
        .into_iter()
        .filter(|(_, step)| step.ack.is_none())
        .collect::<Vec<_>>();
    if pending.is_empty() {
        return Ok(false);
    }
    crate::tenancy::experiment::check_native_allowance(u64::try_from(journal.len())?)?;
    budget.charge(
        1,
        journal
            .len()
            .checked_mul(2)
            .and_then(|n| n.checked_add(8192))
            .ok_or(Error::Capacity)?,
    )?;
    let readback = Readback::load(binding, journal)
        .map_err(|_| anyhow::anyhow!("native experiment ACK source validation failed"))?;
    for (row, previous) in pending {
        let (_, unit) = records::linked_unit(tx, &scope, &row, &previous, budget).await?;
        ensure!(
            unit.principal == descriptor.principal.as_str()
                && unit.harness == descriptor.harness
                && unit.session == descriptor.session
                && unit.profile.revision == descriptor.revision,
            "native ACK owner edge mismatch"
        );
        if let Some(ack) = readback
            .settled(&previous.facts, previous.execution.as_str())
            .map_err(|_| anyhow::anyhow!("native experiment ACK step changed"))?
            && previous.ack.is_none()
        {
            let mut next = previous.clone();
            next.ack = Some(ack);
            records::advance(tx, &scope, &provenance, &row, &previous, &next, budget).await?;
        }
    }
    Ok(true)
}
