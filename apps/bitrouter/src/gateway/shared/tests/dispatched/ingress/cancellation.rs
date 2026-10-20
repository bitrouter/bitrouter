//! HTTP response abandonment preserves observed facts and releases only custody.

use super::*;

pub(super) async fn exercise(
    fixture: &Fixture,
    upstream: &Upstream,
    store: &Store,
    tenant: &str,
    serving: &Arc<Serving>,
) -> Result<()> {
    for known in [true, false] {
        Box::pin(scenario(fixture, upstream, store, tenant, serving, known)).await?;
    }
    Ok(())
}

async fn scenario(
    fixture: &Fixture,
    upstream: &Upstream,
    store: &Store,
    tenant: &str,
    serving: &Arc<Serving>,
    known: bool,
) -> Result<()> {
    let before = store
        .get::<Value>(Kind::Tenant, tenant)
        .await?
        .context("missing cancellation tenant")?
        .value;
    let key = uuid::Uuid::new_v4().to_string();
    let mut document = prompt(if known { "chat-batch" } else { "live" });
    document["stream"] = json!(true);
    let response = serving
        .router()
        .oneshot(request(
            fixture,
            "/v1/chat/completions",
            document.clone(),
            Some(&key),
            "drop-stream",
        )?)
        .await?;
    ensure!(response.status() == StatusCode::OK);
    let execution = response.headers()["x-bitrouter-execution-id"]
        .to_str()?
        .to_owned();
    let mut body = response.into_body().into_data_stream();
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(frame) = body.next().await {
            if std::str::from_utf8(&frame?)?.contains(if known { "batch" } else { "hello" }) {
                return Ok(());
            }
        }
        anyhow::bail!("stream ended before first content")
    })
    .await??;
    ensure!(!serving.custody.reconcile_one().await?);
    drop(body);
    serving
        .snapshot()
        .app
        .language_model()
        .context("missing cancellation pipeline")?
        .drain_required_pending_settlements()
        .await?;
    retire(serving).await?;
    let view = fixture.engine.query(&fixture.key.hash, &execution).await?;
    if known {
        ensure!(matches!(view.state, ExecutionState::Terminal { .. }));
        ensure!(
            matches!(view.attempts.first().map(|attempt| &attempt.state),
            Some(AttemptState::Terminal { result }) if result.usage.total_tokens == Some(10)),
            "observed provider terminal and usage must survive HTTP body drop"
        );
    } else {
        ensure!(matches!(view.state, ExecutionState::Unknown { .. }));
        ensure!(matches!(
            view.attempts.first().map(|attempt| &attempt.state),
            Some(AttemptState::Unknown { .. })
        ));
    }
    let after = store
        .get::<Value>(Kind::Tenant, tenant)
        .await?
        .context("missing cancellation tenant")?
        .value;
    let occupied = before["active"].as_u64().context("missing occupancy")?;
    ensure!(after["active"].as_u64() == Some(occupied + u64::from(!known)));
    let calls = upstream.calls.load(Ordering::SeqCst);
    let duplicate = serving
        .router()
        .oneshot(request(
            fixture,
            "/v1/chat/completions",
            document,
            Some(&key),
            "retry-drop",
        )?)
        .await?;
    ensure!(duplicate.status() == StatusCode::ACCEPTED);
    ensure!(duplicate.headers()["x-bitrouter-execution-id"] == execution);
    drop(duplicate);
    retire(serving).await?;
    ensure!(upstream.calls.load(Ordering::SeqCst) == calls);
    Ok(())
}
