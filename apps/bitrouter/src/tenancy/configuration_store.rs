//! Immutable configuration storage, without authentication or snapshot publication.
//!
//! Callers own transactions and authority checks. Artifact payloads and compiled
//! envelopes must already be encrypted; this module never receives keys or
//! interprets their opaque JSON format. Dependency references are nonsecret.

use std::{collections::HashSet, io::Write};

use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    configuration::{Configuration, ConfigurationRevision, ResourceRef},
    identity::{AuthorityRevision, Digest, PrincipalId, RevisionId, TenantId, Timestamp},
    validation::Validate,
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, QueryResult, SqlErr, Value,
    sea_query::{Alias, Expr, Func, OnConflict, Query, SelectStatement},
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value as JsonValue;

const RESOURCES: &str = "tenancy_resource_revisions";
const CONFIGURATIONS: &str = "tenancy_configurations";
const ACTIVE: &str = "tenancy_active_configurations";
const RESOURCE_KEY: [&str; 5] = [
    "owner_kind",
    "owner_id",
    "resource_kind",
    "resource_id",
    "revision_id",
];

/// Internal storage profile, separate from the public configuration wire limit.
pub(super) const MAX_ENVELOPE_BYTES: usize = 8 * 1024 * 1024;
pub(super) const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;
pub(super) const MAX_DEPENDENCIES: usize = 4096;
const MAX_ACTIVE: usize = 32;
const MAX_JSON_DEPTH: usize = 64;

/// A failed immutable insert or active-pointer compare-and-swap.
#[derive(Debug, thiserror::Error)]
#[error("configuration storage revision conflict")]
pub(super) struct RevisionConflict;

/// One explicitly provisioned, fully qualified encrypted artifact revision.
#[derive(Clone)]
pub(super) struct StoredResource {
    pub reference: ResourceRef,
    pub compiler_version: String,
    /// Opaque encrypted payload, never a plaintext definition or source path.
    pub payload: JsonValue,
    /// JSON array of nonsecret retained qualified references, at most 4096.
    pub dependencies: JsonValue,
    /// Caller-supplied keyed fingerprint; storage cannot verify it without a key.
    pub content_digest: Digest,
    pub created_at: Timestamp,
}

/// The public document and the private encrypted output of its compiler.
#[derive(Clone)]
pub(super) struct StoredConfiguration {
    pub revision: ConfigurationRevision,
    pub compiled: JsonValue,
    /// Full nonsecret closure, including every direct document reference.
    pub dependencies: Vec<ResourceRef>,
    pub creator: PrincipalId,
    pub authority_revision: AuthorityRevision,
    pub created_at: Timestamp,
}

/// Durable pointer metadata; the caller fences it against its authority snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StoredActiveConfiguration {
    pub tenant: TenantId,
    pub revision: RevisionId,
    pub authority_revision: AuthorityRevision,
}

struct BoundedJson {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for BoundedJson {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other(
                "configuration JSON exceeds storage bound",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn json_depth(value: &JsonValue, depth: usize) -> Result<()> {
    ensure!(
        depth <= MAX_JSON_DEPTH,
        "configuration JSON exceeds depth bound"
    );
    match value {
        JsonValue::Object(values) => {
            for value in values.values() {
                json_depth(value, depth + 1)?;
            }
        }
        JsonValue::Array(values) => {
            for value in values {
                json_depth(value, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn sort_json(value: &mut JsonValue) {
    match value {
        JsonValue::Object(values) => {
            values.sort_keys();
            for value in values.values_mut() {
                sort_json(value);
            }
        }
        JsonValue::Array(values) => {
            for value in values {
                sort_json(value);
            }
        }
        _ => {}
    }
}

fn bounded_json(value: &impl Serialize, limit: usize) -> Result<String> {
    let mut writer = BoundedJson {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| anyhow::anyhow!("invalid or oversized configuration JSON"))?;
    String::from_utf8(writer.bytes).context("invalid configuration JSON encoding")
}

fn canonical(value: &JsonValue, limit: usize) -> Result<String> {
    json_depth(value, 0)?;
    // Bound serialization before cloning the tree. Sort object keys only;
    // array order and serde_json number spelling are retained, as in M0.
    let wire = bounded_json(value, limit)?;
    let mut value: JsonValue = serde_json::from_str(&wire)?;
    sort_json(&mut value);
    bounded_json(&value, limit)
}

fn enum_text(value: &impl Serialize) -> Result<String> {
    match serde_json::to_value(value)? {
        JsonValue::String(value) => Ok(value),
        _ => anyhow::bail!("resource identity is not a string enum"),
    }
}

fn text(row: &QueryResult, name: &str) -> Result<String> {
    row.try_get::<Option<String>>("", name)?
        .context("missing or oversized retained configuration field")
}

fn json<T: DeserializeOwned>(row: &QueryResult, name: &str) -> Result<T> {
    serde_json::from_str(&text(row, name)?)
        .map_err(|_| anyhow::anyhow!("invalid retained configuration JSON"))
}

fn authority(row: &QueryResult) -> Result<AuthorityRevision> {
    let value: i64 = row.try_get("", "authority_revision")?;
    Ok(u64::try_from(value)?.try_into()?)
}

fn integer_columns(query: &mut SelectStatement, backend: DatabaseBackend, names: &[&str]) {
    for name in names {
        // SQLite affinity permits arbitrary TEXT/BLOB in INTEGER columns.
        // Do not transfer such cells to the driver before rejecting corruption.
        // https://sqlite.org/datatype3.html
        if backend == DatabaseBackend::Sqlite {
            query.expr_as(
                Expr::case(
                    Expr::expr(Func::cust(Alias::new("typeof")).arg(Expr::col(Alias::new(*name))))
                        .eq("integer"),
                    Expr::col(Alias::new(*name)),
                )
                .finally(Expr::val(Option::<i64>::None)),
                Alias::new(*name),
            );
        } else {
            query.column(Alias::new(*name));
        }
    }
}

fn bounded_columns(
    query: &mut SelectStatement,
    backend: DatabaseBackend,
    columns: &[(&str, usize)],
) -> Result<()> {
    for (name, limit) in columns {
        // Guard projection in SQL, not after allocating an arbitrary TEXT cell.
        // SQLite length(TEXT) counts characters; length(BLOB) counts bytes.
        // https://sqlite.org/lang_corefunc.html#length
        // https://www.postgresql.org/docs/current/functions-string.html
        // https://dev.mysql.com/doc/refman/8.4/en/string-functions.html#function_octet-length
        let length = if backend == DatabaseBackend::Sqlite {
            Func::cust(Alias::new("length"))
                .arg(Expr::col(Alias::new(*name)).cast_as(Alias::new("BLOB")))
        } else {
            Func::cust(Alias::new("octet_length")).arg(Expr::col(Alias::new(*name)))
        };
        query.expr_as(
            Expr::case(
                Expr::expr(length).lte(u64::try_from(*limit)?),
                Expr::col(Alias::new(*name)),
            )
            .finally(Expr::val(Option::<String>::None)),
            Alias::new(*name),
        );
    }
    Ok(())
}

fn resource_key(query: &mut SelectStatement, reference: &ResourceRef) -> Result<()> {
    query
        .and_where(Expr::col(Alias::new("owner_kind")).eq(enum_text(&reference.owner_kind)?))
        .and_where(Expr::col(Alias::new("owner_id")).eq(reference.owner_id.as_str()))
        .and_where(Expr::col(Alias::new("resource_kind")).eq(enum_text(&reference.resource_kind)?))
        .and_where(Expr::col(Alias::new("resource_id")).eq(reference.resource_id.as_str()))
        .and_where(Expr::col(Alias::new("revision_id")).eq(reference.revision_id.as_str()));
    Ok(())
}

async fn resource_row(
    db: &impl ConnectionTrait,
    reference: &ResourceRef,
    lock: bool,
) -> Result<Option<QueryResult>> {
    let mut query = Query::select();
    query.from(Alias::new(RESOURCES)).limit(1);
    bounded_columns(
        &mut query,
        db.get_database_backend(),
        &[
            ("compiler_version", 256),
            ("payload_json", MAX_ENVELOPE_BYTES),
            ("dependency_json", MAX_ENVELOPE_BYTES),
            ("content_digest", 64),
            ("created_at", 64),
        ],
    )?;
    resource_key(&mut query, reference)?;
    if lock && db.get_database_backend() != DatabaseBackend::Sqlite {
        query.lock_exclusive();
    }
    Ok(db
        .query_one(db.get_database_backend().build(&query))
        .await?)
}

/// Read exactly one qualified resource; absence never falls back to another owner.
///
/// # Errors
/// Rejects storage failures, oversized fields and malformed retained metadata.
pub(super) async fn resource(
    db: &impl ConnectionTrait,
    reference: &ResourceRef,
) -> Result<Option<StoredResource>> {
    let Some(row) = resource_row(db, reference, false).await? else {
        return Ok(None);
    };
    let record = StoredResource {
        reference: reference.clone(),
        compiler_version: text(&row, "compiler_version")?,
        payload: json(&row, "payload_json")?,
        dependencies: json(&row, "dependency_json")?,
        content_digest: text(&row, "content_digest")?.try_into()?,
        created_at: text(&row, "created_at")?.try_into()?,
    };
    resource_fields(&record)?;
    Ok(Some(record))
}

fn metadata(compiler: &str, timestamp: &Timestamp) -> Result<()> {
    ensure!(
        !compiler.is_empty() && compiler.chars().count() <= 64,
        "invalid compiler version"
    );
    ensure!(
        timestamp.as_str().len() <= 64,
        "creation timestamp exceeds storage bound"
    );
    Ok(())
}

fn resource_fields(record: &StoredResource) -> Result<Vec<(&'static str, Value)>> {
    metadata(&record.compiler_version, &record.created_at)?;
    let dependency_json = canonical(&record.dependencies, MAX_ENVELOPE_BYTES)?;
    let references: Vec<ResourceRef> = serde_json::from_str(&dependency_json)
        .map_err(|_| anyhow::anyhow!("resource dependencies must be qualified references"))?;
    unique_dependencies(&references)?;
    Ok(vec![
        (
            "owner_kind",
            enum_text(&record.reference.owner_kind)?.into(),
        ),
        ("owner_id", record.reference.owner_id.as_str().into()),
        (
            "resource_kind",
            enum_text(&record.reference.resource_kind)?.into(),
        ),
        ("resource_id", record.reference.resource_id.as_str().into()),
        ("revision_id", record.reference.revision_id.as_str().into()),
        ("compiler_version", record.compiler_version.as_str().into()),
        (
            "payload_json",
            canonical(&record.payload, MAX_ENVELOPE_BYTES)?.into(),
        ),
        ("dependency_json", dependency_json.into()),
        ("content_digest", record.content_digest.as_str().into()),
        ("created_at", record.created_at.as_str().into()),
    ])
}

async fn insert_immutable(
    db: &impl ConnectionTrait,
    table: &str,
    keys: &[&str],
    fields: &[(&str, Value)],
) -> Result<()> {
    let mut insert = Query::insert();
    insert
        .into_table(Alias::new(table))
        .columns(fields.iter().map(|(name, _)| Alias::new(*name)))
        .values(fields.iter().map(|(_, value)| value.clone().into()))?
        // The key is the sole unique constraint. MySQL uses self-assignment,
        // never INSERT IGNORE, so unrelated SQL errors are not suppressed.
        // https://docs.rs/sea-query/0.32.7/sea_query/query/struct.OnConflict.html#method.do_nothing_on
        .on_conflict(
            OnConflict::columns(keys.iter().map(|name| Alias::new(*name)))
                .do_nothing_on(keys.iter().map(|name| Alias::new(*name)))
                .to_owned(),
        );
    db.execute(db.get_database_backend().build(&insert)).await?;
    Ok(())
}

fn same_text(row: &QueryResult, fields: &[(&str, Value)], names: &[&str]) -> Result<()> {
    for name in names {
        let expected = fields
            .iter()
            .find(|(key, _)| key == name)
            .context("missing immutable comparison field")?;
        let actual: Value = text(row, name)?.into();
        if expected.1 != actual {
            return Err(RevisionConflict.into());
        }
    }
    Ok(())
}

/// Insert once, or accept identical canonical payload/dependency bytes and metadata.
/// The original creation timestamp wins on a retry; ciphertext is not replaced.
///
/// # Errors
/// Returns `RevisionConflict` for different content at the same qualified revision,
/// or a validation/storage failure. The caller must roll back on any error.
pub(super) async fn insert_resource(
    db: &impl ConnectionTrait,
    record: &StoredResource,
) -> Result<()> {
    let fields = resource_fields(record)?;
    insert_immutable(db, RESOURCES, &RESOURCE_KEY, &fields).await?;
    // A MySQL repeatable-read snapshot may predate a concurrent winning insert.
    // Use the current locking read after the no-op write.
    // https://dev.mysql.com/doc/refman/8.4/en/innodb-locking-reads.html
    let row = resource_row(db, &record.reference, true)
        .await?
        .context("inserted resource is unavailable")?;
    same_text(
        &row,
        &fields,
        &[
            "compiler_version",
            "payload_json",
            "dependency_json",
            "content_digest",
        ],
    )
}

fn configuration_key(query: &mut SelectStatement, tenant: &TenantId, revision: &RevisionId) {
    query
        .and_where(Expr::col(Alias::new("tenant_id")).eq(tenant.as_str()))
        .and_where(Expr::col(Alias::new("revision_id")).eq(revision.as_str()));
}

async fn configuration_row(
    db: &impl ConnectionTrait,
    tenant: &TenantId,
    revision: &RevisionId,
    lock: bool,
) -> Result<Option<QueryResult>> {
    let mut query = Query::select();
    query.from(Alias::new(CONFIGURATIONS)).limit(1);
    integer_columns(
        &mut query,
        db.get_database_backend(),
        &["schema_version", "authority_revision"],
    );
    bounded_columns(
        &mut query,
        db.get_database_backend(),
        &[
            ("document_json", MAX_DOCUMENT_BYTES),
            ("compiler_version", 256),
            ("content_digest", 64),
            ("compiled_json", MAX_ENVELOPE_BYTES),
            ("dependencies_json", MAX_ENVELOPE_BYTES),
            ("creator_principal_id", 128),
            ("created_at", 64),
        ],
    )?;
    configuration_key(&mut query, tenant, revision);
    if lock && db.get_database_backend() != DatabaseBackend::Sqlite {
        query.lock_exclusive();
    }
    Ok(db
        .query_one(db.get_database_backend().build(&query))
        .await?)
}

fn unique_dependencies(closure: &[ResourceRef]) -> Result<HashSet<&ResourceRef>> {
    ensure!(
        closure.len() <= MAX_DEPENDENCIES,
        "configuration dependency bound exceeded"
    );
    let unique: HashSet<_> = closure.iter().collect();
    ensure!(
        unique.len() == closure.len(),
        "duplicate configuration dependency"
    );
    Ok(unique)
}

fn dependencies(document: &Configuration, closure: &[ResourceRef]) -> Result<()> {
    let unique = unique_dependencies(closure)?;
    ensure!(
        document
            .routes
            .iter()
            .chain(document.policy.iter())
            .all(|reference| unique.contains(reference)),
        "configuration dependency closure omits a document reference"
    );
    Ok(())
}

/// Read a tenant's exact retained configuration and its creation authority fence.
///
/// # Errors
/// Rejects malformed/oversized data; authorization and publication checks are the
/// caller's responsibility, including checking every returned dependency.
pub(super) async fn configuration(
    db: &impl ConnectionTrait,
    tenant: &TenantId,
    revision: &RevisionId,
) -> Result<Option<StoredConfiguration>> {
    let Some(row) = configuration_row(db, tenant, revision, false).await? else {
        return Ok(None);
    };
    let result = StoredConfiguration {
        revision: ConfigurationRevision {
            tenant_id: tenant.clone(),
            revision_id: revision.clone(),
            schema_version: u32::try_from(row.try_get::<i32>("", "schema_version")?)?,
            document: json(&row, "document_json")?,
            compiler_version: text(&row, "compiler_version")?,
            content_digest: text(&row, "content_digest")?.try_into()?,
        },
        compiled: json(&row, "compiled_json")?,
        dependencies: json(&row, "dependencies_json")?,
        creator: text(&row, "creator_principal_id")?.try_into()?,
        authority_revision: authority(&row)?,
        created_at: text(&row, "created_at")?.try_into()?,
    };
    configuration_fields(&result)?;
    Ok(Some(result))
}

fn configuration_fields(record: &StoredConfiguration) -> Result<Vec<(&'static str, Value)>> {
    record.revision.validate()?;
    metadata(&record.revision.compiler_version, &record.created_at)?;
    dependencies(&record.revision.document, &record.dependencies)?;
    Ok(vec![
        ("tenant_id", record.revision.tenant_id.as_str().into()),
        ("revision_id", record.revision.revision_id.as_str().into()),
        (
            "schema_version",
            i32::try_from(record.revision.schema_version)?.into(),
        ),
        (
            "document_json",
            canonical(
                &serde_json::to_value(&record.revision.document)?,
                MAX_DOCUMENT_BYTES,
            )?
            .into(),
        ),
        (
            "compiler_version",
            record.revision.compiler_version.as_str().into(),
        ),
        (
            "content_digest",
            record.revision.content_digest.as_str().into(),
        ),
        (
            "compiled_json",
            canonical(&record.compiled, MAX_ENVELOPE_BYTES)?.into(),
        ),
        (
            "dependencies_json",
            canonical(
                &serde_json::to_value(&record.dependencies)?,
                MAX_ENVELOPE_BYTES,
            )?
            .into(),
        ),
        ("creator_principal_id", record.creator.as_str().into()),
        (
            "authority_revision",
            i64::try_from(record.authority_revision.get())?.into(),
        ),
        ("created_at", record.created_at.as_str().into()),
    ])
}

/// Persist an immutable configuration within the caller's command transaction.
///
/// # Errors
/// A retry must match content, creator, creation authority and timestamp exactly;
/// otherwise returns `RevisionConflict`. Bounds and storage failures also fail.
pub(super) async fn insert_configuration(
    db: &impl ConnectionTrait,
    record: &StoredConfiguration,
) -> Result<()> {
    let fields = configuration_fields(record)?;
    insert_immutable(db, CONFIGURATIONS, &["tenant_id", "revision_id"], &fields).await?;
    let row = configuration_row(
        db,
        &record.revision.tenant_id,
        &record.revision.revision_id,
        true,
    )
    .await?
    .context("inserted configuration is unavailable")?;
    same_text(
        &row,
        &fields,
        &[
            "document_json",
            "compiler_version",
            "content_digest",
            "compiled_json",
            "dependencies_json",
            "creator_principal_id",
            "created_at",
        ],
    )?;
    if authority(&row)? != record.authority_revision
        || row.try_get::<i32>("", "schema_version")?
            != i32::try_from(record.revision.schema_version)?
    {
        return Err(RevisionConflict.into());
    }
    Ok(())
}

async fn creation_revision(
    db: &impl ConnectionTrait,
    tenant: &TenantId,
    revision: &RevisionId,
) -> Result<Option<AuthorityRevision>> {
    let mut query = Query::select();
    query.from(Alias::new(CONFIGURATIONS)).limit(1);
    integer_columns(
        &mut query,
        db.get_database_backend(),
        &["authority_revision"],
    );
    configuration_key(&mut query, tenant, revision);
    db.query_one(db.get_database_backend().build(&query))
        .await?
        .as_ref()
        .map(authority)
        .transpose()
}

/// CAS a tenant's active pointer to an existing same-tenant immutable revision.
/// Call inside the authority/journal transaction; no commit or publication occurs here.
///
/// # Errors
/// Rejects stale pointers, non-increasing authority revisions, missing targets,
/// or storage errors. Creation requires absence; replay belongs to the journal.
/// An ordinary `RevisionConflict` leaves the caller's transaction usable.
pub(super) async fn set_active(
    db: &impl ConnectionTrait,
    tenant: &TenantId,
    expected: Option<&RevisionId>,
    revision: &RevisionId,
    authority_revision: AuthorityRevision,
) -> Result<()> {
    let created = creation_revision(db, tenant, revision)
        .await?
        .context("active configuration target is unavailable")?;
    ensure!(
        created <= authority_revision,
        "activation predates configuration creation"
    );
    let next = i64::try_from(authority_revision.get())?;
    if let Some(expected) = expected {
        let query = Query::update()
            .table(Alias::new(ACTIVE))
            .values([
                (Alias::new("revision_id"), revision.as_str().into()),
                (Alias::new("authority_revision"), next.into()),
            ])
            .and_where(Expr::col(Alias::new("tenant_id")).eq(tenant.as_str()))
            .and_where(Expr::col(Alias::new("revision_id")).eq(expected.as_str()))
            .and_where(Expr::col(Alias::new("authority_revision")).lt(next))
            .to_owned();
        if db
            .execute(db.get_database_backend().build(&query))
            .await?
            .rows_affected()
            != 1
        {
            return Err(RevisionConflict.into());
        }
    } else {
        let backend = db.get_database_backend();
        let mut query = Query::insert();
        query
            .into_table(Alias::new(ACTIVE))
            .columns(["tenant_id", "revision_id", "authority_revision"].map(Alias::new))
            .values([
                tenant.as_str().into(),
                revision.as_str().into(),
                next.into(),
            ])?;
        if backend != DatabaseBackend::MySql {
            // PostgreSQL aborts a transaction after a unique-constraint error.
            // Handle only the expected tenant-PK collision inside the statement.
            query.on_conflict(
                OnConflict::column(Alias::new("tenant_id"))
                    .do_nothing()
                    .to_owned(),
            );
        }
        match db.execute(backend.build(&query)).await {
            Ok(result) if result.rows_affected() == 1 => {}
            Ok(_) => return Err(RevisionConflict.into()),
            // InnoDB rolls back only the duplicate-key statement. Retain the
            // plain INSERT: SQLx enables FOUND_ROWS, so a no-op MySQL upsert
            // could report one affected row and falsely pass this absence CAS.
            // https://dev.mysql.com/doc/refman/8.4/en/innodb-error-handling.html
            // https://dev.mysql.com/doc/refman/8.4/en/insert-on-duplicate.html
            Err(error)
                if backend == DatabaseBackend::MySql
                    && matches!(error.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))) =>
            {
                return Err(RevisionConflict.into());
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// Load at most 32 active pointers; an oversized store fails instead of truncating.
/// The caller must compare each authority revision with its loaded authority snapshot.
///
/// # Errors
/// Rejects oversized stores, malformed metadata, missing targets or pointers that
/// predate their target's creation. Read within the caller's startup transaction.
pub(super) async fn active_revisions(
    db: &impl ConnectionTrait,
) -> Result<Vec<StoredActiveConfiguration>> {
    let mut query = Query::select();
    query
        .from(Alias::new(ACTIVE))
        .order_by(Alias::new("tenant_id"), sea_orm::sea_query::Order::Asc)
        .limit(u64::try_from(MAX_ACTIVE + 1)?);
    integer_columns(
        &mut query,
        db.get_database_backend(),
        &["authority_revision"],
    );
    bounded_columns(
        &mut query,
        db.get_database_backend(),
        &[("tenant_id", 128), ("revision_id", 128)],
    )?;
    let rows = db
        .query_all(db.get_database_backend().build(&query))
        .await?;
    ensure!(
        rows.len() <= MAX_ACTIVE,
        "active configuration capacity exceeded"
    );
    let mut result = Vec::with_capacity(rows.len());
    for row in rows {
        let record = StoredActiveConfiguration {
            tenant: text(&row, "tenant_id")?.try_into()?,
            revision: text(&row, "revision_id")?.try_into()?,
            authority_revision: authority(&row)?,
        };
        let created = creation_revision(db, &record.tenant, &record.revision)
            .await?
            .context("active configuration target is unavailable")?;
        ensure!(
            created <= record.authority_revision,
            "active pointer predates configuration creation"
        );
        result.push(record);
    }
    Ok(result)
}

#[cfg(test)]
#[path = "configuration_store/tests.rs"]
mod tests;
