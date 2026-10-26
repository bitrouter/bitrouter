use super::*;
use anyhow::{Context, Result, ensure};
use sea_orm::{Database, DatabaseConnection, TransactionTrait};
mod names;
mod server;

// Schema/storage roundtrip only: cryptographic payload authentication is tested
// by the real tenancy consumer. These opaque fixture bytes deliberately remain
// uninterpreted by the migration and survive commit/reconnect byte-for-byte.
const SLOT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const PAYLOAD: &str = r#"{"fixture":"opaque ciphertext bytes\u0000"}"#;
async fn insert_slot(db: &impl ConnectionTrait) -> Result<()> {
    use sea_orm::sea_query::{Alias, Query};
    let fields: Vec<(&str, sea_orm::Value)> = vec![
        ("slot_key", SLOT.into()),
        ("installation_id", "owned-installation".into()),
        ("owner_kind", "tenant".into()),
        ("owner_id", "tenant".into()),
        ("credential_resource_id", "slot".into()),
        ("authority_revision", "r1".into()),
        ("generation", 0_i64.into()),
        ("state", "ready".into()),
        ("metadata_json", r#"{"fixture":"opaque metadata"}"#.into()),
        ("payload_json", PAYLOAD.into()),
        ("content_digest", SLOT.into()),
    ];
    let mut insert = Query::insert();
    insert
        .into_table(Alias::new("tenancy_oauth_slots"))
        .columns(fields.iter().map(|(name, _)| Alias::new(*name)))
        .values(fields.into_iter().map(|(_, value)| value.into()))?;
    db.execute(db.get_database_backend().build(&insert)).await?;
    Ok(())
}
async fn stored_payload(db: &impl ConnectionTrait) -> Result<Option<String>> {
    use sea_orm::sea_query::{Alias, Expr, Query};
    let query = Query::select()
        .column(Alias::new("payload_json"))
        .from(Alias::new("tenancy_oauth_slots"))
        .and_where(Expr::col(Alias::new("slot_key")).eq(SLOT))
        .to_owned();
    db.query_one(db.get_database_backend().build(&query))
        .await?
        .map(|row| row.try_get("", "payload_json").map_err(Into::into))
        .transpose()
}
async fn retain(db: &DatabaseConnection) -> Result<()> {
    let manager = SchemaManager::new(db);
    Migration.up(&manager).await?;
    Migration.up(&manager).await?;
    let transaction = db.begin().await?;
    let written = insert_slot(&transaction).await;
    transaction.rollback().await?;
    written?;
    ensure!(stored_payload(db).await?.is_none());
    let transaction = db.begin().await?;
    match insert_slot(&transaction).await {
        Ok(()) => transaction.commit().await?,
        Err(error) => {
            transaction.rollback().await?;
            return Err(error);
        }
    }
    ensure!(stored_payload(db).await?.as_deref() == Some(PAYLOAD));
    Ok(())
}
async fn verify_and_drop(db: &DatabaseConnection) -> Result<()> {
    let manager = SchemaManager::new(db);
    ensure!(stored_payload(db).await?.as_deref() == Some(PAYLOAD));
    Migration.up(&manager).await?;
    ensure!(Migration.down(&manager).await.is_err());
    ensure!(manager.has_table("tenancy_receipt_auxiliary").await?);
    ensure!(stored_payload(db).await?.as_deref() == Some(PAYLOAD));
    db.execute_unprepared("DELETE FROM tenancy_oauth_slots")
        .await?;
    Migration.down(&manager).await?;
    Migration.down(&manager).await?;
    Ok(())
}
async fn roundtrip(db: &DatabaseConnection) -> Result<()> {
    retain(db).await?;
    verify_and_drop(db).await
}
#[tokio::test]
async fn sqlite_additive_schema_commit_rollback_replay_and_down_preflight() -> Result<()> {
    let db = Database::connect("sqlite::memory:").await?;
    roundtrip(&db).await?;
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn incompatible_last_table_is_rejected_before_any_first_table_ddl() -> Result<()> {
    let db = Database::connect("sqlite::memory:").await?;
    db.execute_unprepared("CREATE TABLE tenancy_receipt_auxiliary (arbitrary TEXT)")
        .await?;
    db.execute_unprepared("INSERT INTO tenancy_receipt_auxiliary VALUES ('historical bytes')")
        .await?;
    let manager = SchemaManager::new(&db);
    ensure!(Migration.up(&manager).await.is_err());
    ensure!(!manager.has_table("tenancy_oauth_slots").await?);
    let value = db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT arbitrary FROM tenancy_receipt_auxiliary".to_owned(),
        ))
        .await?
        .context("historical row")?;
    ensure!(value.try_get::<String>("", "arbitrary")? == "historical bytes");
    db.close().await?;
    Ok(())
}
#[tokio::test]
async fn empty_partial_schema_reruns_and_down_refuses_external_dependency_before_any_drop()
-> Result<()> {
    let db = Database::connect("sqlite::memory:").await?;
    let manager = SchemaManager::new(&db);
    manager
        .create_table(definition(&SPECS[0], DatabaseBackend::Sqlite))
        .await?;
    Migration.up(&manager).await?;
    db.execute_unprepared("CREATE VIEW last_owned_view AS SELECT * FROM tenancy_receipt_auxiliary")
        .await?;
    ensure!(Migration.down(&manager).await.is_err());
    for spec in SPECS {
        ensure!(manager.has_table(spec.table).await?);
    }
    db.execute_unprepared("DROP VIEW last_owned_view").await?;
    Migration.down(&manager).await?;
    db.close().await?;
    Ok(())
}

async fn legacy_definitions(db: &DatabaseConnection) -> Result<Vec<(String, String)>> {
    let sql = match db.get_database_backend() {
        DatabaseBackend::Sqlite => {
            "SELECT name,sql AS definition FROM sqlite_schema WHERE name IN ('legacy_oauth_view','legacy_oauth_trigger') ORDER BY name"
        }
        DatabaseBackend::Postgres => {
            "SELECT c.relname AS name,pg_get_viewdef(c.oid,false) AS definition FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=current_schema() AND c.relname='legacy_oauth_view' UNION ALL SELECT g.tgname AS name,pg_get_triggerdef(g.oid,false) AS definition FROM pg_trigger g JOIN pg_class c ON c.oid=g.tgrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=current_schema() AND g.tgname='legacy_oauth_trigger' UNION ALL SELECT p.proname AS name,pg_get_functiondef(p.oid) AS definition FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname=current_schema() AND p.proname='legacy_oauth_audit' ORDER BY name"
        }
        DatabaseBackend::MySql => {
            "SELECT TABLE_NAME AS name,VIEW_DEFINITION AS definition FROM information_schema.views WHERE TABLE_SCHEMA=DATABASE() AND TABLE_NAME='legacy_oauth_view' UNION ALL SELECT TRIGGER_NAME AS name,ACTION_STATEMENT AS definition FROM information_schema.triggers WHERE TRIGGER_SCHEMA=DATABASE() AND TRIGGER_NAME='legacy_oauth_trigger' ORDER BY name"
        }
    };
    db.query_all(Statement::from_string(
        db.get_database_backend(),
        sql.to_owned(),
    ))
    .await?
    .into_iter()
    .map(|row| Ok((row.try_get("", "name")?, row.try_get("", "definition")?)))
    .collect()
}
async fn legacy_rows(db: &DatabaseConnection, table: &str) -> Result<Vec<(i32, String)>> {
    ensure!(matches!(table, "legacy_oauth_data" | "legacy_oauth_log"));
    db.query_all(Statement::from_string(
        db.get_database_backend(),
        format!("SELECT id,payload FROM {table} ORDER BY id"),
    ))
    .await?
    .into_iter()
    .map(|row| Ok((row.try_get("", "id")?, row.try_get("", "payload")?)))
    .collect()
}

// The backend fixture owns its whole database. These Legacy objects deliberately
// have no037 dependency; additive up must preserve their exact catalog text and
// rows while down may conservatively refuse their unresolved executable bodies.
async fn unrelated_legacy_objects_survive_additive_upgrade(db: &DatabaseConnection) -> Result<()> {
    let manager = SchemaManager::new(db);
    for partial in [false, true] {
        db.execute_unprepared(
            "CREATE TABLE legacy_oauth_data (id INTEGER PRIMARY KEY,payload TEXT NOT NULL)",
        )
        .await?;
        db.execute_unprepared(
            "CREATE TABLE legacy_oauth_log (id INTEGER PRIMARY KEY,payload TEXT NOT NULL)",
        )
        .await?;
        db.execute_unprepared(
            "CREATE VIEW legacy_oauth_view AS SELECT id,payload FROM legacy_oauth_data",
        )
        .await?;
        match db.get_database_backend() {
            DatabaseBackend::Sqlite => {
                db.execute_unprepared("CREATE TRIGGER legacy_oauth_trigger AFTER INSERT ON legacy_oauth_data BEGIN INSERT INTO legacy_oauth_log VALUES (NEW.id,NEW.payload); END").await?;
            }
            DatabaseBackend::Postgres => {
                db.execute_unprepared("CREATE FUNCTION legacy_oauth_audit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO legacy_oauth_log VALUES (NEW.id,NEW.payload); RETURN NEW; END $$").await?;
                db.execute_unprepared("CREATE TRIGGER legacy_oauth_trigger AFTER INSERT ON legacy_oauth_data FOR EACH ROW EXECUTE FUNCTION legacy_oauth_audit()").await?;
            }
            DatabaseBackend::MySql => {
                db.execute_unprepared("CREATE TRIGGER legacy_oauth_trigger AFTER INSERT ON legacy_oauth_data FOR EACH ROW INSERT INTO legacy_oauth_log VALUES (NEW.id,NEW.payload)").await?;
            }
        }
        db.execute_unprepared("INSERT INTO legacy_oauth_data VALUES (1,'unchanged Legacy bytes')")
            .await?;
        let definitions = legacy_definitions(db).await?;
        ensure!(
            definitions.len()
                == if db.get_database_backend() == DatabaseBackend::Postgres {
                    3
                } else {
                    2
                }
        );
        let before = legacy_rows(db, "legacy_oauth_data").await?;
        ensure!(before == vec![(1, "unchanged Legacy bytes".to_owned())]);
        ensure!(legacy_rows(db, "legacy_oauth_log").await? == before);
        if partial {
            manager
                .create_table(definition(&SPECS[0], db.get_database_backend()))
                .await?;
        }
        Migration.up(&manager).await?;
        Migration.up(&manager).await?;
        ensure!(legacy_definitions(db).await? == definitions);
        ensure!(legacy_rows(db, "legacy_oauth_data").await? == before);
        ensure!(legacy_rows(db, "legacy_oauth_log").await? == before);
        db.execute_unprepared("INSERT INTO legacy_oauth_data VALUES (2,'trigger still runs')")
            .await?;
        let after = legacy_rows(db, "legacy_oauth_data").await?;
        ensure!(after.len() == 2 && legacy_rows(db, "legacy_oauth_log").await? == after);
        ensure!(Migration.down(&manager).await.is_err());
        for spec in SPECS {
            ensure!(manager.has_table(spec.table).await?);
        }
        ensure!(legacy_definitions(db).await? == definitions);
        ensure!(legacy_rows(db, "legacy_oauth_data").await? == after);
        ensure!(legacy_rows(db, "legacy_oauth_log").await? == after);
        db.execute_unprepared("DROP VIEW legacy_oauth_view").await?;
        if db.get_database_backend() == DatabaseBackend::Postgres {
            db.execute_unprepared("DROP TRIGGER legacy_oauth_trigger ON legacy_oauth_data")
                .await?;
            db.execute_unprepared("DROP FUNCTION legacy_oauth_audit()")
                .await?;
        } else {
            db.execute_unprepared("DROP TRIGGER legacy_oauth_trigger")
                .await?;
        }
        Migration.down(&manager).await?;
        ensure!(legacy_rows(db, "legacy_oauth_data").await? == after);
        ensure!(legacy_rows(db, "legacy_oauth_log").await? == after);
        db.execute_unprepared("DROP TABLE legacy_oauth_log").await?;
        db.execute_unprepared("DROP TABLE legacy_oauth_data")
            .await?;
    }
    Ok(())
}
#[tokio::test]
async fn unrelated_legacy_view_trigger_and_bytes_survive_fresh_and_partial_upgrade() -> Result<()> {
    let db = Database::connect("sqlite::memory:").await?;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        unrelated_legacy_objects_survive_additive_upgrade(&db),
    )
    .await;
    db.close().await?;
    result?
}

#[tokio::test]
async fn temporary_owned_name_collision_still_refuses_upgrade_before_any_create() -> Result<()> {
    let db = Database::connect("sqlite::memory:").await?;
    db.execute_unprepared(
        "CREATE TEMP VIEW tenancy_receipt_auxiliary AS SELECT 'retained bytes' AS payload",
    )
    .await?;
    let manager = SchemaManager::new(&db);
    let outcome: Result<()> = async {
        ensure!(Migration.up(&manager).await.is_err());
        ensure!(!manager.has_table("tenancy_oauth_slots").await?);
        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT payload FROM temp.tenancy_receipt_auxiliary".to_owned(),
            ))
            .await?
            .context("temporary view")?;
        ensure!(row.try_get::<String>("", "payload")? == "retained bytes");
        Ok(())
    }
    .await;
    db.close().await?;
    outcome
}
