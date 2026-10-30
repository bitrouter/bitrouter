use super::*;
use futures::FutureExt;
use std::time::Duration;
mod database;

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL with existing bitrouter_eval_ CREATE/DROP grant"]
async fn owned_oauth_additive_schema_real_commit_rollback_down_and_reconnect() -> Result<()> {
    let mut fixture = database::Fixture::create().await?;
    let outcome = tokio::time::timeout(
        Duration::from_secs(90),
        std::panic::AssertUnwindSafe(async {
            let db = fixture.connect().await?;
            unrelated_legacy_objects_survive_additive_upgrade(&db).await?;
            retain(&db).await?;
            database::close(&db).await?;
            let reopened = fixture.connect().await?;
            // Independent connection sees the committed opaque bytes; rollback
            // and refused down could not rewrite them.
            verify_and_drop(&reopened).await
        })
        .catch_unwind(),
    )
    .await
    .context("OAuth server campaign deadline")
    .and_then(|outcome| outcome.map_err(|_| anyhow::anyhow!("OAuth server campaign unwound")))
    .and_then(|outcome| outcome);
    fixture.finish(outcome).await
}
