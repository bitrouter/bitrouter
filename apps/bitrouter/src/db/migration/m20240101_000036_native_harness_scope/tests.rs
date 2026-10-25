mod sqlite_dependencies;
use super::*;
use anyhow::{Context, Result, ensure};
use sea_orm::{Database, DatabaseConnection};

async fn predecessor(db: &DatabaseConnection) -> Result<()> {
    let manager = SchemaManager::new(db);
    ensure!(
        !manager.has_table("bro_managed_sessions").await?,
        "dedicated empty native-harness database required"
    );
    crate::db::migration::m20240101_000027_bro_managed_sessions::Migration
        .up(&manager)
        .await?;
    db.execute(
        db.get_database_backend().build(
            &Query::insert()
                .into_table(Alias::new("bro_managed_sessions"))
                .columns(["id", "revision", "payload_bytes", "payload"].map(Alias::new))
                .values(["legacy".into(), 1_i64.into(), 2_i64.into(), "{}".into()])?
                .to_owned(),
        ),
    )
    .await?;
    Ok(())
}

async fn matrix(db: &DatabaseConnection) -> Result<()> {
    predecessor(db).await?;
    let manager = SchemaManager::new(db);
    // A previously committed MySQL DDL statement must be recognized exactly.
    manager
        .alter_table(
            Table::alter()
                .table(Alias::new("bro_managed_sessions"))
                .add_column(identity(db.get_database_backend(), "tenant_id", 128, true))
                .to_owned(),
        )
        .await?;
    Migration.up(&manager).await?;
    Migration.up(&manager).await?;
    let row = db
        .query_one(
            db.get_database_backend().build(
                &Query::select()
                    .column(Alias::new("payload"))
                    .from(Alias::new("bro_managed_sessions"))
                    .and_where(Expr::col(Alias::new("id")).eq("legacy"))
                    .to_owned(),
            ),
        )
        .await?
        .context("legacy row")?;
    ensure!(row.try_get::<String>("", "payload")? == "{}");
    db.execute(
        db.get_database_backend().build(
            &Query::update()
                .table(Alias::new("bro_managed_sessions"))
                .value(Alias::new("creator_principal_id"), "partial-owner")
                .to_owned(),
        ),
    )
    .await?;
    ensure!(Migration.down(&manager).await.is_err());
    ensure!(
        manager
            .has_index("bro_managed_sessions", "idx_native36_scope")
            .await?
    );
    for column in COLUMNS {
        ensure!(has_column(&manager, "bro_managed_sessions", column).await?);
    }
    db.execute(
        db.get_database_backend().build(
            &Query::update()
                .table(Alias::new("bro_managed_sessions"))
                .value(Alias::new("creator_principal_id"), Option::<String>::None)
                .to_owned(),
        ),
    )
    .await?;
    Migration.down(&manager).await?;
    ensure!(!has_column(&manager, "bro_managed_sessions", "tenant_id").await?);
    crate::db::migration::m20240101_000027_bro_managed_sessions::Migration
        .down(&manager)
        .await?;
    Ok(())
}

#[tokio::test]
async fn sqlite_native36_resumes_and_refuses_populated_down_without_relabeling() -> Result<()> {
    let db = Database::connect("sqlite::memory:").await?;
    matrix(&db).await
}

#[tokio::test]
async fn sqlite_native36_views_and_external_triggers_refuse_before_index_ddl() -> Result<()> {
    for sql in [
        "CREATE VIEW native_view AS SELECT tenant_id FROM bro_managed_sessions",
        "CREATE VIEW native_view AS SELECT * FROM bro_managed_sessions",
        "CREATE TRIGGER external_trigger AFTER INSERT ON unrelated BEGIN SELECT tenant_id FROM bro_managed_sessions; END",
    ] {
        let db = Database::connect("sqlite::memory:").await?;
        predecessor(&db).await?;
        let manager = SchemaManager::new(&db);
        Migration.up(&manager).await?;
        db.execute_unprepared("CREATE TABLE unrelated (id INTEGER)")
            .await?;
        db.execute_unprepared(sql).await?;
        ensure!(Migration.down(&manager).await.is_err());
        ensure!(
            manager
                .has_index("bro_managed_sessions", "idx_native36_scope")
                .await?
        );
        for column in COLUMNS {
            ensure!(has_column(&manager, "bro_managed_sessions", column).await?);
        }
    }
    Ok(())
}

#[tokio::test]
async fn sqlite_native36_rejects_a_predecessor_view_before_any_addition() -> Result<()> {
    let db = Database::connect("sqlite::memory:").await?;
    db.execute_unprepared("CREATE VIEW bro_managed_sessions AS SELECT 'x' AS id, 1 AS revision, 2 AS payload_bytes, '{}' AS payload").await?;
    let manager = SchemaManager::new(&db);
    ensure!(Migration.up(&manager).await.is_err());
    ensure!(
        !manager
            .has_index("bro_managed_sessions", "idx_native36_scope")
            .await?
    );
    Ok(())
}

#[cfg(any(feature = "db-postgres", feature = "db-mysql"))]
#[tokio::test]
#[ignore = "requires BITROUTER_TEST_MIGRATION_DATABASE_URL for a dedicated database without native harness tables"]
async fn server_native36_legacy_resume_and_down_matrix() -> Result<()> {
    let address = std::env::var("BITROUTER_TEST_MIGRATION_DATABASE_URL")?;
    let parsed = url::Url::parse(&address)?;
    ensure!(
        !parsed.path().trim_matches('/').is_empty(),
        "dedicated database required"
    );
    match parsed.scheme() {
        "postgres" | "postgresql" => ensure!(cfg!(feature = "db-postgres")),
        "mysql" => ensure!(cfg!(feature = "db-mysql")),
        _ => anyhow::bail!("server URL required"),
    }
    let mut options = sea_orm::ConnectOptions::new(address);
    options
        .max_connections(1)
        .min_connections(1)
        .sqlx_logging(false);
    let db = Database::connect(options).await?;
    tokio::time::timeout(std::time::Duration::from_secs(60), matrix(&db)).await??;
    db.close().await?;
    Ok(())
}
