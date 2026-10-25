//! Isolated owned test databases; never migrate/down/drop the supplied database.
use super::*;
use futures::{FutureExt, future::LocalBoxFuture};
use std::{panic::AssertUnwindSafe, time::Duration};

mod database;
use database::Fixture;

async fn run(
    case: impl for<'a> FnOnce(&'a mut Fixture) -> LocalBoxFuture<'a, Result<()>>,
) -> Result<()> {
    let mut fixture = Fixture::create().await?;
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        AssertUnwindSafe(case(&mut fixture)).catch_unwind(),
    )
    .await
    .context("capture Legacy backend case deadline exceeded")
    .and_then(|value| value.map_err(|_| anyhow::anyhow!("capture Legacy backend case unwound")))
    .and_then(|value| value);
    fixture.finish(result).await
}

#[tokio::test]
#[ignore = "requires dedicated PG/MySQL BITROUTER_TEST_DATABASE_URL with CREATE/DROP fixture privileges"]
async fn server_nine_table_schema_bytes_and_restart() -> Result<()> {
    run(|fixture| {
        Box::pin(async move {
            let db = fixture.connect().await?;
            round_trip(&db).await?;
            let before = fixture::historical(&db).await?;
            fixture::metadata(&db, TABLES[8], COLUMNS[2], Some("retained-creator")).await?;
            database::close(&db).await?;
            let db = fixture.connect().await?;
            let manager = SchemaManager::new(&db);
            Migration.up(&manager).await?;
            ensure!(Migration.down(&manager).await.is_err());
            fixture::complete(&manager).await?;
            ensure!(fixture::historical(&db).await? == before);
            fixture::metadata(&db, TABLES[8], COLUMNS[2], None).await?;
            Migration.down(&manager).await?;
            fixture::absent(&manager).await?;
            ensure!(fixture::historical(&db).await? == before);
            Ok(())
        })
    })
    .await
}

#[cfg(all(feature = "core", feature = "harness"))]
#[tokio::test]
#[ignore = "requires dedicated PG/MySQL BITROUTER_TEST_DATABASE_URL with CREATE/DROP fixture privileges"]
async fn server_real_consumers_preserve_foreign_graph_and_cross_family_jobs() -> Result<()> {
    run(|fixture| {
        Box::pin(async move {
            let db = fixture.connect().await?;
            crate::db::run_migrations(&db).await?;
            crate::storage::legacy::tests::mixed_namespace(&db).await
        })
    })
    .await
}
