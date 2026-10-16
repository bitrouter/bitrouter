//! Execution transport tests with HTTP peers and public contracts only.

use std::{collections::VecDeque, error::Error as StdError, sync::Arc, time::Duration};

use axum::{body::Body, extract::Request, response::Response, routing::any};
use bitrouter_ai::harness::{api, runtime::ErrorCode, thread::ThreadObservation};
use bitrouter_sdk::{error::Error, gateway::TransportOptions, harness::HarnessClient};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};

type TestResult<T = ()> = Result<T, Box<dyn StdError + Send + Sync>>;
const TOKEN: &str = "fixture-execution-credential-0123456789";
const INSTANCE: &str = "instance-current";
const THREAD: &str = "thread-one";
const TURN: &str = "turn-one";

struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Body,
}

impl Reply {
    fn json(value: Value) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type", "application/json".into())],
            body: Body::from(value.to_string()),
        }
    }

    fn sse(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type", "text/event-stream".into())],
            body: Body::from(bytes.into()),
        }
    }
}

struct Recorded {
    method: String,
    uri: String,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

struct Server {
    endpoint: String,
    records: Arc<Mutex<Vec<Recorded>>>,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(replies: Vec<Reply>) -> TestResult<Server> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let records = Arc::new(Mutex::new(Vec::new()));
    let captured = records.clone();
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let router = axum::Router::new().fallback(any(move |request: Request| {
        let records = captured.clone();
        let replies = replies.clone();
        async move {
            let result = async {
                let (parts, body) = request.into_parts();
                records.lock().await.push(Recorded {
                    method: parts.method.to_string(),
                    uri: parts.uri.to_string(),
                    headers: parts.headers,
                    body: axum::body::to_bytes(body, 1024 * 1024).await?.to_vec(),
                });
                let reply = replies
                    .lock()
                    .await
                    .pop_front()
                    .ok_or_else(|| std::io::Error::other("unexpected harness request"))?;
                let mut response = Response::builder().status(reply.status);
                for (name, value) in reply.headers {
                    response = response.header(name, value);
                }
                Ok::<_, Box<dyn StdError + Send + Sync>>(response.body(reply.body)?)
            }
            .await;
            match result {
                Ok(response) => response,
                Err(_) => {
                    let mut response = Response::new(Body::from("fixture failed"));
                    *response.status_mut() = axum::http::StatusCode::INTERNAL_SERVER_ERROR;
                    response
                }
            }
        }
    }));
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok(Server {
        endpoint,
        records,
        task,
    })
}

fn capabilities() -> Value {
    let mut limits = serde_json::Map::new();
    for name in [
        "startup_roots",
        "startup_records",
        "startup_metadata_bytes",
        "recovery_readers",
        "recovery_page_records",
        "recovery_page_bytes",
        "recovery_records_per_thread",
        "history_page_bytes",
        "events_per_thread",
        "event_bytes_per_thread",
        "subscribers_per_thread",
        "subscriber_bytes_per_thread",
        "steering_inputs_per_turn",
        "steering_bytes_per_turn",
        "hot_threads",
        "queued_turns_per_thread",
        "context_bytes_per_thread",
        "hot_context_bytes",
        "tools_per_turn",
        "global_tools",
        "active_turns",
        "retained_turns",
        "retained_bytes",
        "retention_seconds",
        "subscriber_queue",
        "request_bytes",
    ] {
        limits.insert(name.into(), json!(128));
    }
    json!({"version":2, "operations":["read_thread", "observe", "start_turn"], "runtime":{
        "native_execution":true, "server_instance_id":INSTANCE, "limits":limits,
        "startup_discovery":null, "execution_ownership":null,
    }})
}

fn snapshot() -> Value {
    json!({
        "server_instance_id":INSTANCE, "thread_id":THREAD, "status":"idle",
        "workspace":"/server/workspace", "model":"fixture/model", "permission_profile":"ask",
        "context_version":0, "cursor":0, "active_turn_id":null, "queued":[],
        "pause_reason":null, "waiting_for_capacity":false,
    })
}

fn turn_snapshot() -> Value {
    json!({
        "thread_id":THREAD, "server_instance_id":INSTANCE, "model":"fixture/model",
        "turn_id":TURN, "status":"completed", "cursor":7, "workspace":"/server/workspace",
        "tool_mode":"coding", "final_answer":"answer", "detail":null, "unknown_effect":false,
        "verification":"not_requested", "verification_evidence":null,
        "pending_input_id":null, "pending_input":null,
    })
}

fn view() -> Value {
    json!({"thread":snapshot(), "verification_command":null, "latest_turn":turn_snapshot(), "config":{
        "model":"fixture/model", "model_mode":"fixed", "effort":null, "max_output_tokens":null,
        "instructions":"server instructions", "max_steps":32, "max_tool_calls":128,
        "max_duration":{"secs":600,"nanos":0}, "max_context_bytes":524288,
        "max_spend_microusd":null, "estimate_rates":null, "tool_mode":"coding",
    }})
}

fn receipt() -> Value {
    json!({"user_item_id":"user-one", "thread_id":THREAD, "turn_id":TURN, "queue_order":1, "status":"accepted"})
}

fn event(sequence: u64) -> Value {
    json!({"server_instance_id":"instance-historical", "thread_id":THREAD,
        "seq":sequence, "timestamp_ms":123, "changes":[{"kind":"queue_resumed"}]})
}

fn input() -> TestResult<api::CreateThreadRequest> {
    Ok(serde_json::from_value(
        json!({"workspace":"/server/workspace", "model":"fixture/model"}),
    )?)
}

#[tokio::test]
async fn all_existing_routes_preserve_bodies_headers_and_explicit_identity() -> TestResult {
    let mut created = Reply::json(json!({"version":2,"thread":snapshot()}));
    created.status = 201;
    let mut accepted = Reply::json(json!({"version":2,"receipt":receipt()}));
    accepted.status = 202;
    let history = json!({"server_instance_id":INSTANCE,"thread_id":THREAD,"cutoff":20,
        "events":[event(3),event(12)],"next_after":12});
    let steering = json!({"input_id":"steer-one","turn_id":TURN,"order":1,"status":"received",
        "context_version":null,"next_step_id":null,"reason":null});
    let server = serve(vec![
        Reply::json(capabilities()),
        created,
        Reply::json(json!({"version":2,"view":view()})),
        accepted,
        Reply::json(json!({"version":2,"turn":turn_snapshot()})),
        Reply::json(json!({"version":2,"receipt":receipt()})),
        Reply::json(json!({"version":2,"receipt":steering})),
        Reply::json(json!({"version":2,"receipt":receipt()})),
        Reply::json(json!({"version":2,"thread":snapshot()})),
        Reply::json(json!({"version":2,"history":history})),
    ])
    .await?;
    let client =
        HarnessClient::connect(&format!("{}/prefix/agent/v2/", server.endpoint), TOKEN).await?;
    assert_eq!(client.capabilities().runtime.server_instance_id, INSTANCE);
    assert_eq!(
        client.create_thread("create", &input()?).await?.thread_id,
        THREAD
    );
    assert_eq!(
        client.read_thread(THREAD).await?.config.instructions,
        "server instructions"
    );
    client
        .submit_turn(
            THREAD,
            "turn",
            &api::TurnRequest {
                prompt: "hello\n世界".into(),
                mode: api::TurnMode::Enqueue,
            },
        )
        .await?;
    assert_eq!(
        client
            .read_turn(THREAD, TURN)
            .await?
            .final_answer
            .as_deref(),
        Some("answer")
    );
    client
        .cancel_turn(
            THREAD,
            TURN,
            "cancel",
            &api::CancelRequest {
                mode: api::CancelMode::Queued,
            },
        )
        .await?;
    client
        .steer(
            THREAD,
            "steer",
            &api::SteeringRequest {
                expected_turn_id: TURN.into(),
                text: "change".into(),
            },
        )
        .await?;
    client
        .answer_input(
            THREAD,
            "input",
            &api::InputRequest {
                turn_id: TURN.into(),
                request_id: "approval-one".into(),
                approved: false,
            },
        )
        .await?;
    client.resume_queue(THREAD, "resume").await?;
    let page = client
        .history(
            THREAD,
            &api::HistoryQuery {
                after: 0,
                cutoff: Some(20),
                limit: 2,
            },
        )
        .await?;
    assert_eq!(page.next_after, Some(12));
    assert_eq!(page.events[0].server_instance_id, "instance-historical");
    let records = server.records.lock().await;
    assert_eq!(records.len(), 10);
    let expected = [
        ("GET", "capabilities", None),
        ("POST", "threads", Some("create")),
        ("GET", "threads/thread-one", None),
        ("POST", "threads/thread-one/turns", Some("turn")),
        ("GET", "threads/thread-one/turns/turn-one", None),
        (
            "POST",
            "threads/thread-one/turns/turn-one/cancel",
            Some("cancel"),
        ),
        ("POST", "threads/thread-one/steer", Some("steer")),
        ("POST", "threads/thread-one/inputs", Some("input")),
        ("POST", "threads/thread-one/resume", Some("resume")),
        (
            "GET",
            "threads/thread-one/history?after=0&cutoff=20&limit=2",
            None,
        ),
    ];
    for (index, (record, (method, path, key))) in records.iter().zip(expected).enumerate() {
        assert_eq!(record.method, method);
        assert_eq!(record.uri, format!("/prefix/agent/v2/{path}"));
        assert_eq!(record.headers["authorization"], format!("Bearer {TOKEN}"));
        assert_eq!(
            record
                .headers
                .get("x-bro-server-instance")
                .and_then(|value| value.to_str().ok()),
            (index != 0).then_some(INSTANCE)
        );
        assert_eq!(
            record
                .headers
                .get("idempotency-key")
                .and_then(|value| value.to_str().ok()),
            key
        );
    }
    let body: Value = serde_json::from_slice(&records[1].body)?;
    assert_eq!(
        body,
        json!({"workspace":"/server/workspace","model":"fixture/model","model_mode":"fixed",
        "effort":null,"max_output_tokens":null,"read_only":false,"verification_command":null})
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&records[3].body)?,
        json!({"prompt":"hello\n世界","mode":"enqueue"})
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&records[5].body)?,
        json!({"mode":"queued"})
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&records[6].body)?,
        json!({"expected_turn_id":TURN,"text":"change"})
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&records[7].body)?,
        json!({"turn_id":TURN,"request_id":"approval-one","approved":false})
    );
    assert!(records[8].body.is_empty());
    assert!(!format!("{client:?}").contains(TOKEN));
    Ok(())
}

#[tokio::test]
async fn rejected_local_inputs_never_dispatch_or_generate_acceptance_keys() -> TestResult {
    let server = serve(vec![Reply::json(capabilities())]).await?;
    let client = HarnessClient::connect(&server.endpoint, TOKEN).await?;
    for key in [
        "",
        "bad\nkey",
        "非ASCII",
        " leading",
        "trailing ",
        "\tleading",
        "trailing\t",
        " ",
        &"x".repeat(129),
    ] {
        assert!(matches!(
            client.create_thread(key, &input()?).await,
            Err(Error::Request(_))
        ));
    }
    let mut request = input()?;
    request.model = "x".repeat(65 * 1024);
    assert!(matches!(
        client.create_thread("bounded", &request).await,
        Err(Error::Request(_))
    ));
    assert!(matches!(
        client.read_thread("..").await,
        Err(Error::Request(_))
    ));
    assert_eq!(server.records.lock().await.len(), 1);
    Ok(())
}

#[tokio::test]
async fn changed_instances_and_versions_never_refresh_or_replay_mutations() -> TestResult {
    let mut changed = snapshot();
    changed["server_instance_id"] = json!("replacement");
    let server = serve(vec![
        Reply::json(capabilities()),
        Reply::json(json!({"version":2,"thread":changed})),
        Reply::json(json!({"version":3,"thread":snapshot()})),
    ])
    .await?;
    let client = HarnessClient::connect(&server.endpoint, TOKEN).await?;
    assert!(matches!(
        client.create_thread("once", &input()?).await,
        Err(Error::Response(_))
    ));
    assert!(matches!(
        client.resume_queue(THREAD, "explicit-next").await,
        Err(Error::Response(_))
    ));
    assert_eq!(client.capabilities().runtime.server_instance_id, INSTANCE);
    assert_eq!(server.records.lock().await.len(), 3);
    Ok(())
}

#[tokio::test]
async fn errors_retain_http_metadata_with_invalid_or_oversized_bodies() -> TestResult {
    for body in ["not JSON".to_owned(), "x".repeat(4096)] {
        let server = serve(vec![
            Reply::json(capabilities()),
            Reply {
                status: 409,
                headers: vec![("retry-after", "12".into()), ("x-fixture", "kept".into())],
                body: Body::from(body),
            },
        ])
        .await?;
        let client = HarnessClient::connect_with_options(
            &server.endpoint,
            TOKEN,
            TransportOptions {
                max_response_bytes: 2048,
                ..TransportOptions::default()
            },
        )
        .await?;
        let Error::Harness(failure) = client
            .resume_queue(THREAD, "once")
            .await
            .err()
            .ok_or("expected error")?
        else {
            return Err("lost service error metadata".into());
        };
        assert_eq!(failure.status, 409);
        assert_eq!(failure.headers["retry-after"], "12");
        assert_eq!(failure.headers["x-fixture"], "kept");
        assert!(failure.body.is_none() && failure.body_error.is_some());
        assert_eq!(server.records.lock().await.len(), 2);
    }
    let mut denied = Reply::json(json!({"error":"credential rejected","code":"invalid_request"}));
    denied.status = 401;
    let server = serve(vec![denied]).await?;
    let Error::Harness(failure) = HarnessClient::connect(&server.endpoint, TOKEN)
        .await
        .err()
        .ok_or("expected auth failure")?
    else {
        return Err("lost authentication response".into());
    };
    assert_eq!(failure.status, 401);
    assert_eq!(
        failure.body.as_ref().ok_or("missing body")?.code,
        ErrorCode::InvalidRequest
    );
    assert!(!format!("{failure:?}").contains("credential rejected"));
    Ok(())
}

#[tokio::test]
async fn resource_segments_are_encoded_and_redirects_are_not_followed() -> TestResult {
    let id = "thread/with space?#";
    let mut response = view();
    response["thread"]["thread_id"] = json!(id);
    response["latest_turn"]["thread_id"] = json!(id);
    let server = serve(vec![
        Reply::json(capabilities()),
        Reply::json(json!({"version":2,"view":response})),
    ])
    .await?;
    HarnessClient::connect(&server.endpoint, TOKEN)
        .await?
        .read_thread(id)
        .await?;
    assert_eq!(
        server.records.lock().await[1].uri,
        "/agent/v2/threads/thread%2Fwith%20space%3F%23"
    );
    let redirect = serve(vec![Reply {
        status: 307,
        headers: vec![("location", format!("{}/other", server.endpoint))],
        body: Body::empty(),
    }])
    .await?;
    assert!(matches!(
        HarnessClient::connect(&redirect.endpoint, TOKEN).await,
        Err(Error::Harness(_))
    ));
    assert_eq!(server.records.lock().await.len(), 2);
    Ok(())
}

fn frame(value: Value) -> String {
    format!("data: {value}\n\n")
}

#[tokio::test]
async fn observation_continues_past_completion_and_retains_historical_epochs() -> TestResult {
    let mut finished = event(7);
    finished["changes"] = json!([{"kind":"turn_lifecycle","turn_id":TURN,"lifecycle":{
        "kind":"finished","status":"completed","detail":"done","final_answer":"answer",
        "verification":"not_requested","verification_evidence":null,"unknown_effect":false,
    }}]);
    let stream = format!(
        "\u{feff}: keepalive\r\n\r\n{}{}{}{}",
        frame(json!({"type":"snapshot","view":view(),"resynchronized":true,"catchup":[event(3)]})),
        frame(json!({"type":"event","event":finished})),
        frame(json!({"type":"live","after_cursor":7,"event":{
            "thread_id":THREAD,"server_instance_id":INSTANCE,"turn_id":TURN,"seq":99,"timestamp_ms":123,
            "payload":{"kind":"assistant_delta","item_id":"item-two","text":"世界"},
        }})),
        frame(json!({"type":"event","event":event(12)})),
    );
    let fragments: Vec<_> = stream
        .as_bytes()
        .chunks(3)
        .map(|chunk| Ok::<_, std::io::Error>(chunk.to_vec()))
        .collect();
    let mut reply = Reply::sse(Vec::new());
    reply.body = Body::from_stream(futures_util::stream::iter(fragments));
    let server = serve(vec![Reply::json(capabilities()), reply]).await?;
    let client = HarnessClient::connect(&server.endpoint, TOKEN).await?;
    let mut stream = client
        .observe(THREAD, &api::ObserveQuery { after: Some(2) })
        .await?;
    let first = stream.next().await.ok_or("missing snapshot")??;
    let ThreadObservation::Snapshot {
        view,
        catchup,
        resynchronized,
    } = first.data
    else {
        return Err("missing snapshot".into());
    };
    assert!(resynchronized);
    assert_eq!(view.thread.server_instance_id, INSTANCE);
    assert_eq!(catchup[0].server_instance_id, "instance-historical");
    assert!(matches!(
        stream.next().await.ok_or("missing terminal Turn")??.data,
        ThreadObservation::Event { .. }
    ));
    let live = stream
        .next()
        .await
        .ok_or("missing live event after terminal Turn")??;
    assert!(matches!(
        live.data,
        ThreadObservation::Live {
            after_cursor: 7,
            ..
        }
    ));
    let ThreadObservation::Event { event } = stream
        .next()
        .await
        .ok_or("missing later durable event")??
        .data
    else {
        return Err("wrong event".into());
    };
    assert_eq!(event.seq, 12);
    assert!(stream.next().await.is_none());
    let records = server.records.lock().await;
    assert_eq!(records.len(), 2);
    assert_eq!(
        records[1].uri,
        "/agent/v2/threads/thread-one/observe?after=2"
    );
    assert_eq!(records[1].headers["accept"], "text/event-stream");
    Ok(())
}

#[tokio::test]
async fn observation_errors_and_incomplete_frames_end_without_reconnection() -> TestResult {
    for bytes in [
        b"data: {\"type\":\"event\"".to_vec(),
        b"data: {}\n".to_vec(),
        b"data: \xff\n\n".to_vec(),
        b"data: not-json\n\n".to_vec(),
        b"data: {\"unexpected\":\"private-wrong-event\"}\n\n".to_vec(),
        b"event: unexpected\ndata: {}\n\n".to_vec(),
    ] {
        let server = serve(vec![Reply::json(capabilities()), Reply::sse(bytes)]).await?;
        let client = HarnessClient::connect(&server.endpoint, TOKEN).await?;
        let mut stream = client
            .observe(THREAD, &api::ObserveQuery { after: None })
            .await?;
        assert!(matches!(stream.next().await, Some(Err(Error::Response(_)))));
        assert!(stream.next().await.is_none());
        assert_eq!(server.records.lock().await.len(), 2);
    }
    let server = serve(vec![
        Reply::json(capabilities()),
        Reply::sse(
            b"event: error\ndata: {\"code\":\"resync_required\",\"message\":\"read history\"}\n\n"
                .to_vec(),
        ),
    ])
    .await?;
    let client = HarnessClient::connect(&server.endpoint, TOKEN).await?;
    let mut stream = client
        .observe(THREAD, &api::ObserveQuery { after: None })
        .await?;
    let Error::HarnessObservation(failure) = stream
        .next()
        .await
        .ok_or("missing error")?
        .err()
        .ok_or("expected failure")?
    else {
        return Err("wrong error".into());
    };
    assert_eq!(
        failure.body.as_ref().ok_or("missing body")?.code,
        ErrorCode::ResyncRequired
    );
    assert_eq!(failure.status, 200);
    assert!(!format!("{failure:?}").contains("read history"));
    assert!(stream.next().await.is_none());
    Ok(())
}

#[tokio::test]
async fn named_malformed_observation_errors_retain_metadata_without_reconnection() -> TestResult {
    for data in [
        "private-non-json",
        r#"{"unexpected":"private-wrong-envelope"}"#,
        r#"{"code":"future_code","message":"private-detail"}"#,
    ] {
        let mut reply = Reply::sse(format!("event: error\ndata: {data}\n\n"));
        reply.headers.extend([
            ("x-request-id", "observation-request-id".into()),
            ("retry-after", "7".into()),
        ]);
        let server = serve(vec![Reply::json(capabilities()), reply]).await?;
        let client = HarnessClient::connect(&server.endpoint, TOKEN).await?;
        let mut stream = client
            .observe(THREAD, &api::ObserveQuery { after: None })
            .await?;
        let error = stream
            .next()
            .await
            .ok_or("missing observation failure")?
            .err()
            .ok_or("expected observation failure")?;
        assert!(!format!("{error:?}").contains("private-"));
        let Error::HarnessObservation(failure) = error else {
            return Err("malformed error lost its observation failure type".into());
        };
        assert_eq!(failure.status, 200);
        assert_eq!(failure.headers["x-request-id"], "observation-request-id");
        assert_eq!(failure.headers["retry-after"], "7");
        assert!(failure.body.is_none());
        assert!(matches!(failure.body_error, Some(Error::Response(_))));
        assert!(stream.next().await.is_none());
        assert_eq!(server.records.lock().await.len(), 2, "must not reconnect");
    }
    Ok(())
}

#[tokio::test]
async fn observation_bounds_and_current_identity_are_checked_before_delivery() -> TestResult {
    let mut wrong_view = view();
    wrong_view["thread"]["server_instance_id"] = json!("replacement");
    let mut wrong_event = event(7);
    wrong_event["thread_id"] = json!("other");
    for value in [
        json!({"type":"snapshot","view":wrong_view,"resynchronized":false,"catchup":[]}),
        json!({"type":"event","event":wrong_event}),
        json!({"type":"live","after_cursor":0,"event":{
            "thread_id":THREAD,"server_instance_id":"replacement","turn_id":TURN,"seq":1,"timestamp_ms":1,"payload":{"kind":"started"},
        }}),
    ] {
        let server = serve(vec![Reply::json(capabilities()), Reply::sse(frame(value))]).await?;
        let client = HarnessClient::connect(&server.endpoint, TOKEN).await?;
        let mut stream = client
            .observe(THREAD, &api::ObserveQuery { after: None })
            .await?;
        assert!(matches!(stream.next().await, Some(Err(Error::Response(_)))));
    }
    let server = serve(vec![
        Reply::json(capabilities()),
        Reply::sse(format!("data: {}\n\n", "x".repeat(1024))),
    ])
    .await?;
    let client = HarnessClient::connect_with_options(
        &server.endpoint,
        TOKEN,
        TransportOptions {
            max_event_bytes: 128,
            ..TransportOptions::default()
        },
    )
    .await?;
    let mut stream = client
        .observe(THREAD, &api::ObserveQuery { after: None })
        .await?;
    assert!(matches!(stream.next().await, Some(Err(Error::Limit))));
    assert!(stream.next().await.is_none());
    Ok(())
}

#[tokio::test]
async fn cancelling_one_pending_poll_preserves_the_partial_frame() -> TestResult {
    let (sender, receiver) = tokio::sync::mpsc::channel::<Vec<u8>>(2);
    let body = futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver
            .recv()
            .await
            .map(|bytes| (Ok::<_, std::io::Error>(bytes), receiver))
    });
    let mut reply = Reply::sse(Vec::new());
    reply.body = Body::from_stream(body);
    let server = serve(vec![Reply::json(capabilities()), reply]).await?;
    let client = HarnessClient::connect(&server.endpoint, TOKEN).await?;
    let mut stream = client
        .observe(THREAD, &api::ObserveQuery { after: None })
        .await?;
    let frame = frame(json!({"type":"event","event":event(7)})).into_bytes();
    let split = frame.len() / 2;
    sender.send(frame[..split].to_vec()).await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(30), stream.next())
            .await
            .is_err()
    );
    sender.send(frame[split..].to_vec()).await?;
    drop(sender);
    let received = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await?
        .ok_or("missing resumed event")??;
    assert!(matches!(received.data, ThreadObservation::Event { .. }));
    assert!(stream.next().await.is_none());
    assert_eq!(server.records.lock().await.len(), 2);
    Ok(())
}

#[tokio::test]
async fn options_and_discovery_validation_redact_credentials() -> TestResult {
    for endpoint in [
        "file:///tmp/socket",
        "http://user:secret@localhost",
        "http://localhost?token=secret",
        "http://localhost/#fragment",
    ] {
        assert!(matches!(
            HarnessClient::connect(endpoint, TOKEN).await,
            Err(Error::Options(_))
        ));
    }
    for (field, value) in [
        ("version", json!(3)),
        ("instance", json!("bad\nheader")),
        ("instance", json!("")),
    ] {
        let mut report = capabilities();
        if field == "version" {
            report["version"] = value;
        } else {
            report["runtime"]["server_instance_id"] = value;
        }
        let server = serve(vec![Reply::json(report)]).await?;
        let failure = HarnessClient::connect(&server.endpoint, TOKEN)
            .await
            .err()
            .ok_or("invalid discovery accepted")?;
        assert!(matches!(failure, Error::Response(_)));
        assert!(!format!("{failure:?}").contains(TOKEN));
    }
    Ok(())
}
