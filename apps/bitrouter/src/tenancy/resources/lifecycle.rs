//! Bounded local lifecycle metadata, separate from immutable resource contents.
//! The single authority writer owns transaction, revision and publication fences.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    authority::ResourceKey,
    configuration::ResourceRef,
    identity::{AuthorityRevision, InstallationId, RevisionId},
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, QueryResult, Value,
    sea_query::{Alias, Expr, Func, OnConflict, Query, SelectStatement},
};

const TABLE: &str = "tenancy_resource_lifecycle";
const KEY: [&str; 5] = [
    "owner_kind",
    "owner_id",
    "resource_kind",
    "resource_id",
    "revision_id",
];
/// Nonsecret status for one exact immutable resource revision.
#[derive(Clone, serde::Serialize)]
pub(in crate::tenancy) struct Record {
    pub reference: ResourceRef,
    pub is_head: bool,
    pub revoked: bool,
    pub authority_revision: AuthorityRevision,
}

/// Scoped lifecycle facts; a restricted view requires explicit proven absence.
#[derive(Clone, Default)]
pub(in crate::tenancy) struct State {
    records: HashMap<ResourceRef, Record>,
    absent: HashSet<ResourceRef>,
    scoped: bool,
}

impl State {
    pub(in crate::tenancy) fn head(&self, key: &ResourceKey) -> Option<&RevisionId> {
        self.records.values().find_map(|record| {
            let reference = &record.reference;
            (record.is_head
                && reference.owner_kind == key.owner_kind
                && reference.owner_id == key.owner_id
                && reference.resource_kind == key.resource_kind
                && reference.resource_id == key.resource_id)
                .then_some(&reference.revision_id)
        })
    }

    pub(in crate::tenancy) fn revoked(&self, reference: &ResourceRef) -> bool {
        match self.record(reference) {
            Some(record) => record.revoked,
            None => self.scoped && !self.absent.contains(reference),
        }
    }

    pub(in crate::tenancy) fn record(&self, reference: &ResourceRef) -> Option<&Record> {
        self.records.get(reference)
    }

    /// Insert caller-validated metadata into the bounded live fact closure.
    /// Callers account for the allocation and demote the old head on publication.
    pub(in crate::tenancy) fn insert(&mut self, record: Record) -> Option<Record> {
        self.absent.remove(&record.reference);
        self.records.insert(record.reference.clone(), record)
    }

    pub(in crate::tenancy) fn restrict(&mut self) {
        self.scoped = true;
    }

    pub(in crate::tenancy) fn loaded(&self, reference: &ResourceRef) -> bool {
        self.records.contains_key(reference) || self.absent.contains(reference)
    }

    pub(in crate::tenancy) fn absent(&mut self, reference: ResourceRef) {
        self.records.remove(&reference);
        self.absent.insert(reference);
    }

    pub(in crate::tenancy) fn records(&self) -> impl Iterator<Item = &Record> {
        self.records.values()
    }

    pub(in crate::tenancy) fn absences(&self) -> impl Iterator<Item = &ResourceRef> {
        self.absent.iter()
    }

    #[cfg(test)]
    pub(in crate::tenancy) fn len(&self) -> usize {
        self.records.len()
    }
}

/// Check one compiler-discovered revision under the caller's fixed read custody.
/// Missing legacy lifecycle metadata is allowed only after an exact DB read.
pub(in crate::tenancy) async fn check_in(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    authority: AuthorityRevision,
    reference: &ResourceRef,
) -> std::result::Result<(), bitrouter_ai::tenancy::operation::ErrorCode> {
    use bitrouter_ai::tenancy::operation::ErrorCode;
    if let Some(record) = exact(db, installation, reference)
        .await
        .map_err(|_| ErrorCode::StorageUnavailable)?
    {
        if record.authority_revision > authority {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        if record.revoked {
            return Err(ErrorCode::Forbidden);
        }
    }
    Ok(())
}

fn select(backend: DatabaseBackend) -> SelectStatement {
    let mut query = Query::select();
    query.from(Alias::new(TABLE));
    for (name, limit) in [
        ("installation_id", 128_u64),
        ("owner_kind", 32),
        ("owner_id", 128),
        ("resource_kind", 32),
        ("resource_id", 128),
        ("revision_id", 128),
    ] {
        // Project NULL, never filter corruption away or allocate an unbounded cell.
        // SQLite length(TEXT) counts characters, so measure the BLOB's bytes.
        // https://sqlite.org/lang_corefunc.html#length
        // https://www.postgresql.org/docs/current/functions-string.html
        // https://dev.mysql.com/doc/refman/8.4/en/string-functions.html#function_octet-length
        let column = Expr::col(Alias::new(name));
        let length = if backend == DatabaseBackend::Sqlite {
            Func::cust(Alias::new("length")).arg(column.clone().cast_as(Alias::new("BLOB")))
        } else {
            Func::cust(Alias::new("octet_length")).arg(column.clone())
        };
        let mut valid = Expr::expr(length).lte(limit);
        if backend == DatabaseBackend::Sqlite {
            valid = valid
                .and(Expr::expr(Func::cust(Alias::new("typeof")).arg(column.clone())).eq("text"));
        }
        query.expr_as(
            Expr::case(valid, column).finally(Expr::val(Option::<String>::None)),
            Alias::new(name),
        );
    }
    for name in ["is_head", "revoked", "authority_revision"] {
        let column = Expr::col(Alias::new(name));
        let mut valid = if name == "authority_revision" {
            column.clone().between(0_i64, AuthorityRevision::MAX as i64)
        } else {
            column.clone().is_in([false, true])
        };
        // SQLite's INTEGER/BOOLEAN affinity also accepts arbitrary TEXT/BLOB.
        // https://sqlite.org/datatype3.html
        if backend == DatabaseBackend::Sqlite {
            valid = valid.and(
                Expr::expr(Func::cust(Alias::new("typeof")).arg(column.clone())).eq("integer"),
            );
        }
        query.expr_as(
            Expr::case(valid, column).finally(Expr::cust("NULL")),
            Alias::new(name),
        );
    }
    query
}

fn text(row: &QueryResult, name: &str) -> Result<String> {
    row.try_get::<Option<String>>("", name)?
        .context("invalid or oversized resource lifecycle field")
}

fn decode(row: &QueryResult, installation: &InstallationId) -> Result<Record> {
    ensure!(
        text(row, "installation_id")? == installation.as_str(),
        "resource lifecycle installation mismatch"
    );
    Ok(Record {
        reference: ResourceRef {
            owner_kind: serde_json::from_value(text(row, "owner_kind")?.into())?,
            owner_id: text(row, "owner_id")?.try_into()?,
            resource_kind: serde_json::from_value(text(row, "resource_kind")?.into())?,
            resource_id: text(row, "resource_id")?.try_into()?,
            revision_id: text(row, "revision_id")?.try_into()?,
        },
        is_head: row.try_get("", "is_head")?,
        revoked: row.try_get("", "revoked")?,
        authority_revision: u64::try_from(row.try_get::<i64>("", "authority_revision")?)?
            .try_into()?,
    })
}

fn select_key(backend: DatabaseBackend, key: &ResourceKey) -> Result<SelectStatement> {
    let mut query = select(backend);
    for (name, value) in [
        ("owner_kind", serde_json::to_value(key.owner_kind)?),
        ("resource_kind", serde_json::to_value(key.resource_kind)?),
    ] {
        query.and_where(
            Expr::col(Alias::new(name)).eq(value.as_str().context("invalid lifecycle key")?),
        );
    }
    query
        .and_where(Expr::col(Alias::new("owner_id")).eq(key.owner_id.as_str()))
        .and_where(Expr::col(Alias::new("resource_id")).eq(key.resource_id.as_str()));
    Ok(query)
}

/// Resolve one exact revision. Only a successful absent lookup means unmanaged.
pub(in crate::tenancy) async fn exact(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    reference: &ResourceRef,
) -> Result<Option<Record>> {
    let mut query = select_key(db.get_database_backend(), &reference.key())?;
    query
        .and_where(Expr::col(Alias::new("revision_id")).eq(reference.revision_id.as_str()))
        .limit(1);
    db.query_one(db.get_database_backend().build(&query))
        .await?
        .as_ref()
        .map(|row| decode(row, installation))
        .transpose()
}

/// Resolve the current head without loading a resource's retained revisions.
pub(in crate::tenancy) async fn head(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    key: &ResourceKey,
) -> Result<Option<Record>> {
    let mut query = select_key(db.get_database_backend(), key)?;
    query
        .and_where(Expr::col(Alias::new("is_head")).eq(true))
        .limit(2);
    let rows = db
        .query_all(db.get_database_backend().build(&query))
        .await?;
    ensure!(rows.len() <= 1, "duplicate resource lifecycle head");
    rows.first()
        .map(|row| decode(row, installation))
        .transpose()
}

/// Upsert exactly one revision inside the single writer's caller-owned transaction.
/// Rotation demotes the old head before inserting the new one; revocation supplies
/// the exact revision's existing head flag. Immutable contents are never mutated.
///
/// # Errors
/// Rejects invalid retained metadata, installation mismatch,
/// conflicting heads or database errors. The caller must roll back on any error.
pub(in crate::tenancy) async fn write(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    record: &Record,
) -> Result<()> {
    // Check the exact old owner even when this is not a head update. The table's
    // primary key predates installation qualification; a foreign row is corrupt,
    // never an absent local revision that can be overwritten.
    exact(db, installation, &record.reference).await?;
    let current_head = if record.is_head {
        head(db, installation, &record.reference.key()).await?
    } else {
        None
    };
    ensure!(
        current_head.is_none_or(|head| head.reference == record.reference),
        "duplicate resource lifecycle head"
    );
    let reference = &record.reference;
    let enum_text = |value: serde_json::Value| -> Result<Value> {
        Ok(value
            .as_str()
            .context("invalid resource lifecycle identity")?
            .into())
    };
    let fields: [(&str, Value); 9] = [
        (
            "owner_kind",
            enum_text(serde_json::to_value(reference.owner_kind)?)?,
        ),
        ("owner_id", reference.owner_id.as_str().into()),
        (
            "resource_kind",
            enum_text(serde_json::to_value(reference.resource_kind)?)?,
        ),
        ("resource_id", reference.resource_id.as_str().into()),
        ("revision_id", reference.revision_id.as_str().into()),
        ("installation_id", installation.as_str().into()),
        ("is_head", record.is_head.into()),
        ("revoked", record.revoked.into()),
        (
            "authority_revision",
            i64::try_from(record.authority_revision.get())?.into(),
        ),
    ];
    let mut insert = Query::insert();
    insert
        .into_table(Alias::new(TABLE))
        .columns(fields.iter().map(|(name, _)| Alias::new(*name)))
        .values(fields.into_iter().map(|(_, value)| value.into()))?
        .on_conflict(
            OnConflict::columns(KEY.map(Alias::new))
                .update_columns(["is_head", "revoked", "authority_revision"].map(Alias::new))
                .to_owned(),
        );
    db.execute(db.get_database_backend().build(&insert)).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tenancy::{configuration_store, store::startup};
    use bitrouter_ai::tenancy::authority::{OwnerKind, ResourceKind};
    use sea_orm::{DatabaseConnection, Statement, TransactionTrait};
    use sea_orm_migration::{MigratorTrait, SchemaManager};
    use serde_json::json;

    async fn database() -> Result<DatabaseConnection> {
        let db = crate::db::connect("sqlite::memory:").await?;
        let manager = SchemaManager::new(&db);
        for migration in crate::db::migration::Migrator::migrations() {
            if [
                "m20240101_000028_",
                "m20240101_000029_",
                "m20240101_000039_",
            ]
            .iter()
            .any(|prefix| migration.name().starts_with(prefix))
            {
                migration.up(&manager).await?;
            }
        }
        sql(&db, "INSERT INTO tenancy_installation (singleton,installation_id,schema_version,authority_revision,operation_ids,safety_operation_ids,saturated,created_at) VALUES (1,'installation',2,3,0,0,0,'2026-01-01T00:00:00Z')").await?;
        Ok(db)
    }

    async fn validate(db: &impl ConnectionTrait) -> Result<()> {
        // Exercise the complete production validator with several small pages,
        // retaining only its header rather than rebuilding a directory in tests.
        let loaded = startup::load_header_in(
            db,
            startup::Budget {
                rows: 3,
                bytes: crate::config::tenancy::capacity::AuthorityViews::default().bytes,
            },
        )
        .await?
        .context("missing fixture header")?;
        ensure!(loaded.header.installation.as_str() == "installation");
        Ok(())
    }

    fn record(revision: &str) -> Result<Record> {
        Ok(Record {
            reference: ResourceRef {
                owner_kind: OwnerKind::Tenant,
                owner_id: "tenant-a".try_into()?,
                resource_kind: ResourceKind::Credential,
                resource_id: "resource-a".try_into()?,
                revision_id: revision.try_into()?,
            },
            is_head: true,
            revoked: false,
            authority_revision: 1.try_into()?,
        })
    }

    async fn retain(db: &impl ConnectionTrait, record: &Record) -> Result<()> {
        configuration_store::insert_resource(
            db,
            &configuration_store::StoredResource {
                reference: record.reference.clone(),
                compiler_version: "fixture-v1".into(),
                payload: json!({"ciphertext": "sealed-fixture"}),
                dependencies: json!([]),
                content_digest: "a".repeat(64).try_into()?,
                created_at: "2026-01-01T00:00:00Z".try_into()?,
            },
        )
        .await
    }

    async fn sql(db: &impl ConnectionTrait, sql: &str) -> Result<()> {
        db.execute(Statement::from_string(DatabaseBackend::Sqlite, sql))
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn rotations_and_exact_revocations_reload_and_rollback() -> Result<()> {
        let db = database().await?;
        let installation = "installation".try_into()?;
        let mut old = record("v1")?;
        let mut new = record("v2")?;
        let key = old.reference.key();
        validate(&db).await?;
        ensure!(head(&db, &installation, &key).await?.is_none());
        ensure!(exact(&db, &installation, &old.reference).await?.is_none());
        let transaction = db.begin().await?;
        retain(&transaction, &old).await?;
        write(&transaction, &installation, &old).await?;
        transaction.commit().await?;
        old.is_head = false;
        old.authority_revision = 2.try_into()?;
        new.authority_revision = 2.try_into()?;
        let transaction = db.begin().await?;
        retain(&transaction, &new).await?;
        write(&transaction, &installation, &old).await?;
        write(&transaction, &installation, &new).await?;
        transaction.commit().await?;
        validate(&db).await?;
        ensure!(
            head(&db, &installation, &key)
                .await?
                .context("missing head")?
                .reference
                == new.reference
        );
        for item in [&old, &new] {
            ensure!(
                !exact(&db, &installation, &item.reference)
                    .await?
                    .context("missing revision")?
                    .revoked
            );
        }
        let current = new.reference.clone();
        for target in [&mut old, &mut new] {
            target.revoked = true;
            target.authority_revision = 3.try_into()?;
            let transaction = db.begin().await?;
            write(&transaction, &installation, target).await?;
            transaction.commit().await?;
            validate(&db).await?;
            ensure!(
                exact(&db, &installation, &target.reference)
                    .await?
                    .context("missing revoked revision")?
                    .revoked
            );
            ensure!(
                exact(&db, &installation, &current)
                    .await?
                    .context("missing current revision")?
                    .revoked
                    == (target.reference == current)
            );
            ensure!(
                head(&db, &installation, &key)
                    .await?
                    .context("missing head")?
                    .reference
                    == current
            );
        }
        let saved = exact(&db, &installation, &old.reference)
            .await?
            .context("missing old revision")?;
        ensure!(saved.revoked && saved.authority_revision == 3.try_into()?);
        let transaction = db.begin().await?;
        new.is_head = false;
        write(&transaction, &installation, &new).await?;
        transaction.rollback().await?;
        validate(&db).await?;
        ensure!(
            head(&db, &installation, &key)
                .await?
                .context("missing head after rollback")?
                .reference
                == new.reference
        );
        let immutable = configuration_store::resource(&db, &old.reference)
            .await?
            .context("missing immutable revision")?;
        ensure!(immutable.payload == json!({"ciphertext": "sealed-fixture"}));
        Ok(())
    }

    #[tokio::test]
    async fn namespaces_are_byte_exact_and_revocation_never_crosses_identity() -> Result<()> {
        let db = database().await?;
        let installation = "installation".try_into()?;
        let mut base = record("v1")?;
        base.revoked = true;
        let mut variants = Vec::new();
        for dimension in 0..7 {
            let mut other = record("v1")?;
            match dimension {
                0 => other.reference.owner_kind = OwnerKind::OperatorPool,
                1 => other.reference.owner_id = "tenant-b".try_into()?,
                2 => other.reference.owner_id = "Tenant-a".try_into()?,
                3 => other.reference.resource_kind = ResourceKind::Route,
                4 => other.reference.resource_id = "Resource-a".try_into()?,
                5 => {
                    other.reference.revision_id = "V1".try_into()?;
                    other.is_head = false;
                }
                _ => other.reference.resource_id = "resource-b".try_into()?,
            }
            variants.push(other);
        }
        let transaction = db.begin().await?;
        for item in std::iter::once(&base).chain(&variants) {
            retain(&transaction, item).await?;
            write(&transaction, &installation, item).await?;
        }
        transaction.commit().await?;
        validate(&db).await?;
        ensure!(
            exact(&db, &installation, &base.reference)
                .await?
                .context("missing base")?
                .revoked
        );
        for other in variants {
            ensure!(
                !exact(&db, &installation, &other.reference)
                    .await?
                    .context("missing distinct revision")?
                    .revoked
            );
            if other.is_head {
                ensure!(
                    head(&db, &installation, &other.reference.key())
                        .await?
                        .context("missing distinct head")?
                        .reference
                        == other.reference
                );
            }
        }
        ensure!(
            exact(&db, &"Installation".try_into()?, &base.reference)
                .await
                .is_err()
        );
        ensure!(
            write(&db, &"Installation".try_into()?, &base)
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn missing_exact_revision_duplicate_heads_and_future_authority_are_rejected() -> Result<()>
    {
        let db = database().await?;
        let installation = "installation".try_into()?;
        let first = record("v1")?;
        retain(&db, &first).await?;
        let mut second = record("v2")?;
        second.is_head = false;
        write(&db, &installation, &second).await?;
        let error = validate(&db)
            .await
            .err()
            .context("missing revision accepted")?;
        ensure!(error.to_string() == "resource lifecycle revision missing");
        retain(&db, &second).await?;
        write(&db, &installation, &first).await?;
        sql(&db, "UPDATE tenancy_installation SET authority_revision=0").await?;
        ensure!(validate(&db).await.is_err());
        sql(&db, "UPDATE tenancy_installation SET authority_revision=1").await?;
        validate(&db).await?;
        second.is_head = true;
        ensure!(write(&db, &installation, &second).await.is_err());
        sql(&db, "UPDATE tenancy_resource_lifecycle SET is_head=1").await?;
        let error = validate(&db)
            .await
            .err()
            .context("duplicate heads accepted")?;
        ensure!(error.to_string() == "duplicate indexed authority identity");
        ensure!(
            head(&db, &installation, &first.reference.key())
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn corrupt_cells_are_null_in_sql_before_driver_allocation() -> Result<()> {
        let db = database().await?;
        let installation = "installation".try_into()?;
        let item = record("v1")?;
        retain(&db, &item).await?;
        write(&db, &installation, &item).await?;
        for name in [
            "installation_id",
            "owner_kind",
            "owner_id",
            "resource_kind",
            "resource_id",
            "revision_id",
            "is_head",
            "revoked",
            "authority_revision",
        ] {
            for value in [
                "zeroblob(1048576)",
                "replace(hex(zeroblob(524288)), '0', 'x')",
            ] {
                let transaction = db.begin().await?;
                sql(
                    &transaction,
                    &format!("UPDATE tenancy_resource_lifecycle SET {name}={value}"),
                )
                .await?;
                let row = transaction
                    .query_one(DatabaseBackend::Sqlite.build(&select(DatabaseBackend::Sqlite)))
                    .await?
                    .context("missing corrupt row")?;
                ensure!(row.try_get::<Option<String>>("", name)?.is_none());
                ensure!(validate(&transaction).await.is_err());
                transaction.rollback().await?;
            }
        }
        for assignment in [
            "is_head=2",
            "revoked=-1",
            "authority_revision=-1",
            "authority_revision=9007199254740992",
            "authority_revision=1.5",
            "owner_id='invalid owner'",
            "owner_kind='unknown'",
            "resource_kind='unknown'",
            "owner_id=replace(hex(zeroblob(64)), '0', char(233))",
        ] {
            let transaction = db.begin().await?;
            sql(
                &transaction,
                &format!("UPDATE tenancy_resource_lifecycle SET {assignment}"),
            )
            .await?;
            ensure!(validate(&transaction).await.is_err());
            transaction.rollback().await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn exact_writes_cross_old_history_limit_without_loading_or_evicting_history() -> Result<()>
    {
        let db = database().await?;
        let installation = "installation".try_into()?;
        let mut item = record("v1")?;
        retain(&db, &item).await?;
        write(&db, &installation, &item).await?;
        sql(&db, "WITH RECURSIVE n(i) AS (SELECT 2 UNION ALL SELECT i+1 FROM n WHERE i<4096) INSERT INTO tenancy_resource_lifecycle SELECT 'tenant','tenant-a','credential','resource-a','v'||i,'installation',0,0,1 FROM n").await?;
        sql(&db, "WITH RECURSIVE n(i) AS (SELECT 2 UNION ALL SELECT i+1 FROM n WHERE i<4096) INSERT INTO tenancy_resource_revisions (owner_kind,owner_id,resource_kind,resource_id,revision_id,compiler_version,payload_json,dependency_json,content_digest,created_at) SELECT owner_kind,owner_id,resource_kind,resource_id,'v'||i,compiler_version,payload_json,dependency_json,content_digest,created_at FROM tenancy_resource_revisions CROSS JOIN n WHERE revision_id='v1'").await?;
        item.revoked = true;
        write(&db, &installation, &item).await?;
        let mut added = record("v4097")?;
        added.is_head = false;
        retain(&db, &added).await?;
        write(&db, &installation, &added).await?;
        ensure!(exact(&db, &installation, &added.reference).await?.is_some());
        ensure!(
            exact(&db, &installation, &item.reference)
                .await?
                .is_some_and(|old| old.revoked)
        );
        ensure!(
            head(&db, &installation, &item.reference.key())
                .await?
                .is_some_and(|head| head.reference == item.reference)
        );
        write(&db, &installation, &item).await?;
        validate(&db).await?;
        // Corruption past the first pages must still fail complete validation.
        sql(
            &db,
            "UPDATE tenancy_resource_lifecycle SET authority_revision=4 WHERE revision_id='v4097'",
        )
        .await?;
        ensure!(validate(&db).await.is_err());
        Ok(())
    }
}
