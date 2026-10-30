//! Additive037 private evidence, committed atomically with the unchanged wire receipt.
use super::{
    auxiliary::{MAX_BYTES, Outcome, Snapshot},
    store::Candidate,
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    execution::ExecutionReceipt,
    identity::{ExecutionId, InstallationId, TenantId},
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, SqlErr,
    sea_query::{Alias, Expr, Func, OnConflict, Order, Query},
};
use sha2::{Digest, Sha256};

const TABLE: &str = "tenancy_receipt_auxiliary";
pub(super) struct Evidence {
    pub json: String,
    pub digest: String,
    facts: Snapshot,
}
impl Evidence {
    pub fn new(receipt: &ExecutionReceipt, facts: Snapshot) -> Result<Self> {
        let domain = match &facts {
            Snapshot::Refresh(facts) => {
                ensure!(facts.len() == 1, "OAuth evidence count");
                validate_refresh(receipt, &facts[0])?;
                "bitrouter.receipt.auxiliary.v1"
            }
            Snapshot::Counts(counts) => {
                ensure!(counts.validate(), "count evidence shape");
                for fact in &counts.counts {
                    ensure!(
                        fact.credential.resource_kind
                            == bitrouter_ai::tenancy::authority::ResourceKind::Credential
                            && (fact.credential.owner_kind
                                != bitrouter_ai::tenancy::authority::OwnerKind::Tenant
                                || fact.credential.owner_id.as_str() == receipt.tenant_id.as_str()),
                        "count evidence foreign scope"
                    );
                }
                "bitrouter.receipt.auxiliary.count.v2"
            }
        };
        let json = serde_json::to_string(&facts)?;
        ensure!(json.len() <= MAX_BYTES, "auxiliary evidence bytes");
        let digest = digest(
            domain,
            &receipt.installation_id,
            &receipt.tenant_id,
            &receipt.execution_id,
            &json,
        );
        Ok(Self {
            json,
            digest,
            facts,
        })
    }
    pub fn unclean(&self) -> bool {
        self.facts.unclean()
    }
    pub fn unknown(&self) -> bool {
        self.facts.unknown()
    }
}
fn validate_refresh(receipt: &ExecutionReceipt, fact: &super::auxiliary::Fact) -> Result<()> {
    ensure!(
        fact.slot
            == crate::tenancy::oauth::storage::slot(&receipt.installation_id, &fact.credential)?,
        "OAuth evidence slot identity mismatch"
    );
    ensure!(
        fact.slot.len() == 64
            && fact.slot.bytes().all(|byte| byte.is_ascii_hexdigit())
            && uuid::Uuid::parse_str(&fact.work).is_ok()
            && fact
                .http_status
                .is_none_or(|status| (200..=599).contains(&status))
            && fact
                .request_digest
                .as_ref()
                .is_none_or(|digest| digest.len() == 64
                    && digest.bytes().all(|byte| byte.is_ascii_hexdigit())),
        "OAuth evidence shape"
    );
    ensure!(
        fact.credential.owner_kind != bitrouter_ai::tenancy::authority::OwnerKind::Tenant
            || fact.credential.owner_id.as_str() == receipt.tenant_id.as_str(),
        "OAuth evidence foreign scope"
    );
    ensure!(
        fact.credential.resource_kind == bitrouter_ai::tenancy::authority::ResourceKind::Credential
            && fact.generation <= i64::MAX as u64,
        "OAuth evidence authority shape"
    );
    let coherent = match fact.outcome {
        Outcome::NotSent => fact.request_digest.is_none() && fact.http_status.is_none(),
        Outcome::Refreshed => {
            fact.request_digest.is_some()
                && fact.http_status.is_some_and(|s| (200..=299).contains(&s))
        }
        Outcome::Rejected => {
            fact.request_digest.is_some()
                && fact.http_status.is_some_and(|s| !(200..=299).contains(&s))
        }
        Outcome::Unknown => {
            fact.request_digest.is_some()
                && fact.http_status.is_none_or(|s| (200..=299).contains(&s))
        }
    };
    ensure!(coherent, "OAuth evidence disposition mismatch");
    Ok(())
}
fn digest(
    domain: &str,
    installation: &InstallationId,
    tenant: &TenantId,
    execution: &ExecutionId,
    json: &str,
) -> String {
    let mut hash = Sha256::new();
    hash.update(domain.as_bytes());
    for value in [
        installation.as_str(),
        tenant.as_str(),
        execution.as_str(),
        json,
    ] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }
    hex::encode(hash.finalize())
}
fn scope(receipt: &ExecutionReceipt) -> sea_orm::sea_query::SelectStatement {
    Query::select()
        .from(Alias::new(TABLE))
        .and_where(Expr::col(Alias::new("installation_id")).eq(receipt.installation_id.as_str()))
        .and_where(Expr::col(Alias::new("tenant_id")).eq(receipt.tenant_id.as_str()))
        .and_where(Expr::col(Alias::new("execution_id")).eq(receipt.execution_id.as_str()))
        .to_owned()
}
pub(super) async fn validate(
    db: &impl ConnectionTrait,
    candidate: &mut Candidate,
    expected: Option<&str>,
) -> Result<()> {
    let backend = db.get_database_backend();
    let mut query = scope(&candidate.receipt);
    for (name, max) in [
        ("evidence_json", MAX_BYTES as u64),
        ("evidence_digest", 64),
        ("receipt_digest", 64),
    ] {
        let length = if backend == DatabaseBackend::Sqlite {
            Func::cust(Alias::new("length"))
                .arg(Expr::col(Alias::new(name)).cast_as(Alias::new("BLOB")))
        } else {
            Func::cust(Alias::new("octet_length")).arg(Expr::col(Alias::new(name)))
        };
        query.expr_as(
            Expr::case(Expr::expr(length).lte(max), Expr::col(Alias::new(name)))
                .finally(Expr::val(Option::<String>::None)),
            Alias::new(name),
        );
    }
    query.column(Alias::new("unknown_outcome"));
    match (expected, db.query_one(backend.build(&query)).await?) {
        (None, None) => Ok(()),
        (Some(expected), Some(row)) => {
            let json: String = row.try_get("", "evidence_json")?;
            let evidence = Evidence::new(&candidate.receipt, serde_json::from_str(&json)?)?;
            ensure!(
                evidence.json == json
                    && evidence.digest == expected
                    && row.try_get::<String>("", "evidence_digest")? == expected
                    && row.try_get::<String>("", "receipt_digest")? == candidate.digest
                    && row.try_get::<bool>("", "unknown_outcome")? == evidence.unknown(),
                "immutable OAuth receipt evidence mismatch"
            );
            candidate.auxiliary = Some(evidence);
            Ok(())
        }
        _ => anyhow::bail!("OAuth receipt evidence presence mismatch"),
    }
}
pub(in crate::tenancy) async fn insert(
    db: &impl ConnectionTrait,
    candidate: &Candidate,
) -> Result<()> {
    let Some(evidence) = &candidate.auxiliary else {
        return Ok(());
    };
    let receipt = &candidate.receipt;
    let fields: Vec<(&str, sea_orm::Value)> = vec![
        ("installation_id", receipt.installation_id.as_str().into()),
        ("tenant_id", receipt.tenant_id.as_str().into()),
        ("execution_id", receipt.execution_id.as_str().into()),
        ("receipt_digest", candidate.digest.clone().into()),
        ("evidence_json", evidence.json.clone().into()),
        ("evidence_digest", evidence.digest.clone().into()),
        ("unknown_outcome", evidence.unknown().into()),
    ];
    let mut insert = Query::insert();
    insert
        .into_table(Alias::new(TABLE))
        .columns(fields.iter().map(|(name, _)| Alias::new(*name)))
        .values(fields.into_iter().map(|(_, value)| value.into()))?;
    let backend = db.get_database_backend();
    if backend != DatabaseBackend::MySql {
        insert.on_conflict(OnConflict::new().do_nothing().to_owned());
    }
    match db.execute(backend.build(&insert)).await {
        Ok(_) => {}
        Err(error)
            if backend == DatabaseBackend::MySql
                && matches!(error.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))) => {}
        Err(error) => return Err(error.into()),
    }
    let mut query = scope(receipt);
    query
        .expr(Expr::val(1))
        .and_where(Expr::col(Alias::new("receipt_digest")).eq(&candidate.digest))
        .and_where(Expr::col(Alias::new("evidence_digest")).eq(&evidence.digest))
        .and_where(Expr::col(Alias::new("evidence_json")).eq(&evidence.json))
        .and_where(Expr::col(Alias::new("unknown_outcome")).eq(evidence.unknown()));
    ensure!(
        db.query_one(backend.build(&query)).await?.is_some(),
        "immutable OAuth receipt conflict"
    );
    Ok(())
}

pub(in crate::tenancy) async fn inventory(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
) -> Result<(
    u64,
    Vec<(TenantId, ExecutionId, chrono::DateTime<chrono::Utc>)>,
)> {
    let backend = db.get_database_backend();
    let octets = if backend == DatabaseBackend::Sqlite {
        "length(CAST(a.evidence_json AS BLOB))"
    } else {
        "octet_length(a.evidence_json)"
    };
    // Validate nullable private disposition linkage before loading any body.
    // Old035 rows have no digest and no auxiliary row; new rows require both.
    let auxiliary_digest = match backend {
        DatabaseBackend::Sqlite => "json_extract(r.disposition_json, '$.auxiliary_digest')",
        DatabaseBackend::Postgres => "r.disposition_json::jsonb->>'auxiliary_digest'",
        DatabaseBackend::MySql => {
            "JSON_UNQUOTE(JSON_EXTRACT(r.disposition_json, '$.auxiliary_digest'))"
        }
    };
    let exact = format!(
        "SELECT 1 FROM tenancy_execution_receipts r LEFT JOIN tenancy_receipt_auxiliary a ON a.installation_id=r.installation_id AND a.tenant_id=r.tenant_id AND a.execution_id=r.execution_id WHERE ({auxiliary_digest} IS NULL AND a.execution_id IS NOT NULL) OR ({auxiliary_digest} IS NOT NULL AND (a.execution_id IS NULL OR a.evidence_digest<>{auxiliary_digest})) LIMIT 1"
    );
    ensure!(
        db.query_one(sea_orm::Statement::from_string(backend, exact))
            .await?
            .is_none(),
        "OAuth evidence inventory presence mismatch"
    );
    let exact = format!(
        "SELECT 1 FROM tenancy_receipt_auxiliary a LEFT JOIN tenancy_execution_receipts r ON a.installation_id=r.installation_id AND a.tenant_id=r.tenant_id AND a.execution_id=r.execution_id WHERE r.execution_id IS NULL OR a.receipt_digest<>r.content_digest OR {octets}>{MAX_BYTES} LIMIT 1"
    );
    ensure!(
        db.query_one(sea_orm::Statement::from_string(backend, exact))
            .await?
            .is_none(),
        "OAuth evidence inventory mismatch"
    );
    let foreign = Query::select()
        .from(Alias::new(TABLE))
        .expr(Expr::val(1))
        .and_where(Expr::col(Alias::new("installation_id")).ne(installation.as_str()))
        .limit(1)
        .to_owned();
    ensure!(
        db.query_one(backend.build(&foreign)).await?.is_none(),
        "foreign OAuth receipt installation"
    );
    // One key page plus one bounded point receipt/evidence pair at a time.
    // Bound total validation work by the existing receipt high-water envelope;
    // never materialize all retained JSON or trust a mutable unknown projection.
    let mut after: Option<(TenantId, ExecutionId)> = None;
    let mut bytes = 0_u64;
    let mut validated_bytes = 0_u64;
    let mut unknown = Vec::new();
    loop {
        let mut query = Query::select();
        query
            .from(Alias::new(TABLE))
            .and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()))
            .order_by(Alias::new("tenant_id"), Order::Asc)
            .order_by(Alias::new("execution_id"), Order::Asc)
            .limit(64);
        for name in ["tenant_id", "execution_id"] {
            let length = if backend == DatabaseBackend::Sqlite {
                Func::cust(Alias::new("length"))
                    .arg(Expr::col(Alias::new(name)).cast_as(Alias::new("BLOB")))
            } else {
                Func::cust(Alias::new("octet_length")).arg(Expr::col(Alias::new(name)))
            };
            query.expr_as(
                Expr::case(Expr::expr(length).lte(128), Expr::col(Alias::new(name)))
                    .finally(Expr::val(Option::<String>::None)),
                Alias::new(name),
            );
        }
        if let Some((tenant, execution)) = &after {
            query.and_where(
                Expr::col(Alias::new("tenant_id"))
                    .gt(tenant.as_str())
                    .or(Expr::col(Alias::new("tenant_id"))
                        .eq(tenant.as_str())
                        .and(Expr::col(Alias::new("execution_id")).gt(execution.as_str()))),
            );
        }
        let page = db.query_all(backend.build(&query)).await?;
        if page.is_empty() {
            break;
        }
        for row in page {
            let tenant: TenantId = row.try_get::<String>("", "tenant_id")?.try_into()?;
            let execution: ExecutionId = row.try_get::<String>("", "execution_id")?.try_into()?;
            if let Some(previous) = &after {
                ensure!(
                    (&tenant, &execution) > (&previous.0, &previous.1),
                    "OAuth inventory key order"
                );
            }
            let candidate = super::store::lookup_candidate(db, installation, &tenant, &execution)
                .await?
                .context("OAuth inventory receipt missing")?;
            let evidence = candidate
                .auxiliary
                .as_ref()
                .context("OAuth inventory evidence missing")?;
            let charged = (evidence.json.len() as u64)
                .checked_add(16 * 1024)
                .context("OAuth inventory overflow")?;
            bytes = bytes
                .checked_add(charged)
                .context("OAuth inventory overflow")?;
            validated_bytes = validated_bytes
                .checked_add(candidate.bytes())
                .and_then(|value| value.checked_add(charged + 16 * 1024))
                .context("OAuth validation work overflow")?;
            ensure!(
                validated_bytes <= super::capacity::HIGH_WATER,
                "OAuth inventory validation work bound"
            );
            if evidence.unknown() {
                ensure!(unknown.len() < 1000, "OAuth unknown evidence capacity");
                unknown.push((
                    tenant.clone(),
                    execution.clone(),
                    chrono::DateTime::parse_from_rfc3339(candidate.receipt.recorded_at.as_str())?
                        .with_timezone(&chrono::Utc),
                ));
            }
            after = Some((tenant, execution));
        }
    }
    Ok((bytes, unknown))
}

/// A row can discharge this exact pending refresh only after both immutable
/// records and their owner/work/generation linkage have been checked. Equal
/// names or token bytes in another slot never participate in this lookup.
pub(in crate::tenancy) async fn pending_receipted(
    db: &impl ConnectionTrait,
    metadata: &crate::tenancy::oauth::storage::Metadata,
    work: &crate::tenancy::oauth::storage::Work,
) -> Result<bool> {
    let Some(candidate) =
        super::store::lookup_candidate(db, &metadata.installation, &work.tenant, &work.execution)
            .await?
    else {
        return Ok(false);
    };
    let Some(evidence) = candidate.auxiliary else {
        return Ok(false);
    };
    let Snapshot::Refresh(facts) = &evidence.facts else {
        anyhow::bail!("pending OAuth matched non-refresh evidence");
    };
    let fact = facts.first().context("pending OAuth evidence missing")?;
    ensure!(
        candidate.receipt.principal_id == work.principal
            && fact.credential == metadata.reference
            && fact.slot == metadata.slot
            && fact.work == work.id
            && fact.generation == metadata.generation
            && fact.generation == work.expected_generation
            && matches!(fact.outcome, Outcome::Unknown | Outcome::Rejected),
        "pending OAuth receipt binding mismatch"
    );
    Ok(true)
}
