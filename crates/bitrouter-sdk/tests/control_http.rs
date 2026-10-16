//! Control transport tests using HTTP peers without any application dependency.

use std::{collections::VecDeque, error::Error as StdError, sync::Arc};

use axum::{body::Body, extract::Request, response::Response, routing::any};
use bitrouter_ai::control::action::Action;
use bitrouter_sdk::{
    control::{ControlClient, ControlOptions, normalize_endpoint},
    error::Error,
};
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::Mutex, task::JoinHandle};

type TestResult<T = ()> = Result<T, Box<dyn StdError + Send + Sync>>;
const TOKEN: &str = "fixture-control-bearer-0123456789abcdef";
const INSTANCE: &str = "47da0290-4510-4f39-ab53-a0b09ba707c2";
const NEXT_INSTANCE: &str = "d2142927-3f57-4f86-9941-99b1e489ec33";

struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
}

impl Reply {
    fn json(value: Value) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type", "application/json".into())],
            body: value.to_string(),
        }
    }
}

struct Recorded {
    method: String,
    uri: String,
    credential: Option<String>,
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
                    credential: parts
                        .headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned),
                    body: axum::body::to_bytes(body, 1024 * 1024).await?.to_vec(),
                });
                let reply = replies
                    .lock()
                    .await
                    .pop_front()
                    .ok_or_else(|| std::io::Error::other("unexpected control request"))?;
                let mut response = Response::builder().status(reply.status);
                for (name, value) in reply.headers {
                    response = response.header(name, value);
                }
                Ok::<_, Box<dyn StdError + Send + Sync>>(response.body(Body::from(reply.body))?)
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

fn capabilities(instance: &str, actions: &[Action]) -> Value {
    let mut descriptors = serde_json::Map::new();
    for action in actions {
        descriptors.insert(
            action.id().into(),
            json!({
                "version": action.version(), "required_scope": action.required_scope(),
                "input_schema": {}, "output_schema": {}
            }),
        );
    }
    json!({
        "protocol": "bitrouter-control", "protocol_version": 1,
        "server_version": "fixture", "server_instance_id": instance,
        "actions": actions.iter().map(|a| a.legacy_name()).collect::<Vec<_>>(),
        "action_descriptors": descriptors,
        "scopes": ["control:read", "control:reload"],
        "resources": {
            "state": {"version": 1, "required_scope": "control:read", "path": "/control/v1/state"},
            "operation": {"version": 1, "required_scope": "control:reload", "path": "/control/v1/operations/{request_id}"}
        }
    })
}

#[test]
fn endpoint_and_credentials_are_explicit_and_bounded() -> TestResult {
    assert_eq!(
        normalize_endpoint("http://127.0.0.1:4358")?,
        "http://127.0.0.1:4358/control/v1/"
    );
    assert_eq!(
        normalize_endpoint("https://router.example/control/v1")?,
        "https://router.example/control/v1/"
    );
    for endpoint in [
        "http://router.example",
        "https://user:private@router.example",
        "https://router.example/?key=private",
        "https://router.example/other",
        "ftp://localhost",
    ] {
        assert!(normalize_endpoint(endpoint).is_err());
    }
    assert!(ControlClient::new("http://localhost", "short").is_err());
    assert!(ControlClient::new("http://localhost", &format!("{TOKEN}\ninvalid")).is_err());
    assert!(
        ControlClient::with_options(
            "http://localhost",
            TOKEN,
            ControlOptions {
                max_response_bytes: 0,
                ..Default::default()
            }
        )
        .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn legacy_discovery_never_infers_new_admin_or_mutation_grants() -> TestResult {
    let server = serve(vec![
        Reply::json(json!({
            "protocol": "bitrouter-control", "protocol_version": 1, "server_version": "legacy",
            "actions": ["status", "models", "route_preview", "requests", "reload", "providers_list"]
        })),
        Reply::json(json!({"running": true, "providers": []})),
    ])
    .await?;
    let client = ControlClient::new(&server.endpoint, TOKEN)?;
    for action in [
        Action::Status,
        Action::Models,
        Action::Route,
        Action::Requests,
    ] {
        assert!(client.can_action(action).await?);
    }
    for action in [Action::Reload, Action::Providers, Action::PolicyShow] {
        assert!(!client.can_action(action).await?);
    }
    assert!(client.status().await?.running);
    let records = server.records.lock().await;
    assert_eq!(
        records.iter().map(|r| r.uri.as_str()).collect::<Vec<_>>(),
        ["/control/v1/capabilities", "/control/v1/status"]
    );
    assert!(
        records
            .iter()
            .all(|r| r.credential.as_deref() == Some(format!("Bearer {TOKEN}").as_str()))
    );
    Ok(())
}

#[tokio::test]
async fn incompatible_protocol_descriptors_and_scopes_prevent_dispatch() -> TestResult {
    for case in 0..4 {
        let mut caps = capabilities(INSTANCE, &[Action::Status]);
        match case {
            0 => caps["protocol_version"] = json!(99),
            1 => caps["action_descriptors"]["status"]["version"] = json!(2),
            2 => caps["action_descriptors"]["status"]["required_scope"] = json!("control:reload"),
            _ => caps["scopes"] = json!(["control:reload"]),
        }
        let server = serve(vec![Reply::json(caps)]).await?;
        let result = ControlClient::new(&server.endpoint, TOKEN)?.status().await;
        match case {
            0 => assert!(matches!(
                result,
                Err(Error::ControlProtocolVersion {
                    server: 99,
                    client: 1
                })
            )),
            _ => assert!(matches!(
                result,
                Err(Error::ControlActionUnavailable {
                    action: Action::Status
                })
            )),
        }
        assert_eq!(server.records.lock().await.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn boot_changes_and_authorization_errors_invalidate_cached_capabilities() -> TestResult {
    for unauthorized in [false, true] {
        let reply = if unauthorized {
            Reply {
                status: 401,
                headers: vec![],
                body: json!({"error":{"code":"unauthorized","message":"expired"}}).to_string(),
            }
        } else {
            let mut reply = Reply::json(json!({"running": true, "providers": []}));
            reply
                .headers
                .push(("x-bitrouter-control-instance", NEXT_INSTANCE.into()));
            reply
        };
        let server = serve(vec![
            Reply::json(capabilities(INSTANCE, &[Action::Status])),
            reply,
            Reply::json(capabilities(NEXT_INSTANCE, &[Action::Models])),
        ])
        .await?;
        let client = ControlClient::new(&server.endpoint, TOKEN)?;
        assert_eq!(client.status().await.is_err(), unauthorized);
        assert!(!client.can_action(Action::Status).await?);
        assert!(client.can_action(Action::Models).await?);
        assert_eq!(server.records.lock().await.len(), 3);
    }
    Ok(())
}

#[tokio::test]
async fn malformed_or_oversized_errors_retain_http_status_and_headers() -> TestResult {
    for oversized in [false, true] {
        let server = serve(vec![Reply {
            status: 401,
            headers: vec![
                ("www-authenticate", "Bearer realm=fixture".into()),
                ("retry-after", "7".into()),
            ],
            body: if oversized {
                "x".repeat(2048)
            } else {
                "private peer body".into()
            },
        }])
        .await?;
        let client = ControlClient::with_options(
            &server.endpoint,
            TOKEN,
            ControlOptions {
                max_response_bytes: 1024,
                ..Default::default()
            },
        )?;
        let error = client
            .capabilities()
            .await
            .err()
            .ok_or("expected error response")?;
        assert!(!format!("{error:?}").contains("private peer body"));
        let Error::Control(failure) = error else {
            return Err("lost control HTTP metadata".into());
        };
        assert_eq!(failure.status, 401);
        assert_eq!(failure.headers["www-authenticate"], "Bearer realm=fixture");
        assert_eq!(failure.headers["retry-after"], "7");
        assert!(failure.body.is_none());
        assert_eq!(matches!(failure.body_error, Some(Error::Limit)), oversized);
        assert_eq!(server.records.lock().await.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn redirects_never_forward_the_control_bearer() -> TestResult {
    let destination = serve(vec![Reply::json(capabilities(INSTANCE, &[]))]).await?;
    let origin = serve(vec![Reply {
        status: 302,
        headers: vec![(
            "location",
            format!("{}/control/v1/capabilities", destination.endpoint),
        )],
        body: String::new(),
    }])
    .await?;
    let result = ControlClient::new(&origin.endpoint, TOKEN)?
        .capabilities()
        .await;
    assert!(matches!(result, Err(Error::Control(failure)) if failure.status == 302));
    assert_eq!(origin.records.lock().await.len(), 1);
    assert!(destination.records.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn lost_reload_receipt_retains_identity_and_never_reposts() -> TestResult {
    let server = serve(vec![
        Reply::json(capabilities(INSTANCE, &[Action::Reload])),
        Reply::json(json!({"server_instance_id":INSTANCE,"generation":7,"running":false,"consistency":"consistent","live_policy_digest":null})),
        Reply { status: 200, headers: vec![], body: "{truncated".into() },
    ]).await?;
    let error = ControlClient::new(&server.endpoint, TOKEN)?
        .reload()
        .await
        .err()
        .ok_or("expected unknown receipt")?;
    let Error::ReloadUnknown { input, source } = error else {
        return Err("lost reload recovery identity".into());
    };
    assert_eq!(input.expected_generation, 7);
    assert_eq!(input.expected_server_instance_id, INSTANCE);
    uuid::Uuid::parse_str(&input.request_id)?;
    assert!(matches!(*source, Error::Response(_)));
    let records = server.records.lock().await;
    assert_eq!(records.len(), 3);
    let posts = records
        .iter()
        .filter(|r| r.method == "POST")
        .collect::<Vec<_>>();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].uri, "/control/v1/reload");
    assert_eq!(
        serde_json::from_slice::<Value>(&posts[0].body)?,
        serde_json::to_value(input)?
    );
    Ok(())
}

#[tokio::test]
async fn a_truncated_error_keeps_its_status_and_authentication_challenge() -> TestResult {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await?;
        let mut request = [0; 4096];
        let count = socket.read(&mut request).await?;
        socket.write_all(b"HTTP/1.1 403 Forbidden\r\nWWW-Authenticate: Bearer scope=control:read\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n{\"error\":").await?;
        socket.shutdown().await?;
        Ok::<_, std::io::Error>(request[..count].to_vec())
    });
    let result = ControlClient::new(&endpoint, TOKEN)?.capabilities().await;
    let request = server.await??;
    assert!(request.starts_with(b"GET /control/v1/capabilities "));
    let Err(Error::Control(failure)) = result else {
        return Err("lost the control HTTP error after a partial body".into());
    };
    assert_eq!(failure.status, 403);
    assert_eq!(
        failure.headers["www-authenticate"],
        "Bearer scope=control:read"
    );
    assert!(failure.body.is_none());
    assert!(matches!(failure.body_error, Some(Error::Transport(_))));
    Ok(())
}

#[tokio::test]
async fn explicit_reload_submission_and_recovery_keep_the_same_identity() -> TestResult {
    use bitrouter_ai::control::reload::{OperationStatus, ReloadInput, ReloadOutcome};
    let input = ReloadInput {
        request_id: NEXT_INSTANCE.into(),
        expected_server_instance_id: INSTANCE.into(),
        expected_generation: 7,
    };
    let running = json!({
        "request_id":input.request_id, "server_instance_id":INSTANCE,
        "generation_before":7, "status":"running", "accepted_at_unix_ms":1000,
        "completed_at_unix_ms":null, "retain_until_unix_ms":null,
        "lookup_url":format!("/control/v1/operations/{}?instance={INSTANCE}", input.request_id),
        "result":null
    });
    let mut completed = running.clone();
    completed["status"] = json!("succeeded");
    completed["completed_at_unix_ms"] = json!(1001);
    completed["retain_until_unix_ms"] = json!(61001);
    completed["result"] = json!({
        "server_instance_id":INSTANCE, "generation":8, "outcome":"succeeded",
        "participants":[{"participant":"routing_table","outcome":"applied"}],
        "started_at":"2026-10-15T00:00:00Z", "completed_at":"2026-10-15T00:00:01Z"
    });
    let server = serve(vec![
        Reply::json(capabilities(INSTANCE, &[Action::Reload])),
        Reply::json(running),
        Reply::json(completed),
    ])
    .await?;
    let client = ControlClient::new(&server.endpoint, TOKEN)?;
    let receipt = client.submit_reload(&input).await?;
    assert_eq!(receipt.request_id, input.request_id);
    assert_eq!(receipt.status, OperationStatus::Running);
    let recovered = client.operation(&input.request_id, INSTANCE).await?;
    assert!(recovered.succeeded());
    assert_eq!(recovered.server_instance_id, INSTANCE);
    let result = recovered.result.ok_or("missing completed reload result")?;
    assert_eq!(result.generation, 8);
    assert_eq!(result.outcome, ReloadOutcome::Succeeded);
    let records = server.records.lock().await;
    assert_eq!(records.len(), 3);
    assert_eq!(records[1].method, "POST");
    assert_eq!(records[1].uri, "/control/v1/reload");
    assert_eq!(
        serde_json::from_slice::<ReloadInput>(&records[1].body)?,
        input
    );
    assert_eq!(records[2].method, "GET");
    assert_eq!(
        records[2].uri,
        format!(
            "/control/v1/operations/{}?instance={INSTANCE}",
            input.request_id
        )
    );
    Ok(())
}
