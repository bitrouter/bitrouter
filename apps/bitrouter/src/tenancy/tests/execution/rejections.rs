//! A rejected provider response must not become permission to try another target
//! when its diagnostic body is truncated or never completes.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use bitrouter_ai::{
    tenancy::{
        authority::{OwnerKind, ResourceKind},
        configuration::ResourceRef,
    },
    types::ApiProtocol,
};
use chrono::Utc;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::{Fixture, MODEL, draft, failure, headers};
use crate::{
    gateway::{
        error::BitrouterError,
        model::client::HttpTimeouts,
        pipeline::{
            executor::HttpExecutor,
            routing::{RoutingPrefs, RoutingTable},
            types::{PipelineInput, PipelineRequest},
        },
    },
    tenancy::{Service, actor::OperationLimits, resources::Resources, store},
};

const CASE_TIMEOUT: Duration = Duration::from_secs(8);
const FIRST_SECRET: &str = "rejection-first-owned-secret";
const SECOND_SECRET: &str = "rejection-second-owned-secret";

#[derive(Clone, Copy, Debug)]
enum BodyMode {
    Truncated,
    Stalled,
    Complete,
}

struct ObservedRequest {
    path: String,
    headers: ::http::HeaderMap,
    body: Value,
}

struct Provider {
    base: String,
    calls: Arc<AtomicUsize>,
    stop: CancellationToken,
    task: Option<JoinHandle<Result<Option<ObservedRequest>>>>,
}

impl Provider {
    async fn start(status: u16, mode: BodyMode) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        // Match the reserved IPv4 listener exactly. The deliberately short
        // response-body timeout must not include localhost's IPv6 fallback.
        let base = format!("http://{}/v1", listener.local_addr()?);
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let task = tokio::spawn(async move {
            let (mut socket, _) = tokio::select! {
                biased;
                _ = stopped.cancelled() => return Ok(None),
                accepted = listener.accept() => accepted?,
            };
            let observed = tokio::time::timeout(CASE_TIMEOUT, read_request(&mut socket))
                .await
                .context("provider did not receive a complete request")??;
            counted.fetch_add(1, Ordering::SeqCst);
            let body = match mode {
                BodyMode::Complete => r#"{"error":"second target rejection"}"#,
                BodyMode::Truncated | BodyMode::Stalled => r#"{"error":"incomplete diagnostic"#,
            };
            let length = body.len()
                + if matches!(mode, BodyMode::Complete) {
                    0
                } else {
                    128
                };
            let response = format!(
                "HTTP/1.1 {status} Rejected\r\ncontent-type: application/json\r\ncontent-length: {length}\r\nconnection: close\r\n\r\n{body}",
            );
            socket.write_all(response.as_bytes()).await?;
            socket.flush().await?;
            if matches!(mode, BodyMode::Stalled) {
                // Keep the unfinished body alive until the caller has returned.
                // Only the fixture executor's read/total timeout can end its read.
                stopped.cancelled().await;
            } else {
                socket.shutdown().await?;
            }
            Ok(Some(observed))
        });
        Ok(Self {
            base,
            calls,
            stop,
            task: Some(task),
        })
    }

    async fn finish(mut self) -> Result<Option<ObservedRequest>> {
        self.stop.cancel();
        let mut task = self.task.take().context("provider task already joined")?;
        let joined = tokio::time::timeout(CASE_TIMEOUT, &mut task).await;
        match joined {
            Ok(result) => result.context("provider task failed")?,
            Err(error) => {
                task.abort();
                let _ = task.await;
                Err(error).context("provider cleanup timed out")
            }
        }
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn read_request(socket: &mut TcpStream) -> Result<ObservedRequest> {
    const MAX_REQUEST: usize = 64 * 1024;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let header_end = loop {
        let read = socket.read(&mut chunk).await?;
        ensure!(read != 0, "request ended before its headers");
        bytes.extend_from_slice(&chunk[..read]);
        ensure!(bytes.len() <= MAX_REQUEST, "request exceeded fixture bound");
        if let Some(index) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let mut lines = std::str::from_utf8(&bytes[..header_end])?.split("\r\n");
    let mut start = lines.next().context("request line")?.split_whitespace();
    ensure!(start.next() == Some("POST"), "expected provider POST");
    let path = start.next().context("request path")?.to_owned();
    let mut headers = ::http::HeaderMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').context("request header")?;
        headers.append(
            ::http::HeaderName::from_bytes(name.as_bytes())?,
            value.trim().parse()?,
        );
    }
    let length: usize = headers
        .get("content-length")
        .context("JSON content length")?
        .to_str()?
        .parse()?;
    ensure!(
        length <= MAX_REQUEST - header_end,
        "JSON exceeded fixture bound"
    );
    let end = header_end + length;
    while bytes.len() < end {
        let read = socket.read(&mut chunk).await?;
        ensure!(read != 0, "request ended before its JSON");
        bytes.extend_from_slice(&chunk[..read]);
        ensure!(bytes.len() <= MAX_REQUEST, "request exceeded fixture bound");
    }
    Ok(ObservedRequest {
        path,
        headers,
        body: serde_json::from_slice(&bytes[header_end..end])?,
    })
}

async fn provision_pair(
    first: &str,
    second: &str,
    protocol: &ApiProtocol,
) -> Result<(Fixture, ResourceRef)> {
    let mut fixture = Fixture::new().await?;
    fixture.service.shutdown().await?;
    let resources = Resources::with_key(fixture.installation.clone(), [43; 32])?;
    let reference = |kind, id: &str| -> Result<ResourceRef> {
        Ok(ResourceRef {
            owner_kind: OwnerKind::Tenant,
            owner_id: fixture.tenant.as_str().try_into()?,
            resource_kind: kind,
            resource_id: id.try_into()?,
            revision_id: "r1".try_into()?,
        })
    };
    let route = reference(ResourceKind::Route, MODEL)?;
    let first_slot = reference(ResourceKind::Credential, "first-slot")?;
    let second_slot = reference(ResourceKind::Credential, "second-slot")?;
    resources.provision_bytes(&fixture.db, &serde_json::to_vec(&json!({
        "format":1, "installation_id":fixture.installation,
        "resources":[
            {"reference":first_slot,"definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":FIRST_SECRET}}}},
            {"reference":second_slot,"definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":SECOND_SECRET}}}},
            {"reference":route,"definition":{"kind":"route","selector":"rejection-chain","policy":null,
                "routing":{
                    "providers":{
                        "rejecting":{"api_base":first,"api_protocol":[{"*":protocol}],"models":[{"id":"model-a"}]},
                        "fallback":{"api_base":second,"api_protocol":[{"*":protocol}],"models":[{"id":"model-a"}]}
                    },
                    "models":{"rejection-chain":{"strategy":"priority","endpoints":[
                        {"provider":"rejecting","service_id":"model-a"},
                        {"provider":"fallback","service_id":"model-a"}
                    ]}}
                },
                "credentials":[
                    {"provider":"rejecting","account":"default","credential":first_slot},
                    {"provider":"fallback","account":"default","credential":second_slot}
                ]}}
        ]
    }))?).await?;
    let snapshot = store::load(&fixture.db, Utc::now()).await?;
    fixture.service = Service::start_prepared(
        fixture.db.clone(),
        None,
        OperationLimits::default(),
        snapshot,
        Arc::new(resources),
    )?;
    fixture.service.executor = Arc::new(HttpExecutor::new(HttpTimeouts {
        connect: Duration::from_secs(2),
        read: Duration::from_millis(250),
        total: Some(Duration::from_secs(3)),
        ..HttpTimeouts::default()
    })?);
    Ok((fixture, route))
}

fn input(protocol: &ApiProtocol, stream: bool) -> Value {
    match protocol {
        ApiProtocol::Decisions => json!({"model":MODEL,"input":"evidence",
            "questions":[{"type":"predicate","name":"q","instructions":"Evaluate the evidence"}]}),
        ApiProtocol::Responses => json!({"model":MODEL,"input":"hello","stream":stream}),
        _ => {
            json!({"model":MODEL,"max_tokens":7,"messages":[{"role":"user","content":"hello"}],"stream":stream})
        }
    }
}

async fn execute(
    fixture: &Fixture,
    route: &ResourceRef,
    protocol: &ApiProtocol,
    stream: bool,
) -> Result<BitrouterError> {
    let revision = draft(fixture, route, "rejection-draft", 25).await?;
    let credential = fixture.issue("rejection-execute").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let headers = headers(
        fixture,
        credential.secret.as_deref().context("execution secret")?,
        &revision.revision_id,
    )?;
    let submission = fixture.service.submission(&headers).map_err(failure)?;
    let execution = fixture
        .service
        .admit_execution(submission, MODEL, protocol)
        .await
        .map_err(failure)?;
    let prefs = RoutingPrefs {
        operation: protocol.operation(),
        inbound_protocol: Some(protocol.clone()),
        ..RoutingPrefs::default()
    };
    let chain = execution
        .route()
        .route_chain(MODEL, &prefs, &execution.caller())
        .await?;
    ensure!(
        chain
            .iter()
            .map(|target| target.provider_name.as_str())
            .collect::<Vec<_>>()
            == ["rejecting", "fallback"],
        "fixture must retain both candidates in explicit priority order"
    );
    let mut request = match crate::gateway::http::parse_input(protocol, input(protocol, stream))? {
        PipelineInput::Generation(prompt) => {
            PipelineRequest::new(MODEL, execution.caller(), *prompt)
        }
        PipelineInput::Decisions(request) => {
            PipelineRequest::new_decisions(MODEL, execution.caller(), request)
        }
    };
    request.request_id = execution.id().to_string();
    request.headers = headers;
    request.inbound_protocol = Some(protocol.clone());
    let pipeline = execution.pipeline()?;
    execution.attach(pipeline.clone(), &mut request)?;
    let result = if stream {
        pipeline.execute_stream(request).await.map(|_| ())
    } else {
        pipeline.execute_detached(request).await.map(|_| ())
    };
    match result {
        Err(error) => Ok(error),
        Ok(()) => anyhow::bail!("rejected upstream unexpectedly established a successful response"),
    }
}

fn check_request(
    request: &ObservedRequest,
    protocol: &ApiProtocol,
    stream: bool,
    secret: &str,
) -> Result<()> {
    let path = match protocol {
        ApiProtocol::Messages => "/v1/messages",
        ApiProtocol::Responses => "/v1/responses",
        ApiProtocol::Decisions => "/v1/decisions",
        _ => "/v1/chat/completions",
    };
    ensure!(request.path == path, "wrong provider wire path");
    ensure!(request.body["model"] == "model-a", "wrong native model");
    if *protocol == ApiProtocol::Messages {
        ensure!(
            request
                .headers
                .get("x-api-key")
                .is_some_and(|value| value.as_bytes() == secret.as_bytes())
        );
        ensure!(!request.headers.contains_key("authorization"));
    } else {
        let expected = format!("Bearer {secret}");
        ensure!(
            request
                .headers
                .get("authorization")
                .is_some_and(|value| value.as_bytes() == expected.as_bytes())
        );
        ensure!(!request.headers.contains_key("x-api-key"));
    }
    if *protocol == ApiProtocol::Decisions {
        ensure!(request.body["questions"][0]["name"] == "q");
    } else {
        ensure!(request.body["stream"] == stream, "wrong streaming mode");
    }
    Ok(())
}

async fn check_case(
    protocol: ApiProtocol,
    stream: bool,
    status: u16,
    mode: BodyMode,
    fallback: bool,
) -> Result<()> {
    let first = Provider::start(status, mode).await?;
    let second = Provider::start(403, BodyMode::Complete).await?;
    let first_calls = first.calls.clone();
    let second_calls = second.calls.clone();
    let (fixture, route) = provision_pair(&first.base, &second.base, &protocol).await?;
    let outcome =
        tokio::time::timeout(CASE_TIMEOUT, execute(&fixture, &route, &protocol, stream)).await;
    // Clean up both sockets and service ownership even if the tested call failed.
    let first_observed = first.finish().await;
    let second_observed = second.finish().await;
    let shutdown = tokio::time::timeout(CASE_TIMEOUT, fixture.service.shutdown()).await;
    let error = outcome.context("tenant invocation exceeded fixture deadline")??;
    let first_observed = first_observed?.with_context(|| {
        format!("first provider received no request; invocation failed: {error:?}")
    })?;
    let second_observed = second_observed?;
    shutdown.context("tenant fixture did not drain")??;
    ensure!(
        first_calls.load(Ordering::SeqCst) == 1,
        "first provider request count"
    );
    ensure!(
        second_calls.load(Ordering::SeqCst) == usize::from(fallback),
        "unexpected fallback request count"
    );
    check_request(&first_observed, &protocol, stream, FIRST_SECRET)?;
    if fallback {
        check_request(
            &second_observed.context("configured fallback was not reachable")?,
            &protocol,
            stream,
            SECOND_SECRET,
        )?;
    } else {
        ensure!(
            second_observed.is_none(),
            "a rejected invocation reached the second provider"
        );
    }
    let expected_status = if fallback { 403 } else { status };
    let BitrouterError::Upstream {
        status: actual,
        message,
    } = &error
    else {
        anyhow::bail!("provider rejection was reclassified: {error:?}");
    };
    ensure!(
        *actual == expected_status,
        "original upstream rejection status was lost"
    );
    if !fallback {
        ensure!(
            message == "upstream error body unavailable",
            "expected unreadable rejection diagnostic"
        );
    }
    // Existing public mapping remains a gateway failure, not a challenge to the
    // caller's tenant credential. The retained upstream status controls fallback.
    ensure!(error.status() == 502);
    Ok(())
}

async fn generation_cases(stream: bool, mode: BodyMode) -> Result<()> {
    for protocol in [
        ApiProtocol::ChatCompletions,
        ApiProtocol::Responses,
        ApiProtocol::Messages,
    ] {
        for status in [401, 403] {
            check_case(protocol.clone(), stream, status, mode, false)
                .await
                .with_context(|| {
                    format!("{protocol:?} stream={stream} status={status} body={mode:?}")
                })?;
        }
    }
    Ok(())
}

async fn decision_cases(mode: BodyMode) -> Result<()> {
    for status in [401, 403] {
        check_case(ApiProtocol::Decisions, false, status, mode, false)
            .await
            .with_context(|| format!("Decisions status={status} body={mode:?}"))?;
    }
    Ok(())
}

#[tokio::test]
async fn truncated_generation_rejections_do_not_fallback() -> Result<()> {
    generation_cases(false, BodyMode::Truncated).await
}

#[tokio::test]
async fn stalled_generation_rejections_do_not_fallback() -> Result<()> {
    generation_cases(false, BodyMode::Stalled).await
}

#[tokio::test]
async fn truncated_decision_rejections_do_not_fallback() -> Result<()> {
    decision_cases(BodyMode::Truncated).await
}

#[tokio::test]
async fn stalled_decision_rejections_do_not_fallback() -> Result<()> {
    decision_cases(BodyMode::Stalled).await
}

#[tokio::test]
async fn truncated_sse_establishment_rejections_do_not_fallback() -> Result<()> {
    generation_cases(true, BodyMode::Truncated).await
}

#[tokio::test]
async fn stalled_sse_establishment_rejections_do_not_fallback() -> Result<()> {
    generation_cases(true, BodyMode::Stalled).await
}

#[tokio::test]
async fn retryable_rejection_keeps_configured_fallback_available() -> Result<()> {
    check_case(
        ApiProtocol::ChatCompletions,
        false,
        503,
        BodyMode::Truncated,
        true,
    )
    .await
}
