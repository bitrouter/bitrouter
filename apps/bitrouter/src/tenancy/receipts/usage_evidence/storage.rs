//! Atomic immutable sidecars; bounded loads verify both directions of the binding.
use super::super::store::Candidate;
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    execution::{ExecutionReceipt, Outcome, ReceiptState},
    identity::{ExecutionId, InstallationId, TenantId},
    usage_evidence::{Capture, ExecutionUsageEvidenceV1, MAX_BYTES},
    validation::Validate,
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend,
    sea_query::{Alias, Expr, Order, Query, SelectStatement},
};
use sha2::{Digest, Sha256};

const TABLE: &str = "tenancy_usage_evidence";
pub(in crate::tenancy::receipts) struct Evidence {
    pub json: String,
    pub digest: String,
    pub document: ExecutionUsageEvidenceV1,
}
impl Evidence {
    pub fn new(document: ExecutionUsageEvidenceV1, receipt: &ExecutionReceipt) -> Result<Self> {
        document.validate()?;
        ensure!(
            (document.capture == Capture::Captured
                || (document.capture == Capture::Unavailable
                    && receipt.state == ReceiptState::Unknown
                    && receipt.outcome == Outcome::Unknown))
                && document.installation_id == receipt.installation_id
                && document.tenant_id == receipt.tenant_id
                && document.execution_id == receipt.execution_id
                && document.attempts.len() == receipt.attempts.len(),
            "usage evidence receipt binding"
        );
        for (observed, actual) in document.attempts.iter().zip(&receipt.attempts) {
            ensure!(
                observed.attempt_id == actual.attempt_id
                    && observed.outcome == actual.outcome
                    && serde_json::to_value(&observed.target)?
                        == serde_json::to_value(&actual.target)?,
                "usage evidence attempt binding"
            );
        }
        let json = serde_json::to_string(&document)?;
        let mut hash = Sha256::new();
        hash.update(b"bitrouter.execution.usage-evidence.v1");
        hash.update((json.len() as u64).to_be_bytes());
        hash.update(json.as_bytes());
        Ok(Self {
            json,
            digest: hex::encode(hash.finalize()),
            document,
        })
    }
}
fn scope(receipt: &ExecutionReceipt) -> SelectStatement {
    Query::select()
        .from(Alias::new(TABLE))
        .and_where(Expr::col(Alias::new("installation_id")).eq(receipt.installation_id.as_str()))
        .and_where(Expr::col(Alias::new("tenant_id")).eq(receipt.tenant_id.as_str()))
        .and_where(Expr::col(Alias::new("execution_id")).eq(receipt.execution_id.as_str()))
        .to_owned()
}
fn octets(backend: DatabaseBackend, name: &str) -> String {
    if backend == DatabaseBackend::Sqlite {
        format!("length(CAST({name} AS BLOB))")
    } else {
        format!("octet_length({name})")
    }
}
fn integer_cell(backend: DatabaseBackend, name: &str) -> String {
    if backend == DatabaseBackend::Sqlite {
        format!("CASE WHEN typeof({name})='integer' THEN {name} ELSE NULL END")
    } else {
        name.to_owned()
    }
}

type Charge<'a> = dyn Fn(usize, usize) -> Result<()> + Sync + 'a;

/// Only scalar lengths cross this boundary. The stored byte_count is checked,
/// never used as the transfer allowance. Two rows include a duplicate sentinel.
async fn preflight(
    db: &impl ConnectionTrait,
    receipt: &ExecutionReceipt,
    expected: Option<&str>,
    charge: &Charge<'_>,
) -> Result<Option<usize>> {
    charge(2, 1024)?;
    let backend = db.get_database_backend();
    let json_bytes = octets(backend, "evidence_json");
    let cast = if backend == DatabaseBackend::MySql {
        "SIGNED"
    } else {
        "BIGINT"
    };
    let mut query = scope(receipt);
    query.expr_as(Expr::cust(format!(
        "CASE WHEN {json_bytes} BETWEEN 1 AND {MAX_BYTES} AND {}=64 AND {}=64 THEN CAST({json_bytes} AS {cast}) ELSE NULL END",
        octets(backend, "evidence_digest"), octets(backend, "receipt_digest")
    )), Alias::new("evidence_bytes"));
    for name in ["byte_count", "schema_version"] {
        query.expr_as(Expr::cust(integer_cell(backend, name)), Alias::new(name));
    }
    query.limit(2);
    let rows = db.query_all(backend.build(&query)).await?;
    charge(0, 0)?;
    ensure!(rows.len() <= 1, "usage evidence multiplicity");
    match (expected, rows.first()) {
        (None, None) => Ok(None),
        (Some(_), Some(row)) => {
            let bytes = usize::try_from(row.try_get::<i64>("", "evidence_bytes")?)?;
            ensure!(
                row.try_get::<i64>("", "byte_count")? == i64::try_from(bytes)?
                    && row.try_get::<i32>("", "schema_version")? == 1,
                "usage evidence length or version"
            );
            // Three copies cover transfer, decoding and canonical encoding.
            // Both digest strings are exactly 64 bytes; fixed overhead covers
            // scalar cells and allocation metadata. A duplicate is rejected
            // before decoding, so its two transferred bodies fit this allowance.
            let allocation = bytes
                .checked_add(128)
                .and_then(|bytes| bytes.checked_mul(3))
                .and_then(|bytes| bytes.checked_add(4096))
                .context("usage evidence allocation bound")?;
            charge(2, allocation)?;
            Ok(Some(bytes))
        }
        _ => anyhow::bail!("usage evidence presence mismatch"),
    }
}
async fn load(
    db: &impl ConnectionTrait,
    receipt: &ExecutionReceipt,
    receipt_digest: &str,
    expected: Option<&str>,
    charge: Option<&Charge<'_>>,
) -> Result<Option<Evidence>> {
    let measured = if let Some(charge) = charge {
        let Some(bytes) = preflight(db, receipt, expected, charge).await? else {
            return Ok(None);
        };
        Some(bytes)
    } else {
        None
    };
    let backend = db.get_database_backend();
    let mut query = scope(receipt);
    for (name, bound) in [
        ("evidence_json", MAX_BYTES),
        ("evidence_digest", 64),
        ("receipt_digest", 64),
    ] {
        let length = octets(backend, name);
        // Keep the absolute CASE guard, and also cap transfer to the measured
        // length. Growth between queries must become NULL before allocation.
        let unchanged = measured.map_or_else(String::new, |bytes| {
            format!(
                " AND {length}={}",
                if name == "evidence_json" { bytes } else { 64 }
            )
        });
        query.expr_as(
            Expr::cust(format!(
                "CASE WHEN {length} <= {bound}{unchanged} THEN {name} ELSE NULL END"
            )),
            Alias::new(name),
        );
    }
    for name in ["byte_count", "schema_version"] {
        query.expr_as(Expr::cust(integer_cell(backend, name)), Alias::new(name));
    }
    query.limit(2);
    let rows = db.query_all(backend.build(&query)).await?;
    if let Some(charge) = charge {
        charge(0, 0)?;
    }
    ensure!(rows.len() <= 1, "usage evidence multiplicity");
    match (expected, rows.first()) {
        (None, None) => Ok(None),
        (Some(expected), Some(row)) => {
            let json: String = row.try_get("", "evidence_json")?;
            ensure!(
                measured.is_none_or(|bytes| json.len() == bytes),
                "usage evidence length changed"
            );
            let evidence = Evidence::new(serde_json::from_str(&json)?, receipt)?;
            ensure!(
                evidence.json == json
                    && evidence.digest == expected
                    && row.try_get::<String>("", "evidence_digest")? == expected
                    && row.try_get::<String>("", "receipt_digest")? == receipt_digest
                    && row.try_get::<i64>("", "byte_count")? == i64::try_from(json.len())?
                    && row.try_get::<i32>("", "schema_version")? == 1,
                "usage evidence integrity"
            );
            Ok(Some(evidence))
        }
        _ => anyhow::bail!("usage evidence presence mismatch"),
    }
}
pub(in crate::tenancy::receipts) async fn validate(
    db: &impl ConnectionTrait,
    candidate: &mut Candidate,
    expected: Option<&str>,
) -> Result<()> {
    candidate.usage_evidence =
        load(db, &candidate.receipt, &candidate.digest, expected, None).await?;
    Ok(())
}
#[cfg(all(feature = "core", feature = "harness"))]
pub(in crate::tenancy::receipts) async fn validate_bounded(
    db: &impl ConnectionTrait,
    candidate: &mut Candidate,
    expected: Option<&str>,
    charge: &Charge<'_>,
) -> Result<()> {
    candidate.usage_evidence = load(
        db,
        &candidate.receipt,
        &candidate.digest,
        expected,
        Some(charge),
    )
    .await?;
    Ok(())
}
pub(in crate::tenancy::receipts) async fn insert(
    db: &impl ConnectionTrait,
    candidate: &Candidate,
    new_receipt: bool,
) -> Result<()> {
    let expected = candidate.usage_evidence.as_ref().map(|e| e.digest.as_str());
    if let Some(evidence) = &candidate.usage_evidence
        && new_receipt
    {
        let receipt = &candidate.receipt;
        let fields: Vec<(&str, sea_orm::Value)> = vec![
            ("installation_id", receipt.installation_id.as_str().into()),
            ("tenant_id", receipt.tenant_id.as_str().into()),
            ("execution_id", receipt.execution_id.as_str().into()),
            ("schema_version", 1_i32.into()),
            ("receipt_digest", candidate.digest.clone().into()),
            ("evidence_digest", evidence.digest.clone().into()),
            ("evidence_json", evidence.json.clone().into()),
            ("byte_count", i64::try_from(evidence.json.len())?.into()),
        ];
        let mut query = Query::insert();
        query
            .into_table(Alias::new(TABLE))
            .columns(fields.iter().map(|(n, _)| Alias::new(*n)))
            .values(fields.into_iter().map(|(_, v)| v.into()))?;
        ensure!(
            db.execute(db.get_database_backend().build(&query))
                .await?
                .rows_affected()
                == 1,
            "usage evidence insert count"
        );
    }
    load(db, &candidate.receipt, &candidate.digest, expected, None).await?;
    Ok(())
}

/// Bounded SQL preflight before keyset traversal. Never trusts projected counts
/// or silently treats a deleted sidecar as historical absence.
pub(in crate::tenancy::receipts) async fn inventory(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
) -> Result<u64> {
    crate::db::migration::m20240101_000040_usage_evidence::validate(db).await?;
    let backend = db.get_database_backend();
    let bytes = octets(backend, "evidence_json");
    let mut invalid = Query::select();
    invalid.from(Alias::new(TABLE)).expr(Expr::val(1)).and_where(Expr::cust(format!("byte_count != {bytes} OR byte_count < 1 OR {bytes} > {MAX_BYTES} OR schema_version != 1 OR {} > 128 OR {} > 128 OR {} > 128 OR {} != 64 OR {} != 64", octets(backend,"installation_id"),octets(backend,"tenant_id"),octets(backend,"execution_id"),octets(backend,"evidence_digest"),octets(backend,"receipt_digest")))).limit(1);
    ensure!(
        db.query_one(backend.build(&invalid)).await?.is_none(),
        "usage evidence inventory bounds"
    );
    if backend == DatabaseBackend::Sqlite {
        let mut types = Query::select();
        types.from(Alias::new(TABLE)).expr(Expr::val(1)).and_where(Expr::cust("typeof(byte_count)!='integer' OR typeof(schema_version)!='integer' OR typeof(evidence_json)!='text'")).limit(1);
        ensure!(
            db.query_one(backend.build(&types)).await?.is_none(),
            "usage evidence SQL types"
        );
    }
    let mut foreign = Query::select();
    foreign
        .from(Alias::new(TABLE))
        .expr(Expr::val(1))
        .and_where(Expr::col(Alias::new("installation_id")).ne(installation.as_str()))
        .limit(1);
    ensure!(
        db.query_one(backend.build(&foreign)).await?.is_none(),
        "foreign usage evidence"
    );
    let marker = match backend {
        DatabaseBackend::Sqlite => "json_extract(r.disposition_json,'$.usage_evidence_digest')",
        DatabaseBackend::Postgres => "r.disposition_json::jsonb->>'usage_evidence_digest'",
        DatabaseBackend::MySql => {
            "JSON_UNQUOTE(JSON_EXTRACT(r.disposition_json,'$.usage_evidence_digest'))"
        }
    };
    let join = "e.installation_id=r.installation_id AND e.tenant_id=r.tenant_id AND e.execution_id=r.execution_id";
    for sql in [
        format!(
            "SELECT 1 FROM tenancy_execution_receipts r LEFT JOIN {TABLE} e ON {join} WHERE {marker} IS NOT NULL AND e.execution_id IS NULL LIMIT 1"
        ),
        format!(
            "SELECT 1 FROM {TABLE} e LEFT JOIN tenancy_execution_receipts r ON {join} WHERE r.execution_id IS NULL OR {marker} IS NULL LIMIT 1"
        ),
    ] {
        ensure!(
            db.query_one(sea_orm::Statement::from_string(backend, sql))
                .await?
                .is_none(),
            "usage evidence orphan or missing sidecar"
        );
    }
    let mut after: Option<(TenantId, ExecutionId)> = None;
    let mut total = 0_u64;
    let mut count = 0_usize;
    loop {
        let mut query = Query::select();
        query
            .from(Alias::new(TABLE))
            .columns(["tenant_id", "execution_id"].map(Alias::new))
            .and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()))
            .order_by(Alias::new("tenant_id"), Order::Asc)
            .order_by(Alias::new("execution_id"), Order::Asc)
            .limit(64);
        if let Some((tenant, id)) = &after {
            query.and_where(
                Expr::col(Alias::new("tenant_id"))
                    .gt(tenant.as_str())
                    .or(Expr::col(Alias::new("tenant_id"))
                        .eq(tenant.as_str())
                        .and(Expr::col(Alias::new("execution_id")).gt(id.as_str()))),
            );
        }
        let rows = db.query_all(backend.build(&query)).await?;
        if rows.is_empty() {
            break;
        }
        count = count
            .checked_add(rows.len())
            .context("usage evidence inventory count")?;
        ensure!(count <= 524288, "usage evidence inventory row bound");
        for row in rows {
            let tenant: TenantId = row.try_get::<String>("", "tenant_id")?.try_into()?;
            let id: ExecutionId = row.try_get::<String>("", "execution_id")?.try_into()?;
            let candidate = super::super::store::lookup_candidate(db, installation, &tenant, &id)
                .await?
                .context("usage evidence receipt missing")?;
            let evidence = candidate.usage_evidence.context("usage evidence missing")?;
            total = total
                .checked_add(evidence.json.len() as u64 + 16 * 1024)
                .context("usage evidence inventory overflow")?;
            ensure!(
                total <= super::super::capacity::HIGH_WATER,
                "usage evidence inventory capacity"
            );
            after = Some((tenant, id));
        }
    }
    Ok(total)
}
