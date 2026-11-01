//! Exercise the generated guarded projection through the actual typed adapter.
use super::*;
use crate::tenancy::{
    model::{AuthorityHeader, ReadContext, Snapshot},
    store::{self, Operation, StoredResponse, directory::CapacityExceeded, startup},
};
use bitrouter_ai::tenancy::{
    identity::PrincipalId,
    operation::{OperationResult, OperationStatus},
};
use chrono::Utc;
use sea_orm::TransactionTrait;
use sea_orm_migration::{MigratorTrait, SchemaManager};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counted<'a> {
    reader: ReadConnection<'a>,
    bodies: AtomicUsize,
}
impl Counted<'_> {
    fn observe(&self, statement: &Statement) {
        if statement.sql.contains(" FROM \"tenancy_operations\"")
            && statement.sql.contains(" AS \"response_json\"")
        {
            self.bodies.fetch_add(1, Ordering::Relaxed);
        }
    }
}
#[async_trait::async_trait]
impl ConnectionTrait for Counted<'_> {
    fn get_database_backend(&self) -> DatabaseBackend {
        DatabaseBackend::Sqlite
    }
    async fn execute(&self, statement: Statement) -> std::result::Result<ExecResult, DbErr> {
        self.reader.execute(statement).await
    }
    async fn execute_unprepared(&self, sql: &str) -> std::result::Result<ExecResult, DbErr> {
        self.reader.execute_unprepared(sql).await
    }
    async fn query_one(
        &self,
        statement: Statement,
    ) -> std::result::Result<Option<QueryResult>, DbErr> {
        self.observe(&statement);
        self.reader.query_one(statement).await
    }
    async fn query_all(
        &self,
        statement: Statement,
    ) -> std::result::Result<Vec<QueryResult>, DbErr> {
        self.observe(&statement);
        self.reader.query_all(statement).await
    }
}

async fn seed(url: &str) -> Result<()> {
    let db = crate::db::connect(url).await?;
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
    let mut state = Snapshot::from_header(&AuthorityHeader {
        installation: "typed-scan".try_into()?,
        revision: 10_u64.try_into()?,
        operation_ids: 0,
        safety_operation_ids: 0,
        saturated: false,
        created_at: "2026-01-01T00:00:00Z".into(),
    });
    db.execute(Statement::from_string(DatabaseBackend::Sqlite,
        "INSERT INTO tenancy_installation(singleton,installation_id,schema_version,authority_revision,operation_ids,safety_operation_ids,saturated,created_at) VALUES(1,'typed-scan',2,10,0,0,0,'2026-01-01T00:00:00Z')"
    )).await?;
    for index in 0..3 {
        state.operation_ids += 1;
        let id = format!("op-{index}").try_into()?;
        let result = OperationResult {
            operation_id: id,
            status: OperationStatus::Applied,
            authority_revision: Some(state.revision),
            resource_revision: Some("r1".try_into()?),
            resource_id: Some("fixture".try_into()?),
            error: None,
        };
        let operation = Operation {
            principal: PrincipalId::for_control_credential("fixture")?,
            id: result.operation_id.clone(),
            context: ReadContext::Installation,
            digest: "a".repeat(64),
            result: result.clone(),
            response: StoredResponse::Operation(result),
            created_at: Utc::now(),
            safety: false,
        };
        let tx = db.begin().await?;
        store::write_in(&tx, &state, &[], &operation).await?;
        tx.commit().await?;
    }
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn full_guarded_projection_reads_each_body_once_and_preflights_before_decode() -> Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("operations.sqlite");
    let url = format!("sqlite:{}?mode=rwc", path.display());
    seed(&url).await?;
    let mut db = SqliteConnection::connect(&url).await?;
    let reader = Counted {
        reader: ReadConnection::new(&mut db).with_row_limit(2),
        bodies: AtomicUsize::new(0),
    };
    let budget = startup::Budget {
        rows: 2,
        bytes: 1024 * 1024,
    };
    let loaded = startup::load_header_in(&reader, budget)
        .await?
        .context("header missing")?;
    assert_eq!(loaded.header.operation_ids, 3);
    assert_eq!(reader.bodies.load(Ordering::Relaxed), 3);
    drop(reader);

    // The body fits the encoded row guard, but its JSON tree exceeds the
    // decode allowance. Its invalid DTO shape must not win over preflight.
    let dense = serde_json::to_string(&vec![serde_json::Value::Null; 1024])?;
    sea_orm::sqlx::query("UPDATE tenancy_operations SET response_json=? WHERE operation_id='op-2'")
        .bind(dense)
        .execute(&mut db)
        .await?;
    let reader = Counted {
        reader: ReadConnection::new(&mut db).with_row_limit(2),
        bodies: AtomicUsize::new(0),
    };
    let error = startup::load_header_in(
        &reader,
        startup::Budget {
            rows: 2,
            bytes: 128 * 1024,
        },
    )
    .await
    .err()
    .context("dense body accepted")?;
    assert!(error.is::<CapacityExceeded>(), "{error:#}");
    assert_eq!(reader.bodies.load(Ordering::Relaxed), 3);
    // With enough decode allowance it reaches canonical validation and fails
    // as corruption, proving that preflight has not replaced the decoder.
    let error = startup::load_header_in(&reader, budget)
        .await
        .err()
        .context("invalid DTO accepted")?;
    assert!(!error.is::<CapacityExceeded>(), "{error:#}");
    assert_eq!(reader.bodies.load(Ordering::Relaxed), 6);
    drop(reader);
    db.close().await?;
    Ok(())
}
