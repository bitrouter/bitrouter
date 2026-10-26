use super::*;
use futures::FutureExt;
mod database;
#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL and narrow bitrouter_eval_ CREATE/DROP grant"]
async fn owned_oauth_slot_and_auxiliary_receipt_real_commit_rollback_reconnect() -> Result<()> {
    let mut fixture = database::Fixture::create().await?;
    let result = tokio::time::timeout(
        Duration::from_secs(90),
        std::panic::AssertUnwindSafe(async {
            let case = case()?;
            let db = fixture.connect().await?;
            write_and_verify(&db, &case).await?;
            database::close(&db).await?;
            let reopened = fixture.connect().await?;
            committed_generation_and_receipt(&reopened, &case).await
        })
        .catch_unwind(),
    )
    .await
    .context("OAuth storage server deadline")
    .and_then(|result| result.map_err(|_| anyhow::anyhow!("OAuth storage server unwound")))
    .and_then(|result| result);
    fixture.finish(result).await
}
