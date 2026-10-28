use super::*;
use crate::{
    harness::trajectory::owned::entities::records,
    storage::scope::{MetadataRef, Provenance},
};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QuerySelect, Set, sea_query::Expr};
use serde::{Serialize, de::DeserializeOwned};

fn query(scope: &Scope) -> sea_orm::Select<records::Entity> {
    records::Entity::find()
        .filter(scope.predicate(records::Column::InstallationId, records::Column::TenantId))
}
pub(super) async fn point(
    tx: &DatabaseTransaction,
    scope: &Scope,
    id: &str,
    kinds: &[&str],
    budget: &OperationBudget<'_>,
) -> Result<Option<records::Model>> {
    let row = io::one(
        tx,
        query(scope)
            .filter(records::Column::RecordId.eq(id))
            .filter(records::Column::Kind.is_in(kinds.iter().copied()))
            .lock_exclusive(),
        RECORD_BYTES + 4096,
        budget,
    )
    .await?;
    if let Some(row) = &row {
        metadata(scope, row)?;
    }
    Ok(row)
}
fn metadata(scope: &Scope, row: &records::Model) -> Result<()> {
    scope.validate_metadata(MetadataRef {
        installation: row.installation_id.as_deref(),
        tenant: row.tenant_id.as_deref(),
        creator: row.creator_principal_id.as_deref(),
    })?;
    ensure!(
        row.revision >= 0 && row.body.len() <= RECORD_BYTES,
        "native experiment row bounds"
    );
    Ok(())
}
pub(super) fn decode<T: DeserializeOwned>(
    row: &records::Model,
    partition: &str,
    session: Option<&str>,
) -> Result<T> {
    ensure!(
        row.scope_id == partition && row.session_key.as_deref() == session,
        "native experiment physical edge mismatch"
    );
    Ok(serde_json::from_str(&row.body)?)
}
pub(super) async fn steps(
    tx: &DatabaseTransaction,
    scope: &Scope,
    unit: &str,
    session: &str,
    budget: &OperationBudget<'_>,
) -> Result<Vec<(records::Model, Step)>> {
    let rows = io::rows(
        tx,
        query(scope)
            .filter(records::Column::ScopeId.eq(unit))
            .filter(records::Column::SessionKey.eq(session))
            .filter(records::Column::Kind.is_in([STEP, STEP_PENDING]))
            .lock_exclusive(),
        MAX_STEPS as usize,
        1024 * 1024 + MAX_STEPS as usize * 4096,
        budget,
    )
    .await?;
    let mut output = Vec::with_capacity(rows.len());
    let mut executions = std::collections::BTreeSet::new();
    let mut origins = std::collections::BTreeSet::new();
    for row in rows {
        metadata(scope, &row)?;
        let step: Step = decode(&row, unit, Some(session))?;
        ensure!(
            step.version == 1
                && step.unit == unit
                && row.record_id
                    == scope.physical_key(
                        KeyDomain::NativeExperimentStep,
                        "",
                        &[step.execution.as_str()]
                    )?
                && row.kind == if step.complete() { STEP } else { STEP_PENDING }
                && executions.insert(step.execution.clone())
                && origins.insert((step.facts.agent.clone(), step.facts.step.clone())),
            "native experiment step closure conflict"
        );
        step.reserve()?;
        output.push((row, step));
    }
    Ok(output)
}

pub(super) struct Slot<'a> {
    pub id: &'a str,
    pub partition: &'a str,
    pub kind: &'a str,
    pub session: Option<&'a str>,
    pub previous: Option<&'a records::Model>,
}

pub(super) async fn put<T: Serialize>(
    tx: &DatabaseTransaction,
    scope: &Scope,
    provenance: &Provenance,
    slot: Slot<'_>,
    value: &T,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    let Slot {
        id,
        partition,
        kind,
        session,
        previous,
    } = slot;
    let body = io::json(value, RECORD_BYTES).map_err(|_| Error::Capacity)?;
    budget.charge(1, body.len() + 4096)?;
    let changed = if let Some(previous) = previous {
        ensure!(
            previous.record_id == id
                && previous.scope_id == partition
                && previous.session_key.as_deref() == session,
            "native experiment replacement edge changed"
        );
        records::Entity::update_many()
            .filter(scope.predicate(records::Column::InstallationId, records::Column::TenantId))
            .filter(records::Column::RecordId.eq(id))
            .filter(records::Column::Kind.eq(&previous.kind))
            .filter(records::Column::Revision.eq(previous.revision))
            .col_expr(
                records::Column::Revision,
                Expr::value(
                    previous
                        .revision
                        .checked_add(1)
                        .context("native experiment revision overflow")?,
                ),
            )
            .col_expr(records::Column::Kind, Expr::value(kind.to_owned()))
            .col_expr(records::Column::Body, Expr::value(body))
            .exec(tx)
            .await?
            .rows_affected
    } else {
        let meta = scope.metadata(provenance)?;
        records::Entity::insert(records::ActiveModel {
            record_id: Set(id.to_owned()),
            scope_id: Set(partition.to_owned()),
            kind: Set(kind.to_owned()),
            session_key: Set(session.map(str::to_owned)),
            revision: Set(0),
            body: Set(body),
            installation_id: Set(meta.installation),
            tenant_id: Set(meta.tenant),
            creator_principal_id: Set(meta.creator),
        })
        .exec_without_returning(tx)
        .await?
    };
    budget.check()?;
    ensure!(changed == 1, "native experiment CAS failed");
    Ok(())
}

pub(super) async fn pending_session(
    tx: &DatabaseTransaction,
    scope: &Scope,
    session: &str,
    budget: &OperationBudget<'_>,
) -> Result<Vec<(records::Model, Step)>> {
    let rows = io::rows(
        tx,
        query(scope)
            .filter(records::Column::SessionKey.eq(session))
            .filter(records::Column::Kind.eq(STEP_PENDING))
            .lock_exclusive(),
        MAX_STEPS as usize,
        MAX_STEPS as usize * (RECORD_BYTES + 4096),
        budget,
    )
    .await?;
    let mut output = Vec::with_capacity(rows.len());
    for row in rows {
        metadata(scope, &row)?;
        let step: Step = decode(&row, &row.scope_id, Some(session))?;
        ensure!(
            step.version == 1
                && step.unit == row.scope_id
                && !step.complete()
                && row.record_id
                    == scope.physical_key(
                        KeyDomain::NativeExperimentStep,
                        "",
                        &[step.execution.as_str()]
                    )?,
            "native pending step identity mismatch"
        );
        step.reserve()?;
        output.push((row, step));
    }
    Ok(output)
}

pub(super) async fn linked_unit(
    tx: &DatabaseTransaction,
    scope: &Scope,
    row: &records::Model,
    step: &Step,
    budget: &OperationBudget<'_>,
) -> Result<(records::Model, Unit)> {
    let unit_row = point(tx, scope, &step.unit, &[UNIT, UNIT_PENDING], budget)
        .await?
        .context("native step has no unit")?;
    let partition = scope.physical_key(KeyDomain::NativeExperimentPending, "", &[])?;
    let unit: Unit = decode(&unit_row, &partition, row.session_key.as_deref())?;
    ensure!(
        unit.version == 1
            && unit.key == step.unit
            && step.unit == row.scope_id
            && unit.facts_match(&step.facts)
            && unit.steps > 0
            && unit.steps <= MAX_STEPS
            && unit.pending <= unit.steps
            && (step.complete() || unit.pending > 0)
            && unit_row.kind
                == if unit.pending == 0 {
                    UNIT
                } else {
                    UNIT_PENDING
                },
        "native step unit phase mismatch"
    );
    ensure!(
        unit.key
            == scope.physical_key(
                KeyDomain::NativeExperimentUnit,
                "",
                &[&unit.principal, &unit.harness, &unit.session, &unit.run]
            )?
            && unit.episode == format!("episode_{}", unit.key)
            && unit.counter
                == scope.physical_key(
                    KeyDomain::NativeExperimentCounter,
                    "",
                    &[&unit.profile.treatment.experiment_id]
                )?
            && unit.assignment
                == unit
                    .profile
                    .treatment
                    .assignment_for(ExperimentAssignmentUnit::Episode, &unit.key)
            && unit_row.creator_principal_id.as_deref() == Some(unit.principal.as_str()),
        "native unit logical/physical closure mismatch"
    );
    Ok((unit_row, unit))
}

pub(super) async fn advance(
    tx: &DatabaseTransaction,
    scope: &Scope,
    provenance: &Provenance,
    row: &records::Model,
    previous: &Step,
    next: &Step,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    ensure!(
        next.execution == previous.execution
            && next.unit == previous.unit
            && next.facts == previous.facts,
        "native completion changed immutable step intent"
    );
    ensure!(
        !previous.complete() || next.complete(),
        "native completion cannot reopen"
    );
    if let (Some(ack), Some(receipt)) = (&next.ack, &next.receipt) {
        ensure!(
            ack.planned || receipt.dispatch == DispatchState::NotStarted,
            "native pre-plan terminal conflicts with actual dispatch"
        );
    }
    next.reserve()?;
    let (unit_row, mut unit) = linked_unit(tx, scope, row, previous, budget).await?;
    if !previous.complete() && next.complete() {
        unit.pending = unit
            .pending
            .checked_sub(1)
            .context("native pending count underflow")?;
        put(
            tx,
            scope,
            provenance,
            Slot {
                id: &unit.key,
                partition: &unit_row.scope_id,
                kind: if unit.pending == 0 {
                    UNIT
                } else {
                    UNIT_PENDING
                },
                session: unit_row.session_key.as_deref(),
                previous: Some(&unit_row),
            },
            &unit,
            budget,
        )
        .await?;
    }
    put(
        tx,
        scope,
        provenance,
        Slot {
            id: &row.record_id,
            partition: &row.scope_id,
            kind: if next.complete() { STEP } else { STEP_PENDING },
            session: row.session_key.as_deref(),
            previous: Some(row),
        },
        next,
        budget,
    )
    .await
}
