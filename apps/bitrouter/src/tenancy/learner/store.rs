//! Bounded scheduling metadata in the existing owned checkpoint partition.
use super::Registration;
use crate::{
    harness::trajectory::owned::{entities::records, io},
    storage::{
        operation_budget::{Error, OperationBudget},
        partition,
    },
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::identity::{InstallationId, TenantId};
use sea_orm::{
    ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
    QueryTrait, Set, sea_query::Expr,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const KIND: &str = "owned_learner_schedule_v1";
const MAX_RECORD: usize = 256 * 1024;

#[derive(Clone, Default)]
pub(in crate::tenancy) struct Cursor {
    pub tenant: Option<TenantId>,
    pub after: Option<String>,
}
pub(in crate::tenancy) struct Page {
    pub cursor: Cursor,
    pub registrations: Vec<Arc<Registration>>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tenancy) struct Schedule {
    pub registration: Registration,
    pub next_sequence: u64,
    pub completed_fence: Option<String>,
}

fn query(installation: &InstallationId, tenant: &TenantId) -> sea_orm::Select<records::Entity> {
    records::Entity::find()
        .filter(records::Column::InstallationId.eq(installation.as_str()))
        .filter(records::Column::TenantId.eq(tenant.as_str()))
        .filter(records::Column::Kind.eq(KIND))
}

fn decode(
    row: &records::Model,
    installation: &InstallationId,
    tenant: &TenantId,
) -> Result<Schedule> {
    ensure!(
        row.body.len() <= MAX_RECORD
            && row.revision >= 0
            && row.kind == KIND
            && row.installation_id.as_deref() == Some(installation.as_str())
            && row.tenant_id.as_deref() == Some(tenant.as_str())
            && row.session_key.is_none(),
        "learner schedule metadata"
    );
    let value: Schedule = serde_json::from_str(&row.body)?;
    let registration = &value.registration;
    ensure!(
        registration.version == 1
            && registration.input.installation == *installation
            && registration.input.tenant == *tenant
            && registration.revision <= row.revision as u64
            && row.creator_principal_id.as_deref() == Some(registration.creator.as_str())
            && row.record_id
                == partition::learner_schedule(installation, tenant, &registration.input.id)?
            && row.scope_id == partition::learner_schedule(installation, tenant, "")?,
        "learner schedule identity"
    );
    Ok(value)
}

pub(in crate::tenancy) async fn read(
    tx: &DatabaseTransaction,
    installation: &InstallationId,
    tenant: &TenantId,
    id: &str,
    budget: &OperationBudget<'_>,
) -> Result<Option<(records::Model, Schedule)>> {
    let key = partition::learner_schedule(installation, tenant, id)?;
    let row = io::one(
        tx,
        query(installation, tenant)
            .filter(records::Column::RecordId.eq(key))
            .lock_exclusive(),
        MAX_RECORD + 4096,
        budget,
    )
    .await?;
    row.map(|row| {
        let value = decode(&row, installation, tenant)?;
        Ok((row, value))
    })
    .transpose()
}

pub(in crate::tenancy) async fn page(
    tx: &DatabaseTransaction,
    installation: &InstallationId,
    tenant: &TenantId,
    after: Option<&str>,
    budget: &OperationBudget<'_>,
) -> Result<Vec<Arc<Registration>>> {
    let mut selected = query(installation, tenant)
        .order_by_asc(records::Column::RecordId)
        .limit(super::MAX_DUE as u64);
    if let Some(after) = after {
        selected = selected.filter(records::Column::RecordId.gt(after));
    }
    // io::rows deliberately probes max_rows + 1. Keep the page's SQL LIMIT in
    // a subquery so a 33rd registration advances the next page instead of
    // rejecting and fencing an otherwise healthy Service.
    let keys = selected
        .select_only()
        .column(records::Column::RecordId)
        .into_query();
    let page = query(installation, tenant)
        .filter(records::Column::RecordId.in_subquery(keys))
        .order_by_asc(records::Column::RecordId);
    // OperationBudget performs the cumulative byte refusal before allocation.
    let rows = io::rows(tx, page, super::MAX_DUE, usize::MAX, budget).await?;
    rows.into_iter()
        .map(|row| {
            decode(&row, installation, tenant).map(|schedule| Arc::new(schedule.registration))
        })
        .collect()
}

pub(in crate::tenancy) async fn put(
    tx: &DatabaseTransaction,
    value: &Schedule,
    previous: Option<&records::Model>,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    let registration = &value.registration;
    let input = &registration.input;
    let body = io::json(value, MAX_RECORD).map_err(|_| Error::Capacity)?;
    budget.charge(1, body.len() + 4096)?;
    let id = partition::learner_schedule(&input.installation, &input.tenant, &input.id)?;
    let changed = if let Some(previous) = previous {
        records::Entity::update_many()
            .filter(records::Column::InstallationId.eq(input.installation.as_str()))
            .filter(records::Column::TenantId.eq(input.tenant.as_str()))
            .filter(records::Column::RecordId.eq(&id))
            .filter(records::Column::Kind.eq(KIND))
            .filter(records::Column::Revision.eq(previous.revision))
            .col_expr(
                records::Column::Revision,
                Expr::value(
                    previous
                        .revision
                        .checked_add(1)
                        .context("learner revision overflow")?,
                ),
            )
            .col_expr(records::Column::Body, Expr::value(body))
            .exec(tx)
            .await?
            .rows_affected
    } else {
        records::Entity::insert(records::ActiveModel {
            record_id: Set(id),
            scope_id: Set(partition::learner_schedule(
                &input.installation,
                &input.tenant,
                "",
            )?),
            kind: Set(KIND.into()),
            session_key: Set(None),
            revision: Set(0),
            body: Set(body),
            installation_id: Set(Some(input.installation.to_string())),
            tenant_id: Set(Some(input.tenant.to_string())),
            creator_principal_id: Set(Some(registration.creator.to_string())),
        })
        .exec_without_returning(tx)
        .await?
    };
    budget.check()?;
    ensure!(changed == 1, "learner schedule CAS failed");
    Ok(())
}
