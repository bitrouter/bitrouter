//! Real HTTP consumers: no synthetic finalization context can mint a mapping.
use super::{Fixture, MODEL, draft, failure, headers, provision_protocol};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use sea_orm::{ConnectionTrait, Statement};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::mpsc;
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};
use tower::ServiceExt;

mod custody;
mod evidence;
mod isolation;
mod maintenance;
mod output_mutation;
#[cfg(unix)]
mod production_restart;
#[cfg(unix)]
mod quarantine;
mod receipts;
mod retained_defaults;
mod revision_policy;
mod server;
#[cfg(feature = "core")]
mod sockets;
mod store;
#[cfg(feature = "core")]
mod trajectory;
#[cfg(unix)]
mod unknown;

const WAIT: Duration = Duration::from_secs(10);
#[derive(Clone, Default)]
struct Options {
    store: bool,
    trailing_error: bool,
    incomplete: bool,
    extra_output: Option<Value>,
}
#[derive(Clone)]
struct Endpoint {
    options: Arc<Mutex<Options>>,
    calls: Arc<AtomicUsize>,
    received: mpsc::Sender<(HeaderMap, Value)>,
    pause: Arc<Mutex<Option<Arc<tokio::sync::Notify>>>>,
    tail: Arc<Mutex<Option<CancellationToken>>>,
}
async fn respond(
    State(state): State<Endpoint>,
    headers: HeaderMap,
    Json(input): Json<Value>,
) -> Response {
    let number = state.calls.fetch_add(1, Ordering::SeqCst) + 1;
    let options = match state.options.lock() {
        Ok(options) => options.clone(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let stream = input.get("stream") == Some(&Value::Bool(true));
    if state.received.try_send((headers, input)).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let pause = match state.pause.lock() {
        Ok(pause) => pause.clone(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if let Some(pause) = pause {
        pause.notified().await;
    }
    let id = format!("resp-http-private-{number}");
    let mut response = json!({"id":id,"object":"response","status":"completed","store":options.store,"model":"model-a","output":[{"id":"msg-private","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"owned reply","annotations":[]}]}],"usage":{"input_tokens":4,"output_tokens":2,"total_tokens":6}});
    if options.incomplete {
        response["status"] = "incomplete".into();
        response["incomplete_details"] = json!({"reason":"max_output_tokens"});
    }
    if let Some(extra) = options.extra_output
        && let Some(output) = response["output"].as_array_mut()
    {
        output.insert(0, extra);
    }
    if stream {
        let created =
            json!({"type":"response.created","response":{"id":id,"status":"in_progress"}});
        let delta = json!({"type":"response.output_text.delta","delta":"owned reply"});
        let terminal = json!({"type":if options.incomplete { "response.incomplete" } else { "response.completed" },"response":response});
        let tail = if options.trailing_error {
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"forged after terminal\"}\n\n"
        } else {
            ""
        };
        let pause_tail = match state.tail.lock() {
            Ok(pause) => pause.clone(),
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        let streamed = async_stream::stream! {
            yield Ok::<_, std::io::Error>(axum::body::Bytes::from(format!("data: {created}\n\ndata: {delta}\n\n")));
            if let Some(pause) = pause_tail {
                pause.cancelled().await;
            }
            yield Ok(axum::body::Bytes::from(format!("data: {terminal}\n\n{tail}")));
        };
        (
            [("content-type", "text/event-stream")],
            Body::from_stream(streamed),
        )
            .into_response()
    } else {
        Json(response).into_response()
    }
}
struct Server {
    url: String,
    state: Endpoint,
    received: mpsc::Receiver<(HeaderMap, Value)>,
    stop: CancellationToken,
    task: AbortOnDropHandle<std::io::Result<()>>,
}
impl Server {
    async fn start() -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/v1", listener.local_addr()?);
        let (sender, received) = mpsc::channel(32);
        let state = Endpoint {
            options: Arc::new(Mutex::new(Options {
                store: true,
                ..Default::default()
            })),
            calls: Arc::new(AtomicUsize::new(0)),
            received: sender,
            pause: Arc::new(Mutex::new(None)),
            tail: Arc::new(Mutex::new(None)),
        };
        let router = Router::new()
            .route("/v1/responses", post(respond))
            .with_state(state.clone());
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stopped.cancelled_owned())
                .await
        }));
        Ok(Self {
            url,
            state,
            received,
            stop,
            task,
        })
    }
    async fn wire(&mut self) -> Result<Value> {
        let (headers, value) = tokio::time::timeout(WAIT, self.received.recv())
            .await?
            .context("provider send")?;
        ensure!(
            headers
                .get("authorization")
                .is_some_and(|h| h == "Bearer owned-upstream-secret")
        );
        Ok(value)
    }
    async fn close(self) -> Result<()> {
        self.stop.cancel();
        tokio::time::timeout(WAIT, self.task).await???;
        Ok(())
    }
}
struct Ready {
    fixture: Fixture,
    headers: HeaderMap,
    route: bitrouter_ai::tenancy::configuration::ResourceRef,
    revision: bitrouter_ai::tenancy::configuration::ConfigurationRevision,
}
async fn ready(server: &Server) -> Result<Ready> {
    let (fixture, route, _) = provision_protocol(&server.url, "responses").await?;
    let revision = draft(&fixture, &route, "http-continuation-draft", 32).await?;
    let issued = fixture.issue("http-continuation-user").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let headers = headers(
        &fixture,
        issued.secret.as_deref().context("credential")?,
        &revision.revision_id,
    )?;
    Ok(Ready {
        fixture,
        headers,
        route,
        revision,
    })
}
async fn send_response(
    ready: &Ready,
    headers: HeaderMap,
    body: Value,
) -> Result<(StatusCode, Vec<u8>)> {
    send_at(ready, headers, "/v1/responses", body).await
}
async fn send_at(
    ready: &Ready,
    headers: HeaderMap,
    path: &str,
    body: Value,
) -> Result<(StatusCode, Vec<u8>)> {
    let app = Router::new()
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(axum::middleware::from_fn_with_state(
            ready.fixture.service.clone(),
            crate::tenancy::http::dispatch,
        ));
    let mut request = Request::post(path).body(Body::from(serde_json::to_vec(&body)?))?;
    *request.headers_mut() = headers;
    request
        .headers_mut()
        .insert("content-type", "application/json".parse()?);
    let response = tokio::time::timeout(WAIT, app.oneshot(request)).await??;
    let status = response.status();
    let bytes = tokio::time::timeout(
        WAIT,
        axum::body::to_bytes(response.into_body(), 1024 * 1024),
    )
    .await??;
    Ok((status, bytes.to_vec()))
}
async fn send(ready: &Ready, headers: HeaderMap, body: Value) -> Result<(StatusCode, Vec<u8>)> {
    let response = send_response(ready, headers, body).await?;
    tokio::time::timeout(WAIT, ready.fixture.service.admission.wait_drained()).await?;
    Ok(response)
}

fn body(stream: bool, previous: Option<&str>) -> Value {
    let mut value = json!({"model":MODEL,"input":"follow up","stream":stream,"store":true});
    if let Some(previous) = previous {
        value["previous_response_id"] = previous.into();
    }
    value
}
fn public_id(bytes: &[u8], stream: bool) -> Result<String> {
    let response: Value = if stream {
        let text = std::str::from_utf8(bytes)?;
        text.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|value| {
                matches!(
                    value["type"].as_str(),
                    Some("response.completed" | "response.incomplete")
                )
            })
            .and_then(|mut value| value.get_mut("response").map(Value::take))
            .context("completed SSE response")?
    } else {
        serde_json::from_slice(bytes)?
    };
    ensure!(
        response["usage"]["input_tokens"] == 4 && response["usage"]["output_tokens"] == 2,
        "incurred usage lost"
    );
    let id = response["id"].as_str().context("public response id")?;
    ensure!(
        crate::gateway::model::protocol::responses::decode_gateway_continuation_id(id)?.is_some()
    );
    ensure!(!String::from_utf8_lossy(bytes).contains("resp-http-private-"));
    Ok(id.into())
}
async fn row_count(ready: &Ready) -> Result<i64> {
    let row = ready
        .fixture
        .db
        .query_one(Statement::from_string(
            ready.fixture.db.get_database_backend(),
            "SELECT COUNT(*) AS n FROM provider_continuations WHERE installation_id IS NOT NULL"
                .to_owned(),
        ))
        .await?
        .context("row count")?;
    Ok(row.try_get("", "n")?)
}

#[tokio::test]
async fn actual_http_json_and_sse_issue_and_resume_only_with_owned_binding() -> Result<()> {
    for first_stream in [false, true] {
        let mut server = Server::start().await?;
        let ready = ready(&server).await?;
        let result:Result<()>=async{
            let (status,bytes)=send(&ready,ready.headers.clone(),body(first_stream,None)).await?;ensure!(status==StatusCode::OK);let id=public_id(&bytes,first_stream)?;
            ensure!(server.wire().await?.get("previous_response_id").is_none());ensure!(row_count(&ready).await?==1);
            for stream in [false,true]{
                let (status,bytes)=send(&ready,ready.headers.clone(),body(stream,Some(&id))).await?;ensure!(status==StatusCode::OK);public_id(&bytes,stream)?;
                ensure!(server.wire().await?["previous_response_id"]=="resp-http-private-1");
            }
            let row=ready.fixture.db.query_one(Statement::from_string(ready.fixture.db.get_database_backend(),"SELECT ciphertext,nonce,cipher_version,installation_id,tenant_id,creator_principal_id FROM provider_continuations LIMIT 1".to_owned())).await?.context("retained row")?;
            ensure!(row.try_get::<i32>("","cipher_version")?==2 && row.try_get::<Option<String>>("","nonce")?.is_none());
            ensure!(!row.try_get::<String>("","ciphertext")?.contains("resp-http-private"));
            ensure!(row.try_get::<String>("","installation_id")?==ready.fixture.installation.as_str());
            ensure!(row.try_get::<String>("","tenant_id")?==ready.fixture.tenant.as_str());
            ensure!(ready.fixture.service.current().is_ok());Ok(())
        }.await;
        ready.fixture.service.shutdown().await?;
        server.close().await?;
        result?;
    }
    Ok(())
}
#[tokio::test]
async fn no_storage_evidence_and_trailing_sse_error_never_publish_mapping() -> Result<()> {
    let mut server = Server::start().await?;
    let ready = ready(&server).await?;
    let result: Result<()> = async {
        for stream in [false, true] {
            server
                .state
                .options
                .lock()
                .map_err(|_| anyhow::anyhow!("options lock"))?
                .store = false;
            let (status, bytes) = send(&ready, ready.headers.clone(), body(stream, None)).await?;
            ensure!(status == StatusCode::OK);
            let id = public_id(&bytes, stream)?;
            server.wire().await?;
            ensure!(row_count(&ready).await? == 0);
            let before = server.state.calls.load(Ordering::SeqCst);
            let (status, _) = send(&ready, ready.headers.clone(), body(false, Some(&id))).await?;
            ensure!(!status.is_success());
            ensure!(server.state.calls.load(Ordering::SeqCst) == before);
        }
        *server
            .state
            .options
            .lock()
            .map_err(|_| anyhow::anyhow!("options lock"))? = Options {
            store: true,
            trailing_error: true,
            ..Default::default()
        };
        let (_, bytes) = send(&ready, ready.headers.clone(), body(true, None)).await?;
        server.wire().await?;
        ensure!(!String::from_utf8_lossy(&bytes).contains("\"type\":\"response.completed\""));
        ensure!(row_count(&ready).await? == 0);
        ensure!(ready.fixture.service.current().is_ok());
        Ok(())
    }
    .await;
    ready.fixture.service.shutdown().await?;
    server.close().await?;
    result
}
#[tokio::test]
async fn different_principal_same_tenant_cannot_resume_and_raw_ids_are_rejected() -> Result<()> {
    let mut server = Server::start().await?;
    let ready = ready(&server).await?;
    let result: Result<()> = async {
        let (_, bytes) = send(&ready, ready.headers.clone(), body(false, None)).await?;
        let id = public_id(&bytes, false)?;
        server.wire().await?;
        // A credential rotation for the SAME principal remains eligible. The
        // following new principal is distinct while its grants remain broad.
        use crate::tenancy::command::Command;
        use bitrouter_ai::tenancy::authority::{BindingPut, PrincipalPut, TenantRole};
        let principal: bitrouter_ai::tenancy::identity::PrincipalId =
            "other-http-principal".try_into()?;
        ready
            .fixture
            .service
            .execute(
                ready.fixture.operator.clone(),
                Command::Principal(
                    principal.clone(),
                    PrincipalPut {
                        operation_id: "http-new-principal".try_into()?,
                        expected_revision: None,
                        issuer: "test".into(),
                        subject: "other-http-subject".into(),
                        installation_id: ready.fixture.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?;
        ready
            .fixture
            .service
            .execute(
                ready.fixture.operator.clone(),
                Command::Binding(
                    ready.fixture.tenant.clone(),
                    "other-http-binding".try_into()?,
                    BindingPut {
                        operation_id: "http-new-binding".try_into()?,
                        expected_revision: None,
                        principal_id: principal.clone(),
                        role: TenantRole::TenantAdmin,
                        scope: crate::tenancy::mutation::tenant_scope(),
                        expires_at: super::super::expiry()?,
                        expected_management_owner: None,
                        installation_id: ready.fixture.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?;
        let mut issuance = ready.fixture.issuance("http-other-credential")?;
        issuance.principal_id = principal;
        issuance.binding_id = "other-http-binding".try_into()?;
        let other = super::super::issued(
            ready
                .fixture
                .service
                .execute(
                    ready.fixture.operator.clone(),
                    Command::Issue(ready.fixture.tenant.clone(), issuance),
                )
                .await
                .map_err(failure)?,
        )?;
        let other_headers = headers(
            &ready.fixture,
            other.secret.as_deref().context("other credential")?,
            &ready.revision.revision_id,
        )?;
        let before = server.state.calls.load(Ordering::SeqCst);
        let (status, _) = send(&ready, other_headers, body(false, Some(&id))).await?;
        ensure!(!status.is_success());
        let (status, _) = send(
            &ready,
            ready.headers.clone(),
            body(false, Some("resp-http-private-1")),
        )
        .await?;
        ensure!(!status.is_success());
        ensure!(server.state.calls.load(Ordering::SeqCst) == before);
        ensure!(row_count(&ready).await? == 1);
        ensure!(ready.fixture.service.current().is_ok());
        Ok(())
    }
    .await;
    ready.fixture.service.shutdown().await?;
    server.close().await?;
    result
}

#[tokio::test]
async fn explicit_store_false_never_publishes_even_if_provider_claims_storage() -> Result<()> {
    let mut server = Server::start().await?;
    let ready = ready(&server).await?;
    let result: Result<()> = async {
        for stream in [false, true] {
            let mut input = body(stream, None);
            input["store"] = false.into();
            let (status, bytes) = send(&ready, ready.headers.clone(), input).await?;
            ensure!(status == StatusCode::OK);
            public_id(&bytes, stream)?;
            ensure!(server.wire().await?["store"] == false);
            ensure!(row_count(&ready).await? == 0);
            ensure!(
                ready
                    .fixture
                    .service
                    .http_continuation
                    .state
                    .pending_tickets()
                    .map_err(|_| anyhow::anyhow!("pending state"))?
                    .is_empty()
            );
        }
        let before = server.state.calls.load(Ordering::SeqCst);
        for conversation in [
            json!("unowned-conversation"),
            json!({"id":"unowned-conversation"}),
        ] {
            let mut input = body(false, None);
            input["conversation"] = conversation;
            let (status, _) = send(&ready, ready.headers.clone(), input).await?;
            ensure!(!status.is_success());
        }
        ensure!(server.state.calls.load(Ordering::SeqCst) == before);
        ensure!(ready.fixture.service.current().is_ok());
        Ok(())
    }
    .await;
    ready.fixture.service.shutdown().await?;
    server.close().await?;
    result
}

#[tokio::test]
async fn cross_protocol_extras_cannot_resume_raw_provider_state() -> Result<()> {
    let server = Server::start().await?;
    let ready = ready(&server).await?;
    let result: Result<()> = async {
        for field in ["previous_response_id", "conversation"] {
            let mut input = json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}]});
            input[field] = "unowned-upstream-state".into();
            let (status, _) =
                send_at(&ready, ready.headers.clone(), "/v1/chat/completions", input).await?;
            ensure!(!status.is_success());
            tokio::time::timeout(WAIT, ready.fixture.service.admission.wait_drained()).await?;
        }
        ensure!(server.state.calls.load(Ordering::SeqCst) == 0);
        ensure!(ready.fixture.service.current().is_ok());
        Ok(())
    }
    .await;
    ready.fixture.service.shutdown().await?;
    server.close().await?;
    result
}
