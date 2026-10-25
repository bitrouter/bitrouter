//! Opt-in compound backend regressions, not a production server-backend profile.
//!
//! StartProof is obtained from the acknowledged Service actor begin. Every store
//! is built from that same admitted execution; no test-created scope/proof or
//! post-admission database replacement is used.

use super::*;
use futures::{FutureExt, future::LocalBoxFuture};
use sea_orm::{DatabaseBackend, DatabaseConnection, DatabaseTransaction};
use std::panic::AssertUnwindSafe;

mod cases;
mod database;
mod fault;
mod harness;
mod rows;

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
#[ignore = "requires BITROUTER_TEST_DATABASE_URL with PG/MySQL CREATE/DROP DATABASE and trigger privileges"]
async fn server_owned_trajectory_five_terminal_faults_roll_back_the_compound_closure() -> Result<()>
{
    for point in fault::Point::ALL {
        run(|database| Box::pin(cases::rollback(database, point))).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL with PG/MySQL CREATE/DROP DATABASE privileges"]
async fn server_owned_trajectory_exact_replay_is_read_only_inside_tx_and_durable_on_reopen()
-> Result<()> {
    run(|database| Box::pin(cases::replay(database))).await
}

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL with PG/MySQL CREATE/DROP DATABASE privileges"]
async fn server_owned_trajectory_absent_or_malformed_closure_is_refused_without_repair()
-> Result<()> {
    let mut failures = Vec::new();
    for damage in cases::Damage::ALL {
        if let Err(error) = run(|database| Box::pin(cases::damaged(database, damage))).await {
            failures.push(format!("{damage:?}: {error:#}"));
        }
    }
    ensure!(
        failures.is_empty(),
        "damaged closure cases failed: {}",
        failures.join("; ")
    );
    Ok(())
}
