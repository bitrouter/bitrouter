//! Opt-in server DDL checks, isolated from populated database_backends fixtures.
//! Requires a dedicated database without the epoch table; cleanup removes only
//! the table created here. A timeout leaves it for deliberate operator cleanup.

use super::*;
use sea_orm::{
    ConnectOptions, Database, DatabaseConnection,
    sea_query::{Expr, Query},
};
use std::time::Duration;

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_MIGRATION_DATABASE_URL for a dedicated database without the epoch table"]
async fn server_migration031_epoch_is_permanent_exact_and_restartable() -> Result<()> {
    let address = std::env::var("BITROUTER_TEST_MIGRATION_DATABASE_URL")
        .context("set BITROUTER_TEST_MIGRATION_DATABASE_URL; no populated-database fallback")?;
    let parsed = url::Url::parse(&address).context("invalid migration test database URL")?;
    match parsed.scheme() {
        "postgres" | "postgresql" => ensure!(cfg!(feature = "db-postgres"), "enable db-postgres"),
        "mysql" => ensure!(cfg!(feature = "db-mysql"), "enable db-mysql"),
        _ => anyhow::bail!("migration test URL must select PostgreSQL or MySQL"),
    }
    ensure!(
        !parsed.path().trim_matches('/').is_empty(),
        "migration test URL must name a dedicated database"
    );
    tokio::time::timeout(Duration::from_secs(120), exercise(&address))
        .await
        .context("migration031 server scenarios exceeded two minutes")?
}

async fn connect(address: &str) -> Result<DatabaseConnection> {
    let mut options = ConnectOptions::new(address);
    options
        .min_connections(1)
        .max_connections(1)
        .sqlx_logging(false);
    Database::connect(options)
        .await
        .map_err(|_| anyhow::anyhow!("could not connect to migration test database"))
}

type Epoch = (i32, String, String, String);

async fn epoch(db: &DatabaseConnection) -> Result<Epoch> {
    let query = Query::select()
        .columns(["singleton", "installation_id", "key_id", "created_at"].map(Alias::new))
        .from(Alias::new(TABLE))
        .limit(2)
        .to_owned();
    let rows = db
        .query_all(db.get_database_backend().build(&query))
        .await?;
    ensure!(rows.len() == 1, "epoch cardinality changed");
    Ok((
        rows[0].try_get("", "singleton")?,
        rows[0].try_get("", "installation_id")?,
        rows[0].try_get("", "key_id")?,
        rows[0].try_get("", "created_at")?,
    ))
}

async fn matches(db: &DatabaseConnection, column: &str, value: &str) -> Result<bool> {
    let query = Query::select()
        .expr(Expr::val(1_i32))
        .from(Alias::new(TABLE))
        .and_where(Expr::col(Alias::new(column)).eq(value))
        .limit(1)
        .to_owned();
    Ok(db
        .query_one(db.get_database_backend().build(&query))
        .await?
        .is_some())
}

async fn cleanup(db: &DatabaseConnection) -> Result<()> {
    // Called only after the absent-table guard and acknowledged creation.
    SchemaManager::new(db)
        .drop_table(Table::drop().table(Alias::new(TABLE)).to_owned())
        .await?;
    Ok(())
}

async fn postgres_unlogged_epoch_is_rejected(
    db: &DatabaseConnection,
    expected: &Epoch,
) -> Result<()> {
    if db.get_database_backend() != DatabaseBackend::Postgres {
        return Ok(());
    }
    db.execute_unprepared("ALTER TABLE tenancy_continuation_epoch SET UNLOGGED")
        .await?;
    let manager = SchemaManager::new(db);
    ensure!(
        Migration.up(&manager).await.is_err(),
        "otherwise identical UNLOGGED epoch was adopted"
    );
    ensure!(
        epoch(db).await? == *expected,
        "rejected UNLOGGED epoch was modified"
    );
    // Repair only this test's table so the following restart is a durable one.
    db.execute_unprepared("ALTER TABLE tenancy_continuation_epoch SET LOGGED")
        .await?;
    Migration.up(&manager).await?;
    Ok(())
}

async fn exercise(address: &str) -> Result<()> {
    let db = connect(address).await?;
    let manager = SchemaManager::new(&db);
    // Never change or clean an existing epoch, even if the supplied URL points
    // at a populated authority integration database by mistake.
    ensure!(
        !manager.has_table(TABLE).await?,
        "dedicated database already contains native epoch table"
    );
    Migration.up(&manager).await?;
    let prepared = async {
        let mut insert = Query::insert();
        insert
            .into_table(Alias::new(TABLE))
            .columns(["singleton", "installation_id", "key_id", "created_at"].map(Alias::new))
            .values([
                1_i32.into(),
                "EpochOwnerCase".into(),
                "A".repeat(64).into(),
                "2026-10-25T00:00:00Z".into(),
            ])?;
        db.execute(db.get_database_backend().build(&insert)).await?;
        let expected = epoch(&db).await?;
        for (column, original, changed) in [
            (
                "installation_id",
                "EpochOwnerCase".to_owned(),
                "epochownercase".to_owned(),
            ),
            (
                "installation_id",
                "EpochOwnerCase".to_owned(),
                "EpochOwnerCase ".to_owned(),
            ),
            ("key_id", "A".repeat(64), "a".repeat(64)),
        ] {
            ensure!(matches(&db, column, &original).await?);
            ensure!(
                !matches(&db, column, &changed).await?,
                "epoch identity comparison was not exact"
            );
        }
        Migration.up(&manager).await?;
        ensure!(
            epoch(&db).await? == expected,
            "resumed migration rewrote the epoch"
        );
        ensure!(Migration.down(&manager).await.is_err());
        ensure!(
            epoch(&db).await? == expected,
            "refused downgrade rewrote the epoch"
        );
        postgres_unlogged_epoch_is_rejected(&db, &expected).await?;
        Ok::<_, anyhow::Error>(expected)
    }
    .await;
    let expected = match prepared {
        Ok(expected) => expected,
        Err(error) => {
            let cleaned = cleanup(&db).await;
            let closed = db.close().await;
            cleaned?;
            closed?;
            return Err(error);
        }
    };
    db.close().await?;
    let db = connect(address).await?;
    let checked = async {
        let manager = SchemaManager::new(&db);
        ensure!(
            epoch(&db).await? == expected,
            "epoch changed after pool restart"
        );
        Migration.up(&manager).await?;
        ensure!(Migration.down(&manager).await.is_err());
        ensure!(epoch(&db).await? == expected);
        // Empty rollback is permitted; populated rollback above must refuse.
        let delete = Query::delete()
            .from_table(Alias::new(TABLE))
            .and_where(Expr::col(Alias::new("singleton")).eq(1_i32))
            .to_owned();
        db.execute(db.get_database_backend().build(&delete)).await?;
        Migration.down(&manager).await?;
        ensure!(!manager.has_table(TABLE).await?);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let cleaned = if SchemaManager::new(&db).has_table(TABLE).await? {
        cleanup(&db).await
    } else {
        Ok(())
    };
    let closed = db.close().await;
    checked?;
    cleaned?;
    closed?;
    Ok(())
}
