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
    sea_query::{Alias, Expr, Func, OnConflict, Query, SelectStatement},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(in crate::tenancy) const TABLE: &str = "tenancy_oauth_slots";
const META_BYTES: usize = 8192;

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
async fn rows(db: &impl ConnectionTrait, mut select: SelectStatement) -> Result<Vec<Row>> {
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
        select.expr_as(
            Expr::case(Expr::expr(length).lte(max), Expr::col(Alias::new(name)))
                .finally(Expr::val(Option::<String>::None)),
            Alias::new(name),
        );
    }
    select.column(Alias::new("generation"));
    let rows = db.query_all(backend.build(&select)).await?;
    ensure!(rows.len() <= profile::MAX_SLOTS, "OAuth slot capacity");
    let mut bytes = 0usize;
    rows.into_iter()
        .map(|row| {
            let value = decode(row)?;
            bytes = bytes
                .checked_add(value.json.len())
                .context("OAuth metadata overflow")?;
            ensure!(bytes <= profile::METADATA_BYTES, "OAuth metadata capacity");
            Ok(value)
        })
        .collect()
}
fn decode(row: QueryResult) -> Result<Row> {
    let json: String = row.try_get("", "metadata_json")?;
    let payload: String = row.try_get("", "payload_json")?;
    let stored: String = row.try_get("", "content_digest")?;
    let metadata: Metadata = serde_json::from_str(&json)?;
    metadata.validate()?;
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
pub(in crate::tenancy) async fn load(
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
    )
    .await?;
    ensure!(rows.len() <= 1, "duplicate OAuth slot");
    let row = rows.pop();
    if let Some(row) = &row {
        ensure!(
            row.metadata.reference == *reference,
            "OAuth authority revision conflict"
        );
    }
    Ok(row)
}
pub(in crate::tenancy) async fn all(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
) -> Result<Vec<Row>> {
    rows(
        db,
        query(installation)
            .limit((profile::MAX_SLOTS + 1) as u64)
            .to_owned(),
    )
    .await
}
pub(in crate::tenancy) async fn insert(db: &impl ConnectionTrait, row: &Row) -> Result<()> {
    let backend = db.get_database_backend();
    let mut insert = Query::insert();
    insert
        .into_table(Alias::new(TABLE))
        .columns(
            [
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
            ]
            .map(Alias::new),
        )
        .values([
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
        ])?;
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
        old.metadata.reference == new.metadata.reference && old.metadata.slot == new.metadata.slot,
        "OAuth update identity conflict"
    );
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
    ensure!(count <= profile::MAX_SLOTS as u64, "OAuth slot capacity");
    count
        .checked_mul((profile::PAYLOAD_BYTES + META_BYTES + 32 * 1024) as u64)
        .context("OAuth slot inventory overflow")
}

#[cfg(test)]
mod tests;

/// A crash can leave a durable start without a terminal auxiliary receipt. It
/// still consumes the same existing unknown-effect budget on restart. Once an
/// exact auxiliary receipt exists, its validated disposition is authoritative;
/// do not count a known rejection as unknown or count the execution twice.
pub(in crate::tenancy) async fn pending_unknown(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
) -> Result<Vec<(TenantId, ExecutionId, chrono::DateTime<chrono::Utc>)>> {
    let selected = query(installation)
        .and_where(Expr::col(Alias::new("state")).ne("ready"))
        .limit((profile::MAX_SLOTS + 1) as u64)
        .to_owned();
    let rows = rows(db, selected).await?;
    let mut pending = Vec::new();
    for row in rows {
        let work = match &row.metadata.state {
            State::RefreshStarted(work) | State::Quarantined(work) => work,
            State::Ready => anyhow::bail!("OAuth pending projection mismatch"),
        };
        if !crate::tenancy::receipts::auxiliary_store::pending_receipted(db, &row.metadata, work)
            .await?
        {
            pending.push((
                work.tenant.clone(),
                work.execution.clone(),
                chrono::DateTime::parse_from_rfc3339(work.started_at.as_str())?
                    .with_timezone(&chrono::Utc),
            ));
        }
    }
    Ok(pending)
}
