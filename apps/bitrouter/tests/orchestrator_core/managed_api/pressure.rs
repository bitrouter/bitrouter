//! Independent HTTP clients reconstruct complete output above transport limits.

use super::*;

#[tokio::test]
async fn concurrent_json_and_sse_replays_preserve_output_above_the_byte_window() -> Result<()> {
    let text = "line\n\"quote\"\\\u{0000}🦀".repeat(2048);
    let fixture = fixture_with_output(Some(json!([{
        "id":"large", "type":"message", "role":"assistant", "status":"completed",
        "content":[{"type":"output_text","text":text,"annotations":[]}]
    }])))
    .await?;
    let limits = Limits {
        ephemeral_bytes: 4096,
        ..Limits::default()
    };
    let (send, _tools, _store, task) = harness_with_limits(&fixture, limits.clone()).await?;
    let mut request = create("large");
    request["bitrouter"]["limits"] = serde_json::to_value(Limits {
        ephemeral_bytes: 1024,
        ..limits
    })?;
    let first: Value = post(&fixture, &fixture.key, &request).await?.json().await?;
    assert_eq!(first["status"], "completed", "{first}");
    assert_eq!(first["output"][0]["content"][0]["text"], text);
    let reads = (0..4).map(|index| {
        let mut request = request.clone();
        let fixture = &fixture;
        async move {
            let streaming = index % 2 == 0;
            request["stream"] = json!(streaming);
            let response = post(fixture, &fixture.key, &request).await?;
            assert_eq!(response.status(), 200);
            Ok::<_, anyhow::Error>((streaming, response.text().await?))
        }
    });
    let received = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        futures::future::try_join_all(reads),
    )
    .await??;
    for (streaming, body) in received {
        assert!(body.len() > 4096);
        let projected = if streaming {
            let events = body
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .map(serde_json::from_str::<Value>)
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for (index, event) in events.iter().enumerate() {
                assert_eq!(event["sequence_number"], index);
            }
            assert_eq!(
                events.first().context("created")?["type"],
                "response.created"
            );
            let delta = events
                .iter()
                .find(|event| event["type"] == "response.output_text.delta")
                .context("text delta")?;
            assert_eq!(delta["delta"], text);
            let terminal = events.last().context("terminal event")?;
            assert_eq!(terminal["type"], "response.completed");
            terminal["response"].clone()
        } else {
            serde_json::from_str(&body)?
        };
        assert_eq!(projected, first);
    }
    assert_eq!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("upstream")?
            .iter()
            .filter(|request| request.url.path() == "/v1/responses")
            .count(),
        1
    );
    fixture.api.shutdown().await;
    drop(send);
    tokio::time::timeout(std::time::Duration::from_secs(10), task).await???;
    Ok(())
}

#[tokio::test]
async fn oversized_ingress_and_zero_output_capacity_do_not_accept_work() -> Result<()> {
    let fixture = fixture().await?;
    let (send, _tools, store, task) = harness(&fixture).await?;
    let head = store.lock().await.head.clone();
    let mut request = create("oversized");
    request["input"] = json!("x".repeat(65537));
    assert_eq!(post(&fixture, &fixture.key, &request).await?.status(), 413);
    let mut request = create("zero");
    request["bitrouter"]["limits"] = serde_json::to_value(Limits {
        ephemeral_bytes: 0,
        ..Limits::default()
    })?;
    assert_eq!(post(&fixture, &fixture.key, &request).await?.status(), 413);
    let missing_beta = fixture_http_client()
        .build()?
        .post(format!("{}/v1/responses", fixture.base))
        .bearer_auth(&fixture.key)
        .json(&create("missing-header"))
        .send()
        .await?;
    assert_eq!(missing_beta.status(), 400);
    assert_eq!(store.lock().await.head, head);
    assert!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("upstream")?
            .is_empty()
    );
    fixture.api.shutdown().await;
    drop(send);
    tokio::time::timeout(std::time::Duration::from_secs(10), task).await???;
    Ok(())
}

#[tokio::test]
async fn held_http_output_does_not_block_head_queries_or_cancellation() -> Result<()> {
    use tower::ServiceExt;
    let fixture = fixture().await?;
    let limits = Limits {
        ephemeral_bytes: 1024,
        ..Limits::default()
    };
    let (send, mut tools, store, task) = harness_with_limits(&fixture, limits).await?;
    let mut request = create("held");
    request["stream"] = json!(true);
    let response = fixture
        .router
        .clone()
        .oneshot(
            http::Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("authorization", format!("Bearer {}", fixture.key))
                .header("bitrouter-beta", "orchestrator_core=v1")
                .body(axum::body::Body::from(request.to_string()))?,
        )
        .await?;
    assert_eq!(response.status(), 200);
    let mut body = response.into_body().into_data_stream();
    let held = body.next().await.context("retained server output")??;
    // The writer reserves the entire 1024-byte window for this first chunk.
    // Keep its Bytes owner alive throughout independent WebSocket operations.
    assert!(!held.is_empty());
    let command = tokio::time::timeout(std::time::Duration::from_secs(10), tools.recv())
        .await?
        .context("dispatched tool")?;
    let (heads, head) = {
        let records = store.lock().await;
        (records.heads_received, records.head.clone())
    };
    send.send(envelope(
        "head-under-pressure",
        "session.head",
        json!({"durable_head":head}),
    ))
    .await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if store.lock().await.heads_received > heads {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("head query must complete while UI bytes remain held")?;
    let mut cancel = envelope(
        "cancel-under-pressure",
        "run.cancel",
        json!({"run_id":command.run_id}),
    );
    cancel["expected_state_revision"] = json!(store.lock().await.head.state_revision);
    send.send(cancel).await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let records = store.lock().await;
            let accepted = records.batches.last().is_some_and(|batch| {
                batch.decode(&Limits::default()).is_ok_and(|payload| {
                    payload.checkpoint.state["operations"]["cancel-under-pressure"].is_object()
                })
            });
            if accepted {
                break;
            }
            drop(records);
            tokio::task::yield_now().await;
        }
    })
    .await?;
    drop(body);
    drop(held);
    fixture.api.shutdown().await;
    drop(send);
    tokio::time::timeout(std::time::Duration::from_secs(10), task).await???;
    Ok(())
}
