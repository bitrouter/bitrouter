//! Bounded exact-owner slot rows. Only H writes; no token hash search exists.
use super::{
    crypto::Crypto,
    profile::{self, Profile},
};
use crate::gateway::providers::auth::credentials::OAuthToken;
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    configuration::ResourceRef,
    identity::{ExecutionId, InstallationId, PrincipalId, TenantId, Timestamp},
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, QueryResult,
    sea_query::{Alias, Expr, Func, OnConflict, Order, Query, SelectStatement},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(in crate::tenancy) mod pending;

pub(in crate::tenancy) const TABLE: &str = "tenancy_oauth_slots";
const META_BYTES: usize = 8192;
const PAGE_ROWS: usize = 32;
const INVENTORY_BYTES: u64 = (profile::PAYLOAD_BYTES + META_BYTES + 32 * 1024) as u64;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tenancy) struct Work {
    pub id: String,
    pub execution: ExecutionId,
    pub tenant: TenantId,
    pub principal: PrincipalId,
    pub expected_generation: u64,
    pub started_at: Timestamp,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "state",
    content = "work",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(in crate::tenancy) enum State {
    Ready,
    RefreshStarted(Work),
    Quarantined(Work),
}
impl State {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::RefreshStarted(_) => "refresh_started",
            Self::Quarantined(_) => "quarantined",
        }
    }
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tenancy) struct Metadata {
    pub format: u32,
    pub installation: InstallationId,
    pub reference: ResourceRef,
    pub slot: String,
    pub profile: Profile,
    pub seed: String,
    pub generation: u64,
    pub state: State,
}
impl Metadata {
    pub fn validate(&self) -> Result<()> {
        self.profile.validate()?;
        ensure!(
            self.format == 1
                && self.reference.resource_kind == ResourceKind::Credential
                && self.slot == slot(&self.installation, &self.reference)?
                && self.seed.len() == 64
                && self.seed.bytes().all(|b| b.is_ascii_hexdigit())
                && self.generation <= i64::MAX as u64,
            "OAuth slot metadata invalid"
        );
        if let State::RefreshStarted(work) | State::Quarantined(work) = &self.state {
            ensure!(
                uuid::Uuid::parse_str(&work.id).is_ok()
                    && work.expected_generation == self.generation,
                "OAuth pending work mismatch"
            );
            ensure!(
                self.reference.owner_kind != OwnerKind::Tenant
                    || self.reference.owner_id.as_str() == work.tenant.as_str(),
                "OAuth foreign work tenant"
            );
        }
        ensure!(
            serde_json::to_vec(self)?.len() <= META_BYTES,
            "OAuth metadata capacity"
        );
        Ok(())
    }
}
/// Stable resource key omits revision/caller/provenance; only explicit pool refs share it.
pub(in crate::tenancy) fn slot(
    installation: &InstallationId,
    reference: &ResourceRef,
) -> Result<String> {
    let key = serde_json::to_vec(&(
        installation,
        reference.owner_kind,
        &reference.owner_id,
        reference.resource_kind,
        &reference.resource_id,
    ))?;
    let mut hash = Sha256::new();
    hash.update(b"bitrouter.owned-oauth-slot.v1");
    hash.update((key.len() as u64).to_be_bytes());
    hash.update(key);
    Ok(hex::encode(hash.finalize()))
}

pub(in crate::tenancy) struct Row {
    pub metadata: Metadata,
    json: String,
    payload: String,
    digest: String,
}
impl Row {
    pub fn new(crypto: &Crypto, metadata: Metadata, token: &OAuthToken) -> Result<Self> {
        metadata.validate()?;
        let json = serde_json::to_string(&metadata)?;
        let payload = crypto.seal(&metadata, token)?;
        let digest = digest(&json, &payload);
        Ok(Self {
            metadata,
            json,
            payload,
            digest,
        })
    }
    pub fn token(&self, crypto: &Crypto) -> Result<OAuthToken> {
        crypto.open(&self.metadata, &self.payload)
    }
    pub fn same(&self, other: &Self) -> bool {
        self.digest == other.digest && self.json == other.json && self.payload == other.payload
    }
}
fn digest(metadata: &str, payload: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"bitrouter.owned-oauth-row.v1");
    for bytes in [metadata.as_bytes(), payload.as_bytes()] {
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    hex::encode(hash.finalize())
}
fn query(installation: &InstallationId) -> SelectStatement {
    Query::select()
        .from(Alias::new(TABLE))
        .and_where(Expr::col(Alias::new("installation_id")).eq(installation.as_str()))
        .to_owned()
}
fn owner(reference: &ResourceRef) -> Result<String> {
    Ok(serde_json::to_value(reference.owner_kind)?
        .as_str()
        .context("OAuth owner kind")?
        .to_owned())
}
async fn rows(
    db: &impl ConnectionTrait,
    mut select: SelectStatement,
    maximum_rows: usize,
) -> Result<Vec<Row>> {
    ensure!(maximum_rows > 0, "OAuth batch bound");
    let maximum_bytes = maximum_rows
        .checked_mul(META_BYTES + profile::PAYLOAD_BYTES)
        .context("OAuth batch byte overflow")?;
    let backend = db.get_database_backend();
    for (name, max) in [
        ("installation_id", 128),
        ("slot_key", 64),
        ("owner_kind", 32),
        ("owner_id", 128),
        ("credential_resource_id", 128),
        ("authority_revision", 128),
        ("state", 32),
        ("metadata_json", META_BYTES as u64),
        ("payload_json", profile::PAYLOAD_BYTES as u64),
        ("content_digest", 64),
    ] {
        let length = if backend == DatabaseBackend::Sqlite {
            Func::cust(Alias::new("length"))
                .arg(Expr::col(Alias::new(name)).cast_as(Alias::new("BLOB")))
        } else {
            Func::cust(Alias::new("octet_length")).arg(Expr::col(Alias::new(name)))
        };
        let mut valid = Expr::expr(length).lte(max);
        if backend == DatabaseBackend::Sqlite {
            valid = valid.and(Expr::cust(format!("typeof({name})='text'")));
        }
        select.expr_as(
            Expr::case(valid, Expr::col(Alias::new(name)))
                .finally(Expr::val(Option::<String>::None)),
            Alias::new(name),
        );
    }
    if backend == DatabaseBackend::Sqlite {
        select.expr_as(
            Expr::cust("CASE WHEN typeof(generation)='integer' THEN generation ELSE NULL END"),
            Alias::new("generation"),
        );
    } else {
        select.column(Alias::new("generation"));
    }
    if backend == DatabaseBackend::Sqlite {
        for name in ["pending_tenant_id", "pending_execution_id"] {
            select.expr_as(Expr::cust(format!(
                "CASE WHEN typeof({name})='text' AND length(CAST({name} AS BLOB)) BETWEEN 1 AND 128 THEN {name} ELSE NULL END"
            )), Alias::new(name));
        }
        select.expr_as(Expr::cust("CASE WHEN (pending_tenant_id IS NULL OR (typeof(pending_tenant_id)='text' AND length(CAST(pending_tenant_id AS BLOB)) BETWEEN 1 AND 128)) AND (pending_execution_id IS NULL OR (typeof(pending_execution_id)='text' AND length(CAST(pending_execution_id AS BLOB)) BETWEEN 1 AND 128)) THEN 1 ELSE 0 END"), Alias::new("pending_valid"));
    }
    let rows = db.query_all(backend.build(&select)).await?;
    ensure!(rows.len() <= maximum_rows, "OAuth batch row bound");
    let mut bytes = 0usize;
    rows.into_iter()
        .map(|row| {
            let value = decode(row, backend == DatabaseBackend::Sqlite)?;
            bytes = bytes
                .checked_add(value.json.len())
                .and_then(|bytes| bytes.checked_add(value.payload.len()))
                .context("OAuth batch byte overflow")?;
            ensure!(bytes <= maximum_bytes, "OAuth batch byte bound");
            Ok(value)
        })
        .collect()
}
fn decode(row: QueryResult, projected: bool) -> Result<Row> {
    let json: String = row.try_get("", "metadata_json")?;
    let payload: String = row.try_get("", "payload_json")?;
    let stored: String = row.try_get("", "content_digest")?;
    let metadata: Metadata = serde_json::from_str(&json)?;
    metadata.validate()?;
    if projected {
        let (tenant, execution) = pending_identity(&metadata);
        ensure!(
            row.try_get::<i64>("", "pending_valid")? == 1
                && row
                    .try_get::<Option<String>>("", "pending_tenant_id")?
                    .as_deref()
                    == tenant
                && row
                    .try_get::<Option<String>>("", "pending_execution_id")?
                    .as_deref()
                    == execution,
            "OAuth pending ownership projection mismatch"
        );
    }
    ensure!(
        stored == digest(&json, &payload)
            && row.try_get::<String>("", "installation_id")? == metadata.installation.as_str()
            && row.try_get::<String>("", "slot_key")? == metadata.slot
            && row.try_get::<String>("", "owner_kind")? == owner(&metadata.reference)?
            && row.try_get::<String>("", "owner_id")? == metadata.reference.owner_id.as_str()
            && row.try_get::<String>("", "credential_resource_id")?
                == metadata.reference.resource_id.as_str()
            && row.try_get::<String>("", "authority_revision")?
                == metadata.reference.revision_id.as_str()
            && row.try_get::<String>("", "state")? == metadata.state.name()
            && row.try_get::<i64>("", "generation")? == i64::try_from(metadata.generation)?,
        "OAuth slot projection mismatch"
    );
    Ok(Row {
        metadata,
        json,
        payload,
        digest: stored,
    })
}

fn pending_identity(metadata: &Metadata) -> (Option<&str>, Option<&str>) {
    match &metadata.state {
        State::Ready => (None, None),
        State::RefreshStarted(work) | State::Quarantined(work) => {
            (Some(work.tenant.as_str()), Some(work.execution.as_str()))
        }
    }
}
pub(in crate::tenancy) async fn load(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    reference: &ResourceRef,
) -> Result<Option<Row>> {
    let row = lookup(db, installation, reference).await?;
    if let Some(row) = &row {
        ensure!(
            row.metadata.reference == *reference,
            "OAuth authority revision conflict"
        );
    }
    Ok(row)
}

/// Read the stable slot, retaining a different revision as a positive conflict.
/// An authority view must distinguish that conflict from a proven empty slot.
pub(in crate::tenancy) async fn lookup(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    reference: &ResourceRef,
) -> Result<Option<Row>> {
    let mut rows = rows(
        db,
        query(installation)
            .and_where(Expr::col(Alias::new("slot_key")).eq(slot(installation, reference)?))
            .limit(2)
            .to_owned(),
        2,
    )
    .await?;
    ensure!(rows.len() <= 1, "duplicate OAuth slot");
    Ok(rows.pop())
}
/// One bounded page for the exclusively owned installation, ordered by stable
/// slot key. Retain the last returned metadata.slot as cursor; an empty page
/// completes the scan. The caller holds a stable read or the exclusive writer.
/// A foreign installation is corruption, not a row to silently skip.
pub(in crate::tenancy) async fn page(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    after_slot: Option<&str>,
) -> Result<Vec<Row>> {
    let mut selected = Query::select().from(Alias::new(TABLE)).to_owned();
    if let Some(after) = after_slot {
        ensure!(
            !after.is_empty() && after.len() <= 64,
            "OAuth page cursor bound"
        );
        selected.and_where(Expr::col(Alias::new("slot_key")).gt(after));
    }
    // Walk the slot primary key. An installation/state index cannot provide
    // this order without sorting the whole catalog; guard aliases must not
    // replace the raw indexed ORDER BY expression either.
    selected
        .order_by((Alias::new(TABLE), Alias::new("slot_key")), Order::Asc)
        .limit(PAGE_ROWS as u64);
    let result = rows(db, selected, PAGE_ROWS).await?;
    let mut previous = after_slot;
    for row in &result {
        ensure!(
            &row.metadata.installation == installation,
            "foreign OAuth page installation"
        );
        ensure!(
            previous.is_none_or(|previous| previous < row.metadata.slot.as_str()),
            "OAuth page key order"
        );
        previous = Some(row.metadata.slot.as_str());
    }
    Ok(result)
}
pub(in crate::tenancy) async fn insert(db: &impl ConnectionTrait, row: &Row) -> Result<()> {
    let backend = db.get_database_backend();
    row.metadata.validate()?;
    let mut insert = Query::insert();
    let mut columns = vec![
        "slot_key",
        "installation_id",
        "owner_kind",
        "owner_id",
        "credential_resource_id",
        "authority_revision",
        "generation",
        "state",
        "metadata_json",
        "payload_json",
        "content_digest",
    ];
    let mut values: Vec<sea_orm::sea_query::SimpleExpr> = vec![
        row.metadata.slot.clone().into(),
        row.metadata.installation.as_str().into(),
        owner(&row.metadata.reference)?.into(),
        row.metadata.reference.owner_id.as_str().into(),
        row.metadata.reference.resource_id.as_str().into(),
        row.metadata.reference.revision_id.as_str().into(),
        i64::try_from(row.metadata.generation)?.into(),
        row.metadata.state.name().into(),
        row.json.clone().into(),
        row.payload.clone().into(),
        row.digest.clone().into(),
    ];
    if backend == DatabaseBackend::Sqlite {
        columns.extend(["pending_tenant_id", "pending_execution_id"]);
        let (tenant, execution) = pending_identity(&row.metadata);
        values.extend([
            tenant.map(str::to_owned).into(),
            execution.map(str::to_owned).into(),
        ]);
    }
    insert
        .into_table(Alias::new(TABLE))
        .columns(columns.into_iter().map(Alias::new))
        .values(values)?;
    if backend != DatabaseBackend::MySql {
        insert.on_conflict(OnConflict::new().do_nothing().to_owned());
    }
    match db.execute(backend.build(&insert)).await {
        Ok(_) => {}
        Err(error)
            if backend == DatabaseBackend::MySql
                && matches!(
                    error.sql_err(),
                    Some(sea_orm::SqlErr::UniqueConstraintViolation(_))
                ) => {}
        Err(error) => return Err(error.into()),
    }
    let existing = load(db, &row.metadata.installation, &row.metadata.reference)
        .await?
        .context("OAuth insert winner missing")?;
    ensure!(existing.same(row), "OAuth insert conflict");
    Ok(())
}
pub(in crate::tenancy) async fn replace(
    db: &impl ConnectionTrait,
    old: &Row,
    new: &Row,
) -> Result<()> {
    ensure!(
        old.metadata.installation == new.metadata.installation
            && old.metadata.reference == new.metadata.reference
            && old.metadata.slot == new.metadata.slot,
        "OAuth update identity conflict"
    );
    new.metadata.validate()?;
    let mut update = Query::update();
    update
        .table(Alias::new(TABLE))
        .values([
            (
                Alias::new("generation"),
                i64::try_from(new.metadata.generation)?.into(),
            ),
            (Alias::new("state"), new.metadata.state.name().into()),
            (Alias::new("metadata_json"), new.json.clone().into()),
            (Alias::new("payload_json"), new.payload.clone().into()),
            (Alias::new("content_digest"), new.digest.clone().into()),
        ])
        .and_where(Expr::col(Alias::new("slot_key")).eq(&old.metadata.slot))
        .and_where(Expr::col(Alias::new("installation_id")).eq(old.metadata.installation.as_str()))
        .and_where(Expr::col(Alias::new("content_digest")).eq(&old.digest));
    if db.get_database_backend() == DatabaseBackend::Sqlite {
        let (tenant, execution) = pending_identity(&new.metadata);
        update.values([
            (
                Alias::new("pending_tenant_id"),
                tenant.map(str::to_owned).into(),
            ),
            (
                Alias::new("pending_execution_id"),
                execution.map(str::to_owned).into(),
            ),
        ]);
    }
    ensure!(
        db.execute(db.get_database_backend().build(&update))
            .await?
            .rows_affected()
            == 1,
        "OAuth update conflict"
    );
    Ok(())
}

/// Reserve the maximum durable footprint of each existing slot, including its
/// future rotated payload. Rotation cannot grow beyond this already charged
/// envelope; no global seed/hash index or token materialization is needed.
pub(in crate::tenancy) async fn inventory(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
) -> Result<u64> {
    let backend = db.get_database_backend();
    let foreign = Query::select()
        .from(Alias::new(TABLE))
        .expr(Expr::val(1))
        .and_where(Expr::col(Alias::new("installation_id")).ne(installation.as_str()))
        .limit(1)
        .to_owned();
    ensure!(
        db.query_one(backend.build(&foreign)).await?.is_none(),
        "foreign OAuth slot installation"
    );
    let count = Query::select()
        .from(Alias::new(TABLE))
        .expr_as(
            Expr::cust(if backend == DatabaseBackend::MySql {
                "CAST(COUNT(*) AS SIGNED)"
            } else {
                "CAST(COUNT(*) AS BIGINT)"
            }),
            Alias::new("slots"),
        )
        .to_owned();
    let count = db
        .query_one(backend.build(&count))
        .await?
        .context("OAuth slot inventory")?
        .try_get::<i64>("", "slots")?;
    let count = u64::try_from(count)?;
    count
        .checked_mul(INVENTORY_BYTES)
        .context("OAuth slot inventory overflow")
}

#[cfg(test)]
mod tests;

/// Test witness for legacy OAuth fault assertions. Production streams canonical
/// obligations and never collects pending work into a whole-directory vector.
#[cfg(test)]
pub(in crate::tenancy) async fn pending_unknown(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
) -> Result<Vec<(TenantId, ExecutionId, chrono::DateTime<chrono::Utc>)>> {
    let mut pending = Vec::new();
    let mut cursor = None::<String>;
    loop {
        let metadata = pending::page(db, installation, cursor.as_deref(), &|_, _| Ok(())).await?;
        if metadata.is_empty() {
            break;
        }
        for metadata in metadata {
            if let State::RefreshStarted(work) | State::Quarantined(work) = &metadata.state
                && !crate::tenancy::receipts::auxiliary_store::pending_receipted(
                    db, &metadata, work,
                )
                .await?
            {
                pending.push((
                    work.tenant.clone(),
                    work.execution.clone(),
                    chrono::DateTime::parse_from_rfc3339(work.started_at.as_str())?
                        .with_timezone(&chrono::Utc),
                ));
            }
            cursor = Some(metadata.slot);
        }
    }
    Ok(pending)
}
