//! Canonical classification and indexed source keys. No stored summary flag is
//! trusted instead of the receipt/auxiliary/OAuth metadata it describes.
use super::*;
use crate::tenancy::{
    oauth::storage as oauth,
    receipts::auxiliary::{Outcome, Snapshot},
};

pub(super) struct Sources {
    pub mask: u8,
    pub first: Option<DateTime<Utc>>,
}
impl Sources {
    fn add(&mut self, flag: u8, at: DateTime<Utc>) {
        self.mask |= flag;
        self.first = Some(self.first.map_or(at, |first| first.min(at)));
    }
}

fn add_pending(
    sources: &mut Sources,
    candidate: Option<&Candidate>,
    metadata: &oauth::Metadata,
    key: &Key,
) -> Result<()> {
    let work = match &metadata.state {
        oauth::State::RefreshStarted(work) | oauth::State::Quarantined(work) => work,
        oauth::State::Ready => return Ok(()),
    };
    if work.tenant != key.tenant || work.execution != key.execution {
        return Ok(());
    }
    if !witnessed(candidate, metadata, work)? {
        sources.add(
            PENDING,
            DateTime::parse_from_rfc3339(work.started_at.as_str())?.with_timezone(&Utc),
        );
    }
    Ok(())
}

// Same exact immutable binding as auxiliary_store::pending_receipted, evaluated
// against the already charged canonical candidate rather than reloading it.
fn witnessed(
    candidate: Option<&Candidate>,
    metadata: &oauth::Metadata,
    work: &oauth::Work,
) -> Result<bool> {
    let Some(candidate) = candidate else {
        return Ok(false);
    };
    let Some(evidence) = &candidate.auxiliary else {
        return Ok(false);
    };
    let snapshot: Snapshot = serde_json::from_str(&evidence.json)?;
    let Snapshot::Refresh(facts) = &snapshot else {
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
pub(super) async fn read(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    key: &Key,
    charge: &Charge<'_>,
) -> Result<Sources> {
    read_expected(db, installation, key, None, charge).await
}

pub(super) async fn read_expected(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    key: &Key,
    expected: Option<&Candidate>,
    charge: &Charge<'_>,
) -> Result<Sources> {
    let candidate = super::super::load::terminal(
        db,
        installation,
        &key.tenant,
        &key.execution,
        Some(charge),
        expected.is_some(),
    )
    .await?;
    if let Some(expected) = expected {
        super::super::same_completion(
            match candidate.as_ref() {
                Some(format::Terminal::Strict(c)) => c,
                _ => anyhow::bail!("strict terminal receipt disappeared"),
            },
            expected,
        )?;
    }
    let mut sources = Sources {
        mask: 0,
        first: None,
    };
    if let Some(candidate) = &candidate {
        let at = DateTime::parse_from_rfc3339(candidate.facts().recorded_at.as_str())?
            .with_timezone(&Utc);
        if candidate.has_unknown_outcome() {
            sources.add(RECEIPT, at);
        }
        if candidate.auxiliary_unknown() {
            sources.add(AUXILIARY, at);
        }
    } else {
        charge(2, 1024)?;
        for table in ["tenancy_usage_evidence", "tenancy_receipt_auxiliary"] {
            let backend = supported(db)?;
            let query = Query::select()
                .from(Alias::new(table))
                .expr(Expr::val(1_i32))
                .and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()))
                .and_where(Expr::col(Alias::new("tenant_id")).eq(key.tenant.as_str()))
                .and_where(Expr::col(Alias::new("execution_id")).eq(key.execution.as_str()))
                .limit(1)
                .to_owned();
            ensure!(
                db.query_one(backend.build(&query)).await?.is_none(),
                "orphan unknown source evidence"
            );
        }
    }
    let strict = match &candidate {
        Some(format::Terminal::Strict(c)) => Some(c.as_ref()),
        _ => None,
    };
    let mut after: Option<String> = None;
    loop {
        let page = oauth::pending::for_execution(
            db,
            installation,
            &key.tenant,
            &key.execution,
            after.as_deref(),
            charge,
        )
        .await?;
        if page.is_empty() {
            break;
        }
        for metadata in page {
            charge(0, crate::tenancy::receipts::auxiliary::MAX_BYTES * 2)?;
            add_pending(&mut sources, strict, &metadata, key)?;
            after = Some(metadata.slot);
        }
    }
    charge(0, 0)?;
    Ok(sources)
}

/// Canonical receipt keys for complete validation/backfill, including known
/// receipts. Caller retains the final key and a stable read/exclusive writer.
pub(in crate::tenancy) async fn receipt_keys(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    after: Option<&Key>,
    charge: &Charge<'_>,
) -> Result<Vec<Key>> {
    keys(db, installation, after, false, charge).await
}
/// Distinct pending execution keys through the execution index. This does not
/// replace full OAuth slot validation, which also inspects falsely null rows.
pub(in crate::tenancy) async fn pending_keys(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    after: Option<&Key>,
    charge: &Charge<'_>,
) -> Result<Vec<Key>> {
    keys(db, installation, after, true, charge).await
}
async fn keys(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    after: Option<&Key>,
    pending: bool,
    charge: &Charge<'_>,
) -> Result<Vec<Key>> {
    let backend = supported(db)?;
    charge(PAGE, PAGE * 512)?;
    let (table, tenant, execution) = if pending {
        (
            "tenancy_oauth_slots",
            "pending_tenant_id",
            "pending_execution_id",
        )
    } else {
        ("tenancy_execution_receipts", "tenant_id", "execution_id")
    };
    // Grouping and ordering operate on the raw covering-index columns; guarded
    // projections are decoded below and never silently omitted as invalid keys.
    let mut query = Query::select();
    let guarded = |name: &str| {
        if backend == DatabaseBackend::Sqlite {
            format!(
                "CASE WHEN typeof({name})='text' AND length(CAST({name} AS BLOB)) BETWEEN 1 AND 128 THEN {name} ELSE NULL END"
            )
        } else {
            format!("CASE WHEN octet_length({name}) BETWEEN 1 AND 128 THEN {name} ELSE NULL END")
        }
    };
    query
        .from(Alias::new(table))
        .expr_as(Expr::cust(guarded(tenant)), Alias::new("tenant_id"))
        .expr_as(Expr::cust(guarded(execution)), Alias::new("execution_id"))
        .and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()))
        .order_by(Alias::new(tenant), Order::Asc)
        .order_by(Alias::new(execution), Order::Asc)
        .limit(PAGE as u64);
    if pending {
        query
            .group_by_columns([Alias::new(tenant), Alias::new(execution)])
            .and_where(Expr::col(Alias::new("pending_tenant_id")).is_not_null())
            .and_where(Expr::col(Alias::new("pending_execution_id")).is_not_null());
    }
    if let Some(after) = after {
        let tenant_key = Expr::col(Alias::new(tenant));
        query.and_where(
            tenant_key.clone().gt(after.tenant.as_str()).or(tenant_key
                .eq(after.tenant.as_str())
                .and(Expr::col(Alias::new(execution)).gt(after.execution.as_str()))),
        );
    }
    let statement = backend.build(&query);
    let rows = db.query_all(statement).await?;
    let mut result = Vec::new();
    result.try_reserve_exact(rows.len())?;
    let mut previous = after.cloned();
    for row in rows {
        let key = Key {
            tenant: row.try_get::<String>("", "tenant_id")?.try_into()?,
            execution: row.try_get::<String>("", "execution_id")?.try_into()?,
        };
        ensure!(
            previous.as_ref().is_none_or(|old| old < &key),
            "unknown source key order"
        );
        previous = Some(key.clone());
        result.push(key);
    }
    charge(0, 0)?;
    Ok(result)
}
