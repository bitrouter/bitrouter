//! Additive037 private evidence, committed atomically with the unchanged wire receipt.
use super::{
    auxiliary::{MAX_BYTES, Outcome, Snapshot},
    store::Candidate,
};
#[cfg(test)]
use anyhow::Context;
use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::{
    execution::ExecutionReceipt,
    identity::{ExecutionId, InstallationId, TenantId},
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, SqlErr,
    sea_query::{Alias, Expr, Func, OnConflict, Query},
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

/// A row can discharge this exact pending refresh only after both immutable
/// records and their owner/work/generation linkage have been checked. Equal
/// names or token bytes in another slot never participate in this lookup.
#[cfg(test)]
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
