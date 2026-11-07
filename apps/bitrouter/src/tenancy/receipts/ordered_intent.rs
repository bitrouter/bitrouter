//! Write-ahead obligation only: recovery terminalizes unknown work, never replays it.
use super::CandidateV2;
use crate::tenancy::{
    origin::Stamp,
    receipts::{
        owner::Identity,
        store::{self, Disposition, format::Terminal},
    },
    transaction::Control,
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    execution::{Outcome, Provenance, ReceiptState, Usage},
    execution_v2::ExecutionReceiptV2,
    identity::{
        CorrelationId, ExecutionId, InstallationId, PrincipalId, RevisionId, TenantId, Timestamp,
    },
    operation::DispatchState,
    ordered::AcceptedSelectionV2,
    usage_evidence::{Capture, Estimation},
    usage_evidence_v2::{EvidenceBudgetV2, ExecutionUsageEvidenceV2},
    validation::Validate,
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, DatabaseTransaction,
    sea_query::{Alias, Expr, Order, Query},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;

const TABLE: &str = "tenancy_dispatch_intents";
pub(super) const MAX_BYTES: usize = 16 * 1024;
// Includes receipt, evidence, disposition, intent and all three row charges.
const MAX_OBLIGATION: u64 = 1024 * 1024 + 2 * 1024 * 1024 + 1024 + 16 * 1024 + 3 * 16 * 1024;
const _: () = assert!(
    MAX_OBLIGATION == 3_212_288
        && MAX_OBLIGATION <= crate::tenancy::receipts::capacity::RESERVATION
);

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tenancy) struct Intent {
    schema_version: u8,
    installation: InstallationId,
    tenant: TenantId,
    principal: PrincipalId,
    execution: ExecutionId,
    correlation: CorrelationId,
    revision: RevisionId,
    selection: AcceptedSelectionV2,
    attempt_limit: u8,
    evidence_bytes: usize,
    frame_bytes: usize,
    reserved_bytes: usize,
    frames: usize,
    origin: Stamp,
    admitted_at: Timestamp,
}
impl Intent {
    pub(in crate::tenancy) fn new(identity: &Identity, origin: Stamp) -> Result<Self> {
        let (selection, limit) = identity
            .ordered
            .as_ref()
            .context("ordered intent selection missing")?;
        let budget = EvidenceBudgetV2::for_attempts(selection.candidate_indices.len())?;
        let intent = Self {
            schema_version: 2,
            installation: identity.installation.clone(),
            tenant: identity.tenant.clone(),
            principal: identity.principal.clone(),
            execution: identity.execution.clone(),
            correlation: identity.correlation.clone(),
            revision: identity.revision.clone(),
            selection: selection.clone(),
            attempt_limit: *limit,
            evidence_bytes: budget.max_bytes,
            frame_bytes: budget.max_frame_bytes,
            reserved_bytes: budget.reserved_bytes,
            frames: budget.max_frames,
            origin,
            admitted_at: chrono::Utc::now()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                .try_into()?,
        };
        intent.validate()?;
        Ok(intent)
    }
    fn validate(&self) -> Result<()> {
        self.selection.validate()?;
        self.origin.validate()?;
        let b = EvidenceBudgetV2::for_attempts(self.selection.candidate_indices.len())?;
        ensure!(
            self.schema_version == 2
                && (1..=64).contains(&self.attempt_limit)
                && self.selection.candidate_indices.len() <= usize::from(self.attempt_limit),
            "intent attempt budget"
        );
        ensure!(
            self.evidence_bytes == b.max_bytes
                && self.frame_bytes == b.max_frame_bytes
                && self.reserved_bytes == b.reserved_bytes
                && self.frames == b.max_frames,
            "intent evidence budget"
        );
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_BYTES,
            "intent byte limit"
        );
        Ok(())
    }
    fn matches(&self, receipt: &ExecutionReceiptV2) -> bool {
        self.installation == receipt.installation_id
            && self.tenant == receipt.tenant_id
            && self.principal == receipt.principal_id
            && self.execution == receipt.execution_id
            && self.correlation == receipt.correlation_id
            && self.revision == receipt.execution_revision
            && self.selection == receipt.accepted_selection
            && self.attempt_limit == receipt.attempt_limit
    }
    fn unknown(&self) -> Result<Terminal> {
        let receipt = ExecutionReceiptV2 {
            schema_version: 2,
            installation_id: self.installation.clone(),
            tenant_id: self.tenant.clone(),
            principal_id: self.principal.clone(),
            execution_id: self.execution.clone(),
            correlation_id: self.correlation.clone(),
            execution_revision: self.revision.clone(),
            accepted_selection: self.selection.clone(),
            attempt_limit: self.attempt_limit,
            state: ReceiptState::Unknown,
            outcome: Outcome::Unknown,
            attempts: Vec::new(),
            usage: Usage {
                input_tokens: None,
                output_tokens: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
                provenance: Provenance::Unknown,
                complete: false,
            },
            recorded_at: self.admitted_at.clone(),
        };
        let evidence = ExecutionUsageEvidenceV2 {
            schema_version: 2,
            installation_id: self.installation.clone(),
            tenant_id: self.tenant.clone(),
            execution_id: self.execution.clone(),
            accepted_selection: self.selection.clone(),
            capture: Capture::Unavailable,
            extractor_version: 1,
            estimation: Estimation::NotCaptured,
            estimate: None,
            attempts: Vec::new(),
            frames: Vec::new(),
            truncated: false,
            dropped_frames: 0,
        };
        Ok(Terminal::V2(Arc::new(CandidateV2::new(
            receipt,
            Disposition {
                dispatch: DispatchState::Unknown,
                statuses: Vec::new(),
                auxiliary_digest: None,
            },
            evidence,
        )?)))
    }
}
fn digest(json: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"bitrouter.dispatch.intent.v2");
    h.update((json.len() as u64).to_be_bytes());
    h.update(json.as_bytes());
    hex::encode(h.finalize())
}
fn octets(backend: DatabaseBackend, col: &str) -> String {
    if backend == DatabaseBackend::Sqlite {
        format!("length(CAST({col} AS BLOB))")
    } else {
        format!("octet_length({col})")
    }
}
fn point(
    installation: &InstallationId,
    tenant: &TenantId,
    execution: &ExecutionId,
) -> sea_orm::sea_query::SelectStatement {
    Query::select()
        .from(Alias::new(TABLE))
        .and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()))
        .and_where(Expr::col(Alias::new("tenant_id")).eq(tenant.as_str()))
        .and_where(Expr::col(Alias::new("execution_id")).eq(execution.as_str()))
        .to_owned()
}
async fn load(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    tenant: &TenantId,
    execution: &ExecutionId,
) -> Result<Option<Intent>> {
    let backend = db.get_database_backend();
    let length = octets(backend, "intent_json");
    let integer = if backend == DatabaseBackend::Sqlite {
        "typeof(byte_count)='integer' AND typeof(intent_json)='text' AND "
    } else {
        ""
    };
    let cast = if backend == DatabaseBackend::MySql {
        "SIGNED"
    } else {
        "BIGINT"
    };
    let mut scalar = point(installation, tenant, execution);
    scalar.expr_as(Expr::cust(format!("CASE WHEN {integer}{length} BETWEEN 1 AND {MAX_BYTES} AND byte_count={length} AND {}=64 THEN CAST({length} AS {cast}) ELSE NULL END", octets(backend,"content_digest"))), Alias::new("bytes")).limit(2);
    let rows = db.query_all(backend.build(&scalar)).await?;
    ensure!(rows.len() <= 1, "intent multiplicity");
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let bytes: i64 = row.try_get("", "bytes")?;
    let mut query = point(installation, tenant, execution);
    query
        .expr_as(
            Expr::cust(format!(
                "CASE WHEN {length}={bytes} THEN intent_json ELSE NULL END"
            )),
            Alias::new("intent_json"),
        )
        .expr_as(
            Expr::cust(format!(
                "CASE WHEN {}=64 THEN content_digest ELSE NULL END",
                octets(backend, "content_digest")
            )),
            Alias::new("content_digest"),
        )
        .limit(2);
    let rows = db.query_all(backend.build(&query)).await?;
    ensure!(rows.len() == 1, "intent changed during read");
    let row = rows.first().context("intent missing")?;
    let json: String = row.try_get("", "intent_json")?;
    let intent: Intent = serde_json::from_str(&json)?;
    intent.validate()?;
    ensure!(
        &intent.installation == installation
            && &intent.tenant == tenant
            && &intent.execution == execution
            && serde_json::to_string(&intent)? == json
            && row.try_get::<String>("", "content_digest")? == digest(&json),
        "intent integrity"
    );
    Ok(Some(intent))
}
pub(in crate::tenancy) async fn insert(tx: &DatabaseTransaction, intent: &Intent) -> Result<()> {
    intent.validate()?;
    if let Some(existing) =
        load(tx, &intent.installation, &intent.tenant, &intent.execution).await?
    {
        ensure!(
            serde_json::to_string(&existing)? == serde_json::to_string(intent)?,
            "immutable intent conflict"
        );
        return Ok(());
    }
    ensure!(
        store::lookup_terminal(tx, &intent.installation, &intent.tenant, &intent.execution)
            .await?
            .is_none(),
        "intent cannot resurrect a terminal execution"
    );
    let json = serde_json::to_string(intent)?;
    let fields: Vec<(&str, sea_orm::Value)> = vec![
        ("installation_id", intent.installation.as_str().into()),
        ("tenant_id", intent.tenant.as_str().into()),
        ("execution_id", intent.execution.as_str().into()),
        ("content_digest", digest(&json).into()),
        ("byte_count", i64::try_from(json.len())?.into()),
        ("intent_json", json.into()),
    ];
    let mut query = Query::insert();
    query
        .into_table(Alias::new(TABLE))
        .columns(fields.iter().map(|(n, _)| Alias::new(*n)))
        .values(fields.into_iter().map(|(_, v)| v.into()))?;
    ensure!(
        tx.execute(tx.get_database_backend().build(&query))
            .await?
            .rows_affected()
            == 1,
        "intent insert count"
    );
    let saved = load(tx, &intent.installation, &intent.tenant, &intent.execution)
        .await?
        .context("intent insert missing")?;
    ensure!(
        serde_json::to_string(&saved)? == serde_json::to_string(intent)?,
        "intent insert changed"
    );
    Ok(())
}
pub(in crate::tenancy) async fn retire(
    tx: &DatabaseTransaction,
    candidate: &CandidateV2,
    changed: bool,
) -> Result<()> {
    let r = &candidate.receipt;
    let Some(intent) = load(tx, &r.installation_id, &r.tenant_id, &r.execution_id).await? else {
        ensure!(!changed, "ordered terminal missing intent");
        return Ok(());
    };
    ensure!(
        changed && intent.matches(r),
        "ordered terminal intent binding"
    );
    let delete = Query::delete()
        .from_table(Alias::new(TABLE))
        .and_where(Expr::col(Alias::new("installation_id")).eq(r.installation_id.as_str()))
        .and_where(Expr::col(Alias::new("tenant_id")).eq(r.tenant_id.as_str()))
        .and_where(Expr::col(Alias::new("execution_id")).eq(r.execution_id.as_str()))
        .to_owned();
    ensure!(
        tx.execute(tx.get_database_backend().build(&delete))
            .await?
            .rows_affected()
            == 1,
        "intent retirement count"
    );
    ensure!(
        load(tx, &r.installation_id, &r.tenant_id, &r.execution_id)
            .await?
            .is_none(),
        "intent retirement not durable"
    );
    Ok(())
}

pub(in crate::tenancy) struct PendingInventory {
    pub rows: u64,
    pub unknown: Vec<(TenantId, ExecutionId, chrono::DateTime<chrono::Utc>)>,
}

/// Bounded scalar inventory includes every outstanding obligation, even before repair.
pub(in crate::tenancy) async fn inventory(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
) -> Result<PendingInventory> {
    crate::db::migration::m20240101_000045_ordered_receipts::validate(db).await?;
    let backend = db.get_database_backend();
    let length = octets(backend, "intent_json");
    let invalid=Query::select().from(Alias::new(TABLE)).expr(Expr::val(1)).and_where(Expr::cust(format!("{length} NOT BETWEEN 1 AND {MAX_BYTES} OR byte_count != {length} OR {} != 64 OR {} >128 OR {} >128 OR {} >128",octets(backend,"content_digest"),octets(backend,"installation_id"),octets(backend,"tenant_id"),octets(backend,"execution_id")))).limit(1).to_owned();
    ensure!(
        db.query_one(backend.build(&invalid)).await?.is_none(),
        "intent inventory length"
    );
    let foreign = Query::select()
        .from(Alias::new(TABLE))
        .expr(Expr::val(1))
        .and_where(Expr::col(Alias::new("installation_id")).ne(installation.as_str()))
        .limit(1)
        .to_owned();
    ensure!(
        db.query_one(backend.build(&foreign)).await?.is_none(),
        "foreign dispatch intent"
    );
    let query = Query::select()
        .from(Alias::new(TABLE))
        .columns(["tenant_id", "execution_id"].map(Alias::new))
        .order_by(Alias::new("tenant_id"), Order::Asc)
        .order_by(Alias::new("execution_id"), Order::Asc)
        .limit(1001)
        .to_owned();
    let rows = db.query_all(backend.build(&query)).await?;
    ensure!(rows.len() <= 1000, "intent instance capacity");
    let mut unknown = Vec::with_capacity(rows.len());
    let mut counts = std::collections::BTreeMap::new();
    let mut bytes = 0_u64;
    for row in rows {
        let tenant: TenantId = row.try_get::<String>("", "tenant_id")?.try_into()?;
        let id: ExecutionId = row.try_get::<String>("", "execution_id")?.try_into()?;
        let n = counts.entry(tenant.clone()).or_insert(0_usize);
        *n += 1;
        ensure!(*n <= 100, "intent tenant capacity");
        let intent = load(db, installation, &tenant, &id)
            .await?
            .context("intent inventory missing")?;
        ensure!(
            store::lookup_terminal(db, installation, &tenant, &id)
                .await?
                .is_none(),
            "intent coexists with terminal receipt"
        );
        // Reserve the whole obligation during startup, not just its tiny seed.
        bytes = bytes
            .checked_add(MAX_OBLIGATION - 16 * 1024)
            .context("intent inventory overflow")?;
        unknown.push((
            tenant,
            id,
            chrono::DateTime::parse_from_rfc3339(intent.admitted_at.as_str())?
                .with_timezone(&chrono::Utc),
        ));
    }
    Ok(PendingInventory {
        rows: unknown.len() as u64,
        unknown,
    })
}

pub(in crate::tenancy) async fn recover(
    db: &DatabaseConnection,
    installation: &InstallationId,
    startup: Option<&Arc<crate::tenancy::origin::StartupOwner>>,
    proof: &Arc<crate::tenancy::transaction::Proof>,
    control: &Control,
) -> Result<()> {
    let pending = inventory(db, installation).await?;
    if pending.rows == 0 {
        return Ok(());
    }
    let current = startup
        .context("ordered recovery requires exclusive owner")?
        .bootstrap_stamp(proof)?;
    for (tenant, id, _) in pending.unknown {
        control.check_read()?;
        let tx = control.begin(db).await?;
        let result = async {
            let intent = load(&tx, installation, &tenant, &id)
                .await?
                .context("recovery intent missing")?;
            ensure!(
                intent.origin != current,
                "cannot recover work from current owner"
            );
            let terminal = intent.unknown()?;
            let staged = store::stage_dispatch_terminal(&tx, &terminal).await?;
            let _change = staged.reconcile(&|_, _| control.check_read()).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        match result {
            Ok(()) => control.commit(tx).await?,
            Err(error) => {
                control.rollback(tx).await?;
                return Err(error);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "ordered_tests.rs"]
mod tests;
