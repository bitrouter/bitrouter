//! Provider entity admission happens before complete output enters core state.

use super::*;
use bitrouter_orchestrator::core::accounting::work::{CostWorkKind, CostWorkState};
use bitrouter_orchestrator::core::session::SessionSnapshot;

#[tokio::test]
async fn unfinished_chunked_provider_body_is_rejected_without_waiting_for_eof() -> Result<()> {
    let upstream = axum::Router::new()
        .route(
            "/v1/responses/input_tokens",
            axum::routing::post(|| async {
                axum::Json(json!({"object":"response.input_tokens","input_tokens":20}))
            }),
        )
        .route(
            "/v1/responses",
            axum::routing::post(|| async {
                // No Content-Length and no EOF: admission must inspect actual chunks.
                let chunks = futures::stream::iter(
                    (0..257).map(|_| Ok::<_, std::io::Error>(vec![b'x'; 4096])),
                )
                .chain(futures::stream::pending());
                axum::body::Body::from_stream(chunks)
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let provider = format!("http://{}", listener.local_addr()?);
    let mut server = tokio::task::JoinSet::new();
    server.spawn(async move { axum::serve(listener, upstream).await });
    let fixture = configured_fixture_with_provider(None, None, Some(&provider)).await?;
    let (send, mut tools, store, task) = harness(&fixture).await?;
    let mut request = create("chunked-provider");
    request["bitrouter"]["limits"] = serde_json::to_value(Limits {
        checkpoint_bytes: 1024 * 1024,
        ..Limits::default()
    })?;
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        post(&fixture, &fixture.key, &request),
    )
    .await??;
    assert_eq!(response.status(), 200);
    let response: Value = response.json().await?;
    assert_eq!(response["status"], "failed", "{response}");
    assert!(tools.try_recv().is_err());
    let records = store.lock().await;
    let state = &records
        .batches
        .last()
        .context("checkpoint")?
        .decode(&Limits::default())?
        .checkpoint
        .state;
    assert!(serde_json::to_string(state)?.contains("managed upstream response exceeds byte limit"));
    drop(records);
    fixture.api.shutdown().await;
    drop(send);
    tokio::time::timeout(std::time::Duration::from_secs(60), task).await???;
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn oversized_provider_success_and_error_bodies_fail_with_unknown_spend() -> Result<()> {
    for upstream_status in [200, 500, 400, 403, 429] {
        let fixture = fixture().await?;
        let output = "oversized-provider-body".repeat(100_000);
        let reply = if upstream_status == 200 {
            json!({"id":"oversized", "object":"response", "status":"completed", "model":"served",
                "output":[{"id":"call","type":"function_call","call_id":"unaccepted","name":"read",
                    "arguments":format!("{{\"path\":\"{output}\"}}"),"status":"completed"}],
                "usage":{"input_tokens":20,"output_tokens":10,"total_tokens":30}})
        } else {
            json!({"error":{"message":output}})
        };
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(ResponseTemplate::new(upstream_status).set_body_json(reply))
            .with_priority(1)
            .mount(&fixture.upstream)
            .await;
        let (send, mut tools, store, task) = harness(&fixture).await?;
        let mut request = create("oversized-provider");
        request["bitrouter"]["limits"] = serde_json::to_value(Limits {
            checkpoint_bytes: 1024 * 1024,
            ..Limits::default()
        })?;
        let response = post(&fixture, &fixture.key, &request).await?;
        let status = response.status();
        let response: Value = response.json().await?;
        assert_eq!(status, 200, "{response}");
        assert_eq!(response["status"], "failed", "{response}");
        assert_eq!(response["bitrouter"]["run_status"], "failed", "{response}");
        assert!(tools.try_recv().is_err());
        let records = store.lock().await;
        let state: SessionSnapshot = serde_json::from_value(
            records
                .batches
                .last()
                .context("checkpoint")?
                .decode(&Limits::default())?
                .checkpoint
                .state,
        )?;
        let turn = state.root_turn().context("root turn")?;
        assert!(turn.invocations.is_empty());
        let reports: Vec<_> = turn
            .steps
            .iter()
            .flat_map(|step| &step.attempts)
            .filter_map(|attempt| attempt.receipt.as_ref().map(|receipt| &receipt.report))
            .collect();
        assert_eq!(reports.len(), 1);
        assert!(reports[0].result.is_none());
        let error = reports[0].error.as_deref().context("attempt failure")?;
        match upstream_status {
            200 => assert!(error.contains("exceeds byte limit"), "{error}"),
            400 => assert_eq!(error, "upstream bad request"),
            429 => assert_eq!(error, "upstream rate limited"),
            _ => assert!(
                error.contains("managed upstream error body unavailable"),
                "{error}"
            ),
        }
        let run_id = &state.run.as_ref().context("run")?.run_id;
        let ledger = &state.cost_work[run_id];
        let work = ledger
            .work
            .values()
            .find(|work| work.kind == CostWorkKind::ProviderAttempt)
            .context("provider cost exposure")?;
        assert_eq!(work.state, CostWorkState::OutcomeRecorded);
        assert!(!work.unknown_cost_reason.is_empty());
        assert!(ledger.charge_unknown.contains_key(&reports[0].request_id));
        assert!(!serde_json::to_string(&state)?.contains("oversized-provider-body"));
        let head = records.head.clone();
        drop(records);
        let replay: Value = post(&fixture, &fixture.key, &request).await?.json().await?;
        assert_eq!(replay, response);
        assert_eq!(store.lock().await.head, head);
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
        tokio::time::timeout(std::time::Duration::from_secs(60), task).await???;
    }
    Ok(())
}
