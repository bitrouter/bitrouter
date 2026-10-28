//! Exercise the actual HTTP adapter with a stored, non-executable native Thread.

use std::sync::Arc;
use std::time::Duration;

use eventsource_stream::Eventsource;
use futures::StreamExt;

use super::*;
use crate::harness::service::core_disabled_tests::offline_fixture;
use crate::harness::storage::ExecutionStore;
use bitrouter_ai::harness::thread::ThreadObservation;
use bitrouter_ai::harness::thread::ThreadStatus;
use bitrouter_sdk::harness::HarnessClient;
use serde_json::{Value, json};

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

fn sdk_unavailable<T>(
    result: bitrouter_sdk::error::Result<T>,
) -> Result<(), Box<dyn std::error::Error>> {
    let failure = match result {
        Err(bitrouter_sdk::error::Error::Harness(failure)) => failure,
        Err(error) => return Err(error.into()),
        Ok(_) => return Err("core-free harness accepted a mutation".into()),
    };
    assert_eq!(failure.status, StatusCode::NOT_IMPLEMENTED.as_u16());
    assert!(failure.body_error.is_none());
    assert_eq!(
        failure.body.ok_or("missing typed harness rejection")?.code,
        ErrorCode::UnsupportedCapability
    );
    Ok(())
}

#[tokio::test]
async fn sdk_reads_offline_history_and_observation_without_acquiring_execution_authority()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = offline_fixture(api_caller()).await?;
    let before = serde_json::to_vec(&fixture.store.load(&fixture.target.thread_id).await?)?;
    let index_before = fixture.store.read_index(0, None, 16, 1024 * 1024).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let credential = "offline-sdk-execution-credential";
    let api = BoundAgentApi {
        listener,
        state: ApiState {
            service: fixture.service.clone(),
            workspaces: vec![fixture.workspace.path().canonicalize()?],
            token: credential.into(),
            admission: Arc::new(tokio::sync::Semaphore::new(8)),
        },
    };
    let stop = tokio_util::sync::CancellationToken::new();
    let server = tokio::spawn(api.serve(stop.clone().cancelled_owned()));
    let client = HarnessClient::connect(&endpoint, credential).await?;
    let capabilities = client.capabilities();
    assert_eq!(capabilities.version, PROTOCOL_VERSION);
    assert!(!capabilities.runtime.native_execution);
    assert_eq!(
        capabilities.runtime.server_instance_id,
        fixture.target.server_instance_id
    );
    assert_eq!(
        capabilities.operations,
        ["read_thread", "read_turn", "history", "observe"]
    );

    let thread_id = &fixture.target.thread_id;
    let view = client.read_thread(thread_id).await?;
    assert_eq!(view.thread.status, ThreadStatus::RecoveryRequired);
    assert_eq!(view.thread.queued.len(), 1);
    let turn_id = &view.thread.queued[0].turn_id;
    let turn = client.read_turn(thread_id, turn_id).await?;
    assert_eq!(turn.status, bitrouter_ai::harness::turn::TurnStatus::Queued);
    let history = client
        .history(
            thread_id,
            &HistoryQuery {
                after: 0,
                cutoff: None,
                limit: 128,
            },
        )
        .await?;
    assert!(!history.events.is_empty());
    assert!(history.events.iter().all(|event| {
        event.thread_id == *thread_id
            && event.server_instance_id != capabilities.runtime.server_instance_id
    }));
    assert!(history.next_after.is_none());
    let exhausted = client
        .history(
            thread_id,
            &HistoryQuery {
                after: history.cutoff,
                cutoff: Some(history.cutoff),
                limit: 128,
            },
        )
        .await?;
    assert!(exhausted.events.is_empty());

    let mut observation = client
        .observe(thread_id, &ObserveQuery { after: None })
        .await?;
    let first = tokio::time::timeout(Duration::from_secs(2), observation.next())
        .await?
        .ok_or("SDK observation detached before its snapshot")??;
    assert!(
        matches!(first.data, ThreadObservation::Snapshot { view, .. }
        if view.thread.thread_id == *thread_id
            && view.thread.status == ThreadStatus::RecoveryRequired
            && view.thread.queued.len() == 1)
    );
    // An idle observation remains attached; it does not declare queued work done.
    assert!(
        tokio::time::timeout(Duration::from_millis(50), observation.next())
            .await
            .is_err()
    );
    drop(observation);
    let before_mutations = client.read_thread(thread_id).await?;

    // Native capability admission precedes workspace lookup and all mutations.
    sdk_unavailable(
        client
            .create_thread(
                "create",
                &CreateThreadRequest {
                    skills: None,
                    workspace: fixture.workspace.path().join("missing"),
                    model: "fixture".into(),
                    model_mode: bitrouter_ai::core::protocol::ModelMode::Fixed,
                    effort: None,
                    max_output_tokens: None,
                    read_only: false,
                    verification_command: None,
                },
            )
            .await,
    )?;
    for mode in [TurnMode::Start, TurnMode::Enqueue] {
        sdk_unavailable(
            client
                .submit_turn(
                    thread_id,
                    "submit",
                    &api::TurnRequest {
                        prompt: "must remain unexecuted".into(),
                        mode,
                    },
                )
                .await,
        )?;
    }
    for mode in [CancelMode::Active, CancelMode::Queued] {
        sdk_unavailable(
            client
                .cancel_turn(thread_id, turn_id, "cancel", &CancelRequest { mode })
                .await,
        )?;
    }
    sdk_unavailable(
        client
            .steer(
                thread_id,
                "steer",
                &api::SteeringRequest {
                    expected_turn_id: turn_id.clone(),
                    text: "must remain unapplied".into(),
                },
            )
            .await,
    )?;
    sdk_unavailable(
        client
            .answer_input(
                thread_id,
                "input",
                &InputRequest {
                    skill_consent: false,
                    turn_id: turn_id.clone(),
                    request_id: "unrequested".into(),
                    approved: true,
                },
            )
            .await,
    )?;
    sdk_unavailable(client.resume_queue(thread_id, "resume").await)?;

    let after = client.read_thread(thread_id).await?;
    assert_eq!(
        serde_json::to_value(before_mutations)?,
        serde_json::to_value(after)?
    );
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(8), server).await???;
    fixture.service.shutdown().await;
    assert_eq!(
        before,
        serde_json::to_vec(&fixture.store.load(thread_id).await?)?
    );
    let index_after = fixture.store.read_index(0, None, 16, 1024 * 1024).await?;
    assert_eq!(index_before.cutoff, index_after.cutoff);
    assert_eq!(index_before.entries, index_after.entries);
    assert_eq!(index_before.next_after, index_after.next_after);
    assert!(
        fixture
            .store
            .read_owner(&fixture.target.server_instance_id)
            .await?
            .is_none()
    );
    assert_eq!(std::fs::read_dir(fixture.workspace.path())?.count(), 0);
    Ok(())
}
