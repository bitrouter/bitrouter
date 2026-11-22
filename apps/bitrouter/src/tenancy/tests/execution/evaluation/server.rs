//! Server-backed directory and receipt regressions independent of experimental routing.
use super::*;
use futures::{FutureExt, future::LocalBoxFuture};
use sea_orm::DatabaseTransaction;
use std::panic::AssertUnwindSafe;

mod database;
mod empty_sources;
mod known_receipts;
mod projection;

async fn run(
    case: impl for<'a> FnOnce(&'a mut database::Fixture) -> LocalBoxFuture<'a, Result<()>>,
) -> Result<()> {
    let mut database = database::Fixture::create().await?;
    let outcome = tokio::time::timeout(
        Duration::from_secs(180),
        AssertUnwindSafe(case(&mut database)).catch_unwind(),
    )
    .await
    .context("compound trajectory campaign deadline exceeded")
    .and_then(|result| result.map_err(|_| anyhow::anyhow!("compound campaign unwound")))
    .and_then(|result| result);
    database.finish(outcome).await
}

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL with PG/MySQL CREATE/DROP DATABASE privileges"]
async fn db_server_both_owned_trajectory_empty_source_compatibility_refuses_retained_rows()
-> Result<()> {
    run(|database| Box::pin(empty_sources::check(database))).await
}

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL with PG/MySQL CREATE/DROP DATABASE privileges"]
async fn db_server_both_owned_trajectory_directory_projection_preserves_integer_decoders_and_bounds()
-> Result<()> {
    run(|database| Box::pin(projection::check(database))).await
}

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL with PG/MySQL CREATE/DROP DATABASE privileges"]
async fn db_server_both_owned_trajectory_known_receipts_drain_restart_and_scan_with_one_reservation()
-> Result<()> {
    run(|database| Box::pin(known_receipts::check(database))).await
}
