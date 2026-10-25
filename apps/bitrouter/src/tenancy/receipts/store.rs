//! Bounded immutable bytes, written only by the existing authority actor.
use super::capacity::Inventory;
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    execution::{ExecutionPage, ExecutionReceipt, ReceiptState},
    identity::{CorrelationId, ExecutionId, InstallationId, TenantId},
    operation::DispatchState,
    validation::Validate,
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseTransaction, QueryResult, SqlErr,
    sea_query::{Alias, Asterisk, Expr, OnConflict, Order, Query, SelectStatement},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[cfg(test)]
mod tests;

const TABLE: &str = "tenancy_execution_receipts";
const MAX_RECEIPT: usize = 1024 * 1024;
const MAX_DISPOSITION: usize = 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tenancy) struct Disposition {
    pub dispatch: DispatchState,
    pub statuses: Vec<Option<u16>>,
}

pub(in crate::tenancy) struct Candidate {
    pub receipt: ExecutionReceipt,
    json: String,
    private_json: String,
    digest: String,
}

impl Candidate {
    pub fn new(receipt: ExecutionReceipt, disposition: Disposition) -> Result<Self> {
        receipt.validate()?;
        ensure!(
            disposition.statuses.len() == receipt.attempts.len(),
            "receipt attempt disposition count"
        );
        ensure!(
            disposition
                .statuses
                .iter()
                .flatten()
                .all(|status| (200..=599).contains(status)),
            "receipt status range"
        );
        let json = serde_json::to_string(&receipt)?;
        let private_json = serde_json::to_string(&disposition)?;
        ensure!(
            json.len() <= MAX_RECEIPT && private_json.len() <= MAX_DISPOSITION,
            "receipt byte bound"
        );
        let digest = digest(&json, &private_json);
        Ok(Self {
            receipt,
            json,
            private_json,
            digest,
        })
    }
    #[cfg(test)]
    pub(in crate::tenancy) fn fingerprint_for_test(
        &self,
    ) -> crate::tenancy::transaction::receipt_observation::Fingerprint {
        crate::tenancy::transaction::receipt_observation::Fingerprint {
            digest: self.digest.clone(),
            bytes: self.bytes(),
            attempt_count: self.receipt.attempts.len(),
            first_attempt: self
                .receipt
                .attempts
                .first()
                .map(|attempt| attempt.attempt_id.clone()),
        }
    }
    pub fn bytes(&self) -> u64 {
        (self.json.len() + self.private_json.len()) as u64
    }
    pub fn unknown(&self) -> bool {
        self.receipt.state == ReceiptState::Unknown
    }
}

fn digest(json: &str, disposition: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"bitrouter.receipt.immutable.v1");
    for part in [json, disposition] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    hex::encode(hash.finalize())
}
fn scope(installation: &InstallationId, tenant: &TenantId) -> SelectStatement {
    Query::select()
        .from(Alias::new(TABLE))
        .and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()))
        .and_where(Expr::col(Alias::new("tenant_id")).eq(tenant.as_str()))
        .to_owned()
}
fn point(installation: &InstallationId, tenant: &TenantId, id: &ExecutionId) -> SelectStatement {
    scope(installation, tenant)
        .and_where(Expr::col(Alias::new("execution_id")).eq(id.as_str()))
        .to_owned()
}

fn octets(backend: DatabaseBackend, name: &str) -> String {
    if backend == DatabaseBackend::Sqlite {
        format!("length(CAST({name} AS BLOB))")
    } else {
        format!("octet_length({name})")
    }
}
fn oversized(backend: DatabaseBackend) -> String {
    [
        ("receipt_json", MAX_RECEIPT),
        ("disposition_json", MAX_DISPOSITION),
        ("installation_id", 128),
        ("tenant_id", 128),
        ("execution_id", 128),
        ("principal_id", 128),
        ("correlation_id", 128),
        ("content_digest", 64),
        ("recorded_at", 20),
    ]
    .into_iter()
    .map(|(name, max)| format!("{} > {max}", octets(backend, name)))
    .collect::<Vec<_>>()
    .join(" OR ")
}
async fn row(db: &impl ConnectionTrait, mut query: SelectStatement) -> Result<Option<QueryResult>> {
    let backend = db.get_database_backend();
    // Bound stored TEXT before transferring it; a corrupt cell is never materialized.
    let length = oversized(backend);
    let oversized = query
        .clone()
        .expr(Expr::val(1))
        .and_where(Expr::cust(length))
        .limit(1)
        .to_owned();
    ensure!(
        db.query_one(backend.build(&oversized)).await?.is_none(),
        "stored receipt byte bound"
    );
    query.column(Asterisk).limit(1);
    Ok(db.query_one(backend.build(&query)).await?)
}

fn decode(
    row: QueryResult,
    installation: &InstallationId,
    tenant: &TenantId,
    id: &ExecutionId,
) -> Result<Candidate> {
    let json: String = row.try_get("", "receipt_json")?;
    let private: String = row.try_get("", "disposition_json")?;
    ensure!(
        json.len() <= MAX_RECEIPT && private.len() <= MAX_DISPOSITION,
        "receipt byte bound"
    );
    let candidate = Candidate::new(
        serde_json::from_str(&json)?,
        serde_json::from_str(&private)?,
    )?;
    let receipt = &candidate.receipt;
    ensure!(
        &receipt.installation_id == installation
            && &receipt.tenant_id == tenant
            && &receipt.execution_id == id,
        "receipt namespace mismatch"
    );
    for (column, value) in [
        ("installation_id", installation.as_str()),
        ("tenant_id", tenant.as_str()),
        ("execution_id", id.as_str()),
        ("principal_id", receipt.principal_id.as_str()),
        ("correlation_id", receipt.correlation_id.as_str()),
        ("recorded_at", receipt.recorded_at.as_str()),
    ] {
        ensure!(
            row.try_get::<String>("", column)? == value,
            "receipt projection mismatch"
        );
    }
    ensure!(
        candidate.json == json
            && candidate.private_json == private
            && row.try_get::<String>("", "content_digest")? == candidate.digest
            && row.try_get::<i64>("", "byte_count")? == i64::try_from(candidate.bytes())?
            && row.try_get::<bool>("", "unknown_outcome")? == candidate.unknown(),
        "receipt integrity mismatch"
    );
    Ok(candidate)
}

pub(in crate::tenancy) async fn lookup(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    tenant: &TenantId,
    id: &ExecutionId,
) -> Result<Option<ExecutionReceipt>> {
    row(db, point(installation, tenant, id))
        .await?
        .map(|row| decode(row, installation, tenant, id).map(|candidate| candidate.receipt))
        .transpose()
}

/// The actor owns this transaction through confirmed commit/rollback. Reconciliation
/// compares exact immutable bytes; it never replays inference or widens authority.
pub(in crate::tenancy) async fn insert(
    tx: &DatabaseTransaction,
    candidate: &Candidate,
) -> Result<bool> {
    let receipt = &candidate.receipt;
    let existing = row(
        tx,
        point(
            &receipt.installation_id,
            &receipt.tenant_id,
            &receipt.execution_id,
        ),
    )
    .await?;
    if let Some(row) = existing {
        let saved = decode(
            row,
            &receipt.installation_id,
            &receipt.tenant_id,
            &receipt.execution_id,
        )?;
        ensure!(
            saved.digest == candidate.digest
                && saved.json == candidate.json
                && saved.private_json == candidate.private_json,
            "immutable receipt conflict"
        );
        return Ok(false);
    }
    let fields: Vec<(&str, sea_orm::Value)> = vec![
        ("installation_id", receipt.installation_id.as_str().into()),
        ("tenant_id", receipt.tenant_id.as_str().into()),
        ("execution_id", receipt.execution_id.as_str().into()),
        ("principal_id", receipt.principal_id.as_str().into()),
        ("correlation_id", receipt.correlation_id.as_str().into()),
        ("receipt_json", candidate.json.clone().into()),
        ("content_digest", candidate.digest.clone().into()),
        ("disposition_json", candidate.private_json.clone().into()),
        ("byte_count", i64::try_from(candidate.bytes())?.into()),
        ("unknown_outcome", candidate.unknown().into()),
        ("recorded_at", receipt.recorded_at.as_str().into()),
    ];
    let mut query = Query::insert();
    query
        .into_table(Alias::new(TABLE))
        .columns(fields.iter().map(|(name, _)| Alias::new(*name)))
        .values(fields.into_iter().map(|(_, value)| value.into()))?;
    let backend = tx.get_database_backend();
    if backend != DatabaseBackend::MySql {
        query.on_conflict(OnConflict::new().do_nothing().to_owned());
    }
    let changed = match tx.execute(backend.build(&query)).await {
        Ok(result) => {
            ensure!(result.rows_affected() <= 1, "receipt insert count");
            result.rows_affected() == 1
        }
        Err(error)
            if backend == DatabaseBackend::MySql
                && matches!(error.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))) =>
        {
            false
        }
        Err(error) => return Err(error.into()),
    };
    let saved = decode(
        row(
            tx,
            point(
                &receipt.installation_id,
                &receipt.tenant_id,
                &receipt.execution_id,
            ),
        )
        .await?
        .context("receipt physical collision")?,
        &receipt.installation_id,
        &receipt.tenant_id,
        &receipt.execution_id,
    )?;
    ensure!(
        saved.digest == candidate.digest
            && saved.json == candidate.json
            && saved.private_json == candidate.private_json,
        "immutable receipt conflict"
    );
    Ok(changed)
}

/// Caller supplies one stable read transaction and rechecks current permission
/// after it. Multiplicity is query-wide, independent of cursor and page size.
pub(in crate::tenancy) async fn page(
    db: &DatabaseTransaction,
    installation: &InstallationId,
    tenant: &TenantId,
    correlation: &CorrelationId,
    after: Option<&ExecutionId>,
    limit: u64,
) -> Result<ExecutionPage> {
    ensure!((1..=100).contains(&limit), "receipt page limit");
    let backend = db.get_database_backend();
    let invalid = scope(installation, tenant)
        .expr(Expr::val(1))
        .and_where(Expr::col(Alias::new("correlation_id")).eq(correlation.as_str()))
        .and_where(Expr::cust(format!(
            "{} > 128",
            octets(backend, "execution_id")
        )))
        .limit(1)
        .to_owned();
    ensure!(
        db.query_one(backend.build(&invalid)).await?.is_none(),
        "receipt page identity byte bound"
    );
    let mut query = scope(installation, tenant);
    query
        .and_where(Expr::col(Alias::new("correlation_id")).eq(correlation.as_str()))
        .column(Alias::new("execution_id"))
        .order_by(Alias::new("execution_id"), Order::Asc)
        .limit(2);
    let multiple_executions = db.query_all(backend.build(&query)).await?.len() > 1;
    if let Some(after) = after {
        query.and_where(Expr::col(Alias::new("execution_id")).gt(after.as_str()));
    }
    query.limit(limit + 1);
    let rows = db.query_all(backend.build(&query)).await?;
    let more = rows.len() > usize::try_from(limit)?;
    let execution_ids = rows
        .into_iter()
        .take(usize::try_from(limit)?)
        .map(|row| {
            let raw: String = row.try_get("", "execution_id")?;
            Ok(raw.try_into()?)
        })
        .collect::<Result<Vec<ExecutionId>>>()?;
    let next_after = if more {
        execution_ids.last().cloned()
    } else {
        None
    };
    let page = ExecutionPage {
        execution_ids,
        multiple_executions,
        next_after,
        installation_id: installation.clone(),
        correlation_id: correlation.clone(),
    };
    page.validate()?;
    Ok(page)
}

pub(in crate::tenancy) async fn inventory(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
) -> Result<Inventory> {
    let backend = db.get_database_backend();
    let query = Query::select()
        .from(Alias::new(TABLE))
        .expr(Expr::val(1))
        .and_where(Expr::col(Alias::new("installation_id")).ne(installation.as_str()))
        .limit(1)
        .to_owned();
    ensure!(
        db.query_one(backend.build(&query)).await?.is_none(),
        "foreign receipt installation"
    );
    // Do not trust a stored count that can understate the reserved payload. The
    // persistent schema bounds identity/index bytes; measure actual TEXT octets
    // before SUM. No payload is transferred during this check.
    let exact = format!(
        "byte_count != {} + {} OR byte_count < 1 OR {}",
        octets(backend, "receipt_json"),
        octets(backend, "disposition_json"),
        oversized(backend)
    );
    let invalid = Query::select()
        .from(Alias::new(TABLE))
        .expr(Expr::val(1))
        .and_where(Expr::cust(exact))
        .limit(1)
        .to_owned();
    ensure!(
        db.query_one(backend.build(&invalid)).await?.is_none(),
        "receipt inventory byte count"
    );
    let unknown_mismatch = match backend {
        DatabaseBackend::Sqlite => {
            "json_extract(receipt_json, '$.state') IS NULL OR json_extract(receipt_json, '$.state') NOT IN ('committed','incomplete','unknown') OR unknown_outcome != (json_extract(receipt_json, '$.state') = 'unknown')"
        }
        DatabaseBackend::Postgres => {
            "receipt_json::jsonb->>'state' IS NULL OR receipt_json::jsonb->>'state' NOT IN ('committed','incomplete','unknown') OR unknown_outcome <> (receipt_json::jsonb->>'state' = 'unknown')"
        }
        DatabaseBackend::MySql => {
            "JSON_EXTRACT(receipt_json, '$.state') IS NULL OR JSON_UNQUOTE(JSON_EXTRACT(receipt_json, '$.state')) NOT IN ('committed','incomplete','unknown') OR unknown_outcome <> (JSON_UNQUOTE(JSON_EXTRACT(receipt_json, '$.state')) = 'unknown')"
        }
    };
    let invalid = Query::select()
        .from(Alias::new(TABLE))
        .expr(Expr::val(1))
        .and_where(Expr::cust(unknown_mismatch))
        .limit(1)
        .to_owned();
    ensure!(
        db.query_one(backend.build(&invalid)).await?.is_none(),
        "receipt unknown projection mismatch"
    );
    let query = Query::select()
        .from(Alias::new(TABLE))
        .expr_as(
            Expr::cust(if backend == DatabaseBackend::MySql {
                "CAST(SUM(byte_count) AS SIGNED)"
            } else {
                "CAST(SUM(byte_count) AS BIGINT)"
            }),
            Alias::new("bytes"),
        )
        .expr_as(
            Expr::cust(if backend == DatabaseBackend::MySql {
                "CAST(COUNT(*) AS SIGNED)"
            } else {
                "CAST(COUNT(*) AS BIGINT)"
            }),
            Alias::new("rows"),
        )
        .and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()))
        .to_owned();
    let row = db
        .query_one(backend.build(&query))
        .await?
        .context("receipt inventory")?;
    let bytes = row.try_get::<Option<i64>>("", "bytes")?.unwrap_or(0);
    let count = u64::try_from(row.try_get::<i64>("", "rows")?)?;
    let query = Query::select()
        .from(Alias::new(TABLE))
        .columns(["tenant_id", "execution_id", "recorded_at"].map(Alias::new))
        .and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()))
        .and_where(Expr::col(Alias::new("unknown_outcome")).eq(true))
        .limit(1001)
        .to_owned();
    let rows = db.query_all(backend.build(&query)).await?;
    ensure!(rows.len() <= 1000, "receipt unknown instance bound");
    let mut unknown = Vec::with_capacity(rows.len());
    let mut counts = std::collections::BTreeMap::new();
    for row in rows {
        let tenant: TenantId = row.try_get::<String>("", "tenant_id")?.try_into()?;
        let id: ExecutionId = row.try_get::<String>("", "execution_id")?.try_into()?;
        let recorded =
            chrono::DateTime::parse_from_rfc3339(&row.try_get::<String>("", "recorded_at")?)?
                .with_timezone(&chrono::Utc);
        let count = counts.entry(tenant.clone()).or_insert(0_usize);
        *count += 1;
        ensure!(*count <= 100, "receipt unknown tenant bound");
        unknown.push((tenant, id, recorded));
    }
    Ok(Inventory {
        bytes: u64::try_from(bytes)?,
        rows: count,
        unknown,
    })
}
