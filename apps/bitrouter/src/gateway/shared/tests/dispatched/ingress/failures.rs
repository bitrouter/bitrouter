//! An unconfirmed admission exposes only a candidate identity for investigation.

use super::*;
use crate::gateway::shared::engine::Rejection;
use sea_orm::TransactionTrait;

pub(super) async fn exercise(
    fixture: &Fixture,
    upstream: &Upstream,
    store: &Store,
    tenant: &str,
    serving: &Arc<Serving>,
) -> Result<()> {
    let calls = upstream.calls.load(Ordering::SeqCst);
    let key = uuid::Uuid::new_v4().to_string();
    // Deliberately hold a conflicting transaction beyond the server's bounded
    // admission wait. This tests the unconfirmed-error response, not a lost
    // acknowledgement after a successful commit.
    let held = store.database().begin().await?;
    store.lock::<Value>(&held, Kind::Tenant, tenant).await?;
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        serving.router().oneshot(request(
            fixture,
            "/v1/chat/completions",
            prompt("ok"),
            Some(&key),
            "admission-unconfirmed",
        )?),
    )
    .await;
    held.rollback().await?;
    let response = result??;
    ensure!(response.status() == StatusCode::SERVICE_UNAVAILABLE);
    let candidate = response.headers()["x-bitrouter-execution-id"]
        .to_str()?
        .to_owned();
    ensure!(response.headers()["x-bitrouter-request-id"] == "admission-unconfirmed");
    ensure!(response.headers()["location"] == format!("/v1/gateway/executions/{candidate}"));
    ensure!(response.headers()["retry-after"] == "1");
    drop(response);
    retire(serving).await?;
    let error = fixture
        .engine
        .query(&fixture.key.hash, &candidate)
        .await
        .err()
        .context("uncommitted candidate unexpectedly exists")?;
    ensure!(matches!(
        error.downcast_ref::<Rejection>(),
        Some(Rejection::NotFound)
    ));
    ensure!(upstream.calls.load(Ordering::SeqCst) == calls);
    // Explicit resubmission binds a new service ID, proving that the failed
    // transaction left no idempotency binding. The server itself never retries.
    let response = serving
        .router()
        .oneshot(request(
            fixture,
            "/v1/chat/completions",
            prompt("ok"),
            Some(&key),
            "explicit-resubmission",
        )?)
        .await?;
    ensure!(response.status() == StatusCode::OK);
    ensure!(response.headers()["x-bitrouter-execution-id"] != candidate);
    drop(response);
    retire(serving).await?;
    ensure!(upstream.calls.load(Ordering::SeqCst) == calls + 1);
    Ok(())
}
