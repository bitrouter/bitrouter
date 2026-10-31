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
async fn load(
    db: &impl ConnectionTrait,
    receipt: &ExecutionReceipt,
    receipt_digest: &str,
    expected: Option<&str>,
) -> Result<Option<Evidence>> {
    let backend = db.get_database_backend();
    let mut query = scope(receipt);
    for (name, bound) in [
        ("evidence_json", MAX_BYTES),
        ("evidence_digest", 64),
        ("receipt_digest", 64),
    ] {
        query.expr_as(
            Expr::cust(format!(
                "CASE WHEN {} <= {bound} THEN {name} ELSE NULL END",
                octets(backend, name)
            )),
            Alias::new(name),
        );
    }
    query
        .column(Alias::new("byte_count"))
        .column(Alias::new("schema_version"))
        .limit(2);
    let rows = db.query_all(backend.build(&query)).await?;
    ensure!(rows.len() <= 1, "usage evidence multiplicity");
    match (expected, rows.first()) {
        (None, None) => Ok(None),
        (Some(expected), Some(row)) => {
            let json: String = row.try_get("", "evidence_json")?;
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
    candidate.usage_evidence = load(db, &candidate.receipt, &candidate.digest, expected).await?;
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
    load(db, &candidate.receipt, &candidate.digest, expected).await?;
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
