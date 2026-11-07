//! One stopped-owner v1 count-fence transition, never a general resume operation.
use super::{connection::ReadConnection, validation};
use crate::{
    db::migration::m20240101_000044_capacity_upgrade as schema, offline::ValidationBudget,
    tenancy::store::startup,
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::identity::{AuthorityRevision, InstallationId, OperationId};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement, sqlx::SqliteConnection};
use sea_orm_migration::{MigratorTrait, sea_query::SqliteQueryBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MIGRATION: &str = "m20240101_000044_capacity_upgrade";

#[derive(Serialize)]
pub(crate) struct Request {
    pub installation: InstallationId,
    pub revision: AuthorityRevision,
    pub operation: OperationId,
    pub acknowledge_legacy_count_saturation: bool,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Outcome {
    pub format: u32,
    pub installation_id: InstallationId,
    pub operation_id: OperationId,
    pub old_authority_revision: u64,
    pub new_authority_revision: u64,
    pub ordinary_operations: u64,
    pub safety_operations: u64,
    pub installation_created_at: String,
    pub completed_at: String,
    pub from_format: u32,
    pub to_format: u32,
    pub legacy_count_latch_cleared: bool,
}
impl Outcome {
    fn validate(&self, request: &Request) -> Result<()> {
        ensure!(
            self.format == 1
                && self.from_format == 1
                && self.to_format == 2
                && self.legacy_count_latch_cleared
                && self.installation_id == request.installation
                && self.operation_id == request.operation
                && self.old_authority_revision == request.revision.get()
                && self.old_authority_revision.checked_add(1) == Some(self.new_authority_revision)
                && self.ordinary_operations == startup::LEGACY_ORDINARY_OPERATIONS
                && self.safety_operations <= startup::LEGACY_SAFETY_OPERATIONS,
            "invalid capacity upgrade result"
        );
        let _: AuthorityRevision = self.new_authority_revision.try_into()?;
        chrono::DateTime::parse_from_rfc3339(&self.installation_created_at)?;
        chrono::DateTime::parse_from_rfc3339(&self.completed_at)?;
        Ok(())
    }
}

fn digest(request: &Request) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(b"bitrouter.offline.legacy-count-upgrade.v1\0");
    hash.update(serde_json::to_vec(request)?);
    Ok(hex::encode(hash.finalize()))
}

async fn ledger(db: &impl ConnectionTrait) -> Result<bool> {
    // Only the header, audit result and migration ledger are written. Reject
    // trigger/shadow/cascade behavior that could make those writes alter evidence.
    ensure!(db.query_one(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT 1 FROM main.sqlite_schema WHERE type='trigger' AND lower(tbl_name) IN ('tenancy_installation','tenancy_capacity_upgrades','seaql_migrations') UNION ALL SELECT 1 FROM temp.sqlite_schema WHERE lower(name) IN ('tenancy_installation','tenancy_capacity_upgrades','seaql_migrations') OR (type='trigger' AND lower(tbl_name) IN ('tenancy_installation','tenancy_capacity_upgrades','seaql_migrations')) LIMIT 1")).await?.is_none(), "unsupported upgrade write schema");
    ensure!(db.query_one(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT 1 FROM main.sqlite_schema s JOIN pragma_foreign_key_list(s.name) f WHERE s.type='table' AND lower(f.\"table\") IN ('tenancy_installation','tenancy_capacity_upgrades','seaql_migrations') LIMIT 1")).await?.is_none(), "unsupported upgrade write cascade");
    let known: std::collections::BTreeSet<_> = crate::db::migration::Migrator::migrations()
        .iter()
        .map(|m| m.name().to_owned())
        .collect();
    ensure!(
        known.contains(MIGRATION),
        "capacity upgrade migration is not registered"
    );
    let rows = db.query_all(Statement::from_string(DatabaseBackend::Sqlite, format!("SELECT CASE WHEN typeof(version)='text' AND length(CAST(version AS BLOB))<=256 THEN version ELSE NULL END AS version FROM seaql_migrations LIMIT {}", known.len()+1))).await?;
    let mut actual = std::collections::BTreeSet::new();
    for row in rows {
        ensure!(
            actual.insert(row.try_get::<String>("", "version")?),
            "duplicate migration identity"
        );
    }
    let recorded = actual.remove(MIGRATION);
    let mut expected = known;
    expected.remove(MIGRATION);
    ensure!(
        actual == expected,
        "apply the registered prerequisite schema migrations before capacity upgrade"
    );
    Ok(recorded)
}

/// Validate and stage in the caller's exclusive transaction. Never commit here.
/// A saved result wins over current revision/stop checks and performs no writes.
/// `before_write` must check fresh physical capacity under that same owner.
pub(crate) async fn apply_in(
    db: &mut SqliteConnection,
    request: &Request,
    budget: ValidationBudget,
    before_write: impl FnOnce() -> Result<()>,
) -> Result<Outcome> {
    if budget.rows == 0 || budget.bytes < 1024 * 1024 || budget.bytes > isize::MAX as usize {
        return Err(crate::tenancy::store::directory::CapacityExceeded.into());
    }
    let fingerprint = digest(request)?;
    let (loaded, present, recorded) = {
        let reader = ReadConnection::new(db).with_row_limit(budget.rows.max(128));
        let header = startup::read_header_in(&reader, budget.bytes.min(4096))
            .await?
            .context("authority identity missing")?;
        ensure!(
            header.header.installation == request.installation,
            "expected installation differs"
        );
        let present = reader
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT 1 FROM sqlite_schema WHERE name='tenancy_capacity_upgrades' LIMIT 1",
            ))
            .await?
            .is_some();
        if present {
            schema::validate(&reader).await?;
            let saved = reader.query_all(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
                "SELECT CASE WHEN typeof(request_digest)='text' AND length(CAST(request_digest AS BLOB))=64 THEN request_digest ELSE NULL END AS request_digest, CASE WHEN typeof(result_json)='text' AND length(CAST(result_json AS BLOB))<=4096 THEN result_json ELSE NULL END AS result_json FROM tenancy_capacity_upgrades WHERE installation_id=? AND operation_id=? LIMIT 2", [request.installation.as_str().into(), request.operation.as_str().into()])).await?;
            ensure!(saved.len() <= 1, "duplicate capacity upgrade operation");
            if let Some(saved) = saved.first() {
                ensure!(
                    saved.try_get::<String>("", "request_digest")? == fingerprint,
                    "capacity upgrade operation conflict"
                );
                let json: String = saved.try_get("", "result_json")?;
                let outcome: Outcome = serde_json::from_str(&json)?;
                outcome.validate(request)?;
                ensure!(
                    serde_json::to_string(&outcome)? == json
                        && header.format != startup::Format::V1
                        && header.header.revision.get() >= outcome.new_authority_revision
                        && header.header.created_at == outcome.installation_created_at
                        && header.header.operation_ids >= outcome.ordinary_operations
                        && header.header.safety_operation_ids >= outcome.safety_operations,
                    "capacity upgrade result disagrees with authority"
                );
                // Replay performs no writes, but still proves complete current
                // canonical/projection coverage under this exclusive transaction.
                validation::load(&reader, budget).await?;
                return Ok(outcome);
            }
            ensure!(
                reader
                    .query_one(Statement::from_string(
                        DatabaseBackend::Sqlite,
                        "SELECT 1 FROM tenancy_capacity_upgrades LIMIT 1"
                    ))
                    .await?
                    .is_none(),
                "capacity upgrade already recorded under a different operation"
            );
        }
        ensure!(
            request.acknowledge_legacy_count_saturation,
            "explicit legacy count-saturation acknowledgment required"
        );
        ensure!(
            header.format == startup::Format::V1
                && header.header.revision == request.revision
                && header.header.saturated
                && header.header.operation_ids == startup::LEGACY_ORDINARY_OPERATIONS,
            "not the expected legacy count-saturated authority"
        );
        let recorded = ledger(&reader).await?;
        let loaded = validation::load(&reader, budget).await?;
        (loaded, present, recorded)
    };
    let old = &loaded.header;
    let next: AuthorityRevision = old
        .revision
        .get()
        .checked_add(1)
        .context("authority revision exhausted")?
        .try_into()?;
    let outcome = Outcome {
        format: 1,
        installation_id: old.installation.clone(),
        operation_id: request.operation.clone(),
        old_authority_revision: old.revision.get(),
        new_authority_revision: next.get(),
        ordinary_operations: old.operation_ids,
        safety_operations: old.safety_operation_ids,
        installation_created_at: old.created_at.clone(),
        completed_at: chrono::Utc::now().to_rfc3339(),
        from_format: 1,
        to_format: 2,
        legacy_count_latch_cleared: true,
    };
    outcome.validate(request)?;
    let json = serde_json::to_string(&outcome)?;
    ensure!(json.len() <= 4096, "capacity upgrade result bound");
    // Full validation and exact replay precede the fresh exclusive-owner probe.
    // Refusal leaves all header, evidence and operation rows untouched.
    before_write()?;
    if !present {
        sea_orm::sqlx::query(&schema::definition().to_string(SqliteQueryBuilder))
            .execute(&mut *db)
            .await?;
    }
    let changed = sea_orm::sqlx::query("UPDATE tenancy_installation SET schema_version=2,saturated=0,authority_revision=? WHERE singleton=1 AND schema_version=1 AND installation_id=? AND authority_revision=? AND operation_ids=? AND safety_operation_ids=? AND saturated=1 AND created_at=?")
        .bind(i64::try_from(next.get())?).bind(old.installation.as_str()).bind(i64::try_from(old.revision.get())?).bind(i64::try_from(old.operation_ids)?).bind(i64::try_from(old.safety_operation_ids)?).bind(&old.created_at).execute(&mut *db).await?;
    ensure!(
        changed.rows_affected() == 1,
        "authority changed during capacity upgrade"
    );
    sea_orm::sqlx::query("INSERT INTO tenancy_capacity_upgrades (installation_id,operation_id,request_digest,result_json) VALUES (?,?,?,?)").bind(old.installation.as_str()).bind(request.operation.as_str()).bind(fingerprint).bind(json).execute(&mut *db).await?;
    if !recorded {
        sea_orm::sqlx::query("INSERT INTO seaql_migrations(version,applied_at) VALUES (?,?)")
            .bind(MIGRATION)
            .bind(chrono::Utc::now().timestamp())
            .execute(&mut *db)
            .await?;
    }
    Ok(outcome)
}

#[cfg(all(test, unix))]
mod tests;
