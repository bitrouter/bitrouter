//! Operator transport tests depend on contracts and HTTP only.

#[path = "operator_http/operator_cases.rs"]
mod operator_cases;

use std::{collections::VecDeque, error::Error as StdError, sync::Arc, time::Duration};

use axum::{
    body::{Body, to_bytes},
    extract::Request,
    response::Response,
};
use bitrouter_ai::gateway::operator::*;
use bitrouter_sdk::{
    error::Error,
    operator::{OperatorClient, OperatorOptions},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{Mutex, oneshot},
    task::JoinHandle,
};

type Result<T = ()> = std::result::Result<T, Box<dyn StdError + Send + Sync>>;
const TOKEN: &str = "fixture-operator-token-0123456789abcdef";
const SECRET: &str = "fixture-import-private-material";

struct Recorded {
    method: String,
    uri: String,
    token: Option<String>,
    body: Vec<u8>,
}
struct Reply {
    status: u16,
    body: Value,
    location: Option<String>,
}
struct Server {
    endpoint: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
    stop: Option<oneshot::Sender<()>>,
    task: RetainedTask,
}

struct RetainedTask(Option<JoinHandle<std::io::Result<()>>>);

impl Drop for RetainedTask {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

impl RetainedTask {
    async fn join(&mut self, budget: Duration) -> Result {
        let Some(task) = self.0.as_mut() else {
            return Ok(());
        };
        // Poll a borrow; cancellation keeps the handle and timeout aborts/joins it.
        // https://docs.rs/tokio/latest/tokio/task/struct.JoinHandle.html#cancel-safety
        match tokio::time::timeout(budget, &mut *task).await {
            Ok(result) => {
                self.0 = None;
                result??;
                Ok(())
            }
            Err(error) => {
                task.abort();
                let joined = (&mut *task).await;
                self.0 = None;
                if let Err(joined) = joined
                    && !joined.is_cancelled()
                {
                    return Err(joined.into());
                }
                Err(error.into())
            }
        }
    }
}

impl Server {
    async fn finish(mut self) -> Result {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.join(Duration::from_secs(3)).await
    }
}

async fn serve(replies: Vec<Reply>) -> Result<Server> {
    let replies = replies
        .into_iter()
        .map(|reply| {
            let mut response = Response::builder()
                .status(reply.status)
                .header("content-type", "application/json");
            if let Some(location) = reply.location {
                response = response.header("location", location);
            }
            response.body(Body::from(reply.body.to_string()))
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    serve_responses(replies).await
}

async fn serve_responses(replies: Vec<Response>) -> Result<Server> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/proxy/operator/v1", listener.local_addr()?);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
    let router = axum::Router::new().fallback(move |request: Request| {
        let captured = captured.clone();
        let replies = replies.clone();
        async move {
            let (parts, body) = request.into_parts();
            let body = to_bytes(body, 3 * 1024 * 1024)
                .await
                .map(|value| value.to_vec())
                .unwrap_or_default();
            captured.lock().await.push(Recorded {
                method: parts.method.to_string(),
                uri: parts.uri.to_string(),
                token: parts
                    .headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
                body,
            });
            replies.lock().await.pop_front().unwrap_or_else(|| {
                let mut response = Response::new(Body::empty());
                *response.status_mut() = axum::http::StatusCode::INTERNAL_SERVER_ERROR;
                response
            })
        }
    });
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
    });
    Ok(Server {
        endpoint,
        requests,
        stop: Some(stop),
        task: RetainedTask(Some(task)),
    })
}

fn input() -> Mutation {
    Mutation {
        operation_id: "..".into(),
        command: Command::ImportCredential(CredentialImport {
            provider: "fixture".into(),
            account: "default".into(),
            resource_pool_id: "pool".into(),
            expected_version: None,
            credential: CredentialInput::ApiKey {
                value: SECRET.into(),
            },
        }),
    }
}

fn receipt() -> Value {
    json!({"namespace":"fixture", "operator_id":"operator", "operation_id":"..", "committed_at_ms":42,
        "outcome":{"kind":"credential","value":{"provider":"fixture","account":"default","resource_pool_id":"pool","version":1}}})
}

#[tokio::test]
async fn submit_once_and_lookup_keep_public_identity_without_secret_errors() -> Result {
    let server = serve(vec![
        Reply {
            status: 503,
            body: json!({"code":"unknown","operation_id":".."}),
            location: None,
        },
        Reply {
            status: 200,
            body: json!({"status":"unresolved","operation_id":".."}),
            location: None,
        },
        Reply {
            status: 200,
            body: json!({"status":"committed","receipt":receipt()}),
            location: None,
        },
    ])
    .await?;
    let result = async {
        let client = OperatorClient::new(&server.endpoint, TOKEN)?;
        let error = client.apply(&input()).await.err().ok_or("accepted unknown write")?;
        assert!(matches!(&error, Error::OperatorUnknown {operation_id,..} if operation_id == ".."));
        assert!(!format!("{error:?} {error} {client:?}").contains(SECRET));
        assert!(!format!("{client:?}").contains(TOKEN));
        assert!(matches!(client.operation("..").await?,OperationLookup::Unresolved {..}));
        assert!(matches!(client.operation("..").await?,OperationLookup::Committed {..}));
        let requests = server.requests.lock().await;
        assert_eq!(requests.len(),3);
        assert_eq!(requests[0].method,"POST");
        assert_eq!(requests[0].uri,"/proxy/operator/v1/mutations");
        assert_eq!(requests[0].token.as_deref(),Some(format!("Bearer {TOKEN}").as_str()));
        assert_eq!(serde_json::from_slice::<Value>(&requests[0].body)?["command"]["input"]["credential"]["value"],SECRET);
        assert!(requests[1..].iter().all(|request| request.method == "GET" && request.uri == "/proxy/operator/v1/operations?operation_id=.."));
        Ok::<_, Box<dyn StdError+Send+Sync>>(())
    }.await;
    server.finish().await?;
    result
}

#[tokio::test]
async fn malformed_mismatched_and_redirected_writes_remain_unknown() -> Result {
    for reply in [
        Reply {
            status: 200,
            body: json!({"private":SECRET}),
            location: None,
        },
        Reply {
            status: 200,
            body: {
                let mut value = receipt();
                value["operation_id"] = json!("other");
                value
            },
            location: None,
        },
        Reply {
            status: 200,
            body: {
                let mut value = receipt();
                value["outcome"]["value"]["account"] = json!("other");
                value
            },
            location: None,
        },
        Reply {
            status: 302,
            body: json!({}),
            location: Some("http://127.0.0.1:1/stolen".into()),
        },
        Reply {
            status: 200,
            body: json!({"private":"x".repeat(4096)}),
            location: None,
        },
    ] {
        let server = serve(vec![reply]).await?;
        let result = async {
            let client = OperatorClient::with_options(
                &server.endpoint,
                TOKEN,
                OperatorOptions {
                    max_response_bytes: 1024,
                    ..OperatorOptions::default()
                },
            )?;
            let error = client
                .apply(&input())
                .await
                .err()
                .ok_or("accepted invalid write response")?;
            assert!(matches!(error, Error::OperatorUnknown { .. }));
            assert!(!format!("{error:?}").contains(SECRET));
            assert_eq!(server.requests.lock().await.len(), 1);
            Ok::<_, Box<dyn StdError + Send + Sync>>(())
        }
        .await;
        server.finish().await?;
        result?;
    }
    Ok(())
}

#[tokio::test]
async fn lost_reply_does_not_retry_and_retains_only_the_operation_id() -> Result {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let mut peer = RetainedTask(Some(tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let mut bytes = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            let count = stream.read(&mut chunk).await?;
            if count == 0 {
                return Err(std::io::Error::other("request ended before fixture secret"));
            }
            bytes.extend_from_slice(&chunk[..count]);
            if bytes
                .windows(SECRET.len())
                .any(|part| part == SECRET.as_bytes())
            {
                break;
            }
            if bytes.len() > 16384 {
                return Err(std::io::Error::other("fixture request too large"));
            }
        }
        stream.shutdown().await?;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        Ok::<_, std::io::Error>(())
    })));
    let client = OperatorClient::with_options(
        &endpoint,
        TOKEN,
        OperatorOptions {
            request_timeout: Duration::from_secs(1),
            ..OperatorOptions::default()
        },
    )?;
    let response = client.apply(&input()).await;
    let joined = peer.join(Duration::from_secs(3)).await;
    let error = response.err().ok_or("accepted missing response")?;
    assert!(matches!(&error,Error::OperatorUnknown {operation_id,..} if operation_id==".."));
    assert!(!format!("{error:?}").contains(SECRET));
    joined?;
    Ok(())
}

#[tokio::test]
async fn fixture_cleanup_retains_cancelled_waits_and_joins_after_timeout() -> Result {
    struct NotifyDrop(Option<oneshot::Sender<()>>);
    impl Drop for NotifyDrop {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }
    let (started, ready) = oneshot::channel();
    let (finished, dropped) = oneshot::channel();
    let mut task = RetainedTask(Some(tokio::spawn(async move {
        let _guard = NotifyDrop(Some(finished));
        let _ = started.send(());
        std::future::pending::<std::io::Result<()>>().await
    })));
    tokio::time::timeout(Duration::from_secs(1), ready).await??;
    assert!(
        tokio::time::timeout(Duration::from_millis(1), task.join(Duration::from_secs(1)))
            .await
            .is_err()
    );
    assert!(task.0.is_some());
    assert!(task.join(Duration::from_millis(1)).await.is_err());
    assert!(task.0.is_none());
    dropped.await?;
    Ok(())
}

#[tokio::test]
async fn known_rejection_is_typed_and_inputs_reject_before_network() -> Result {
    let server = serve(vec![Reply {
        status: 403,
        body: json!({"code":"forbidden","operation_id":null}),
        location: None,
    }])
    .await?;
    let result = async {
        let client = OperatorClient::new(&server.endpoint, TOKEN)?;
        assert!(matches!(
            client.apply(&input()).await,
            Err(Error::OperatorRejected(ErrorCode::Forbidden))
        ));
        let mut invalid = input();
        invalid.operation_id = "invalid id".into();
        assert!(matches!(
            client.apply(&invalid).await,
            Err(Error::Request(_))
        ));
        let mut oversized = input();
        if let Command::ImportCredential(input) = &mut oversized.command {
            input.credential = CredentialInput::ApiKey {
                value: "x".repeat(2 * 1024 * 1024),
            };
        }
        assert!(matches!(
            client.apply(&oversized).await,
            Err(Error::Request(_))
        ));
        assert!(matches!(
            client.tenant("bad\nidentifier").await,
            Err(Error::Request(_))
        ));
        assert_eq!(server.requests.lock().await.len(), 1);
        Ok::<_, Box<dyn StdError + Send + Sync>>(())
    }
    .await;
    server.finish().await?;
    result
}

#[test]
fn endpoint_and_token_options_are_explicit_and_bounded() {
    for endpoint in [
        "http://example.com",
        "https://user:secret@example.com",
        "https://example.com?token=secret",
        "https://example.com/#secret",
        "https://example.com/control/v1",
        "file:///tmp/operator",
    ] {
        assert!(OperatorClient::new(endpoint, TOKEN).is_err());
    }
    for token in [
        "short",
        "                                ",
        "fixture-token-with-a-newline-0123\n",
    ] {
        assert!(OperatorClient::new("http://localhost", token).is_err());
    }
    assert!(
        OperatorClient::with_options(
            "https://example.com",
            TOKEN,
            OperatorOptions {
                request_timeout: Duration::MAX,
                ..OperatorOptions::default()
            }
        )
        .is_err()
    );
    assert!(OperatorClient::new("http://[::1]:4358/operator/v1", TOKEN).is_ok());
}
