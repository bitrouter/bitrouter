//! Exercise the actual HTTP adapter with a stored, non-executable native Thread.

use std::sync::Arc;
use std::time::Duration;

use eventsource_stream::Eventsource;
use futures::StreamExt;

use super::*;
use crate::harness::service::core_disabled_tests::offline_fixture;
use crate::harness::storage::ExecutionStore;
use crate::harness::thread::{ThreadObservation, ThreadStatus};

#[tokio::test]
async fn offline_http_observation_and_capabilities_match_execution_rejection()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = offline_fixture(api_caller()).await?;
    let before = serde_json::to_vec(&fixture.store.load(&fixture.target.thread_id).await?)?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let token = "offline-execution-test-credential";
    let api = BoundAgentApi {
        listener,
        state: ApiState {
            service: fixture.service.clone(),
            workspaces: vec![fixture.workspace.path().canonicalize()?],
            token: token.into(),
            admission: Arc::new(tokio::sync::Semaphore::new(8)),
        },
    };
    let stop = tokio_util::sync::CancellationToken::new();
    let server = tokio::spawn(api.serve(stop.clone().cancelled_owned()));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let request = |method, path: &str| {
        client
            .request(method, format!("{endpoint}{path}"))
            .bearer_auth(token)
            .header("x-bro-server-instance", &fixture.target.server_instance_id)
            .header("idempotency-key", "offline-request")
    };
    let capabilities: Value = request(reqwest::Method::GET, "/agent/v2/capabilities")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(capabilities["runtime"]["native_execution"], false);
    assert_eq!(
        capabilities["operations"],
        json!(["read_thread", "read_turn", "history", "observe"])
    );
    let response = client
        .get(format!("{endpoint}/agent/v2/capabilities"))
        .bearer_auth("wrong-token")
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    for (suffix, body) in [
        (
            "",
            json!({"workspace":fixture.workspace.path().join("missing"),"model":"fixture"}),
        ),
        ("/absent/turns", json!({"prompt":"blocked","mode":"start"})),
        (
            "/absent/turns",
            json!({"prompt":"blocked","mode":"enqueue"}),
        ),
        ("/absent/turns/absent/cancel", json!({"mode":"active"})),
        ("/absent/turns/absent/cancel", json!({"mode":"queued"})),
        (
            "/absent/inputs",
            json!({"turn_id":"absent","request_id":"absent","approved":true}),
        ),
        (
            "/absent/steer",
            json!({"expected_turn_id":"absent","text":"blocked"}),
        ),
        ("/absent/resume", json!({})),
    ] {
        let response = request(reqwest::Method::POST, &format!("/agent/v2/threads{suffix}"))
            .json(&body)
            .send()
            .await?;
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED, "{suffix}");
        let value: Value = response.json().await?;
        assert_eq!(value["code"], "unsupported_capability");
    }
    let unauthorized = client
        .post(format!("{endpoint}/agent/v2/threads/absent/resume"))
        .bearer_auth("wrong-token")
        .send()
        .await?;
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let thread = format!("/agent/v2/threads/{}", fixture.target.thread_id);
    let read: Value = request(reqwest::Method::GET, &thread)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(
        read["view"]["thread"]["queued"].as_array().map(Vec::len),
        Some(1)
    );
    let response = request(reqwest::Method::GET, &format!("{thread}/observe"))
        .send()
        .await?
        .error_for_status()?;
    let mut events = response.bytes_stream().eventsource();
    let event = tokio::time::timeout(Duration::from_secs(2), events.next())
        .await?
        .ok_or("observation ended before its snapshot")??;
    let observation: ThreadObservation = serde_json::from_str(&event.data)?;
    assert!(
        matches!(observation, ThreadObservation::Snapshot { view, .. }
        if view.thread.thread_id == fixture.target.thread_id
            && view.thread.status == ThreadStatus::RecoveryRequired
            && view.thread.queued.len() == 1)
    );
    drop(events);
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(8), server).await???;
    fixture.service.shutdown().await;
    assert_eq!(
        before,
        serde_json::to_vec(&fixture.store.load(&fixture.target.thread_id).await?)?
    );
    assert_eq!(std::fs::read_dir(fixture.workspace.path())?.count(), 0);
    Ok(())
}
