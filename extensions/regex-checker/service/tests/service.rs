use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use bitrouter_checker_protocol::capability;
use bitrouter_checker_protocol::v1::{
    self, CheckerBinding, ContentFragment, ContentFragmentKind, ContentRole, Coverage,
    CoverageScope, CoverageStatus, Decision,
};
use bitrouter_guardrails::checker;
use bitrouter_guardrails::config::{InputAction, InputGuardrailConfig, InputRuleSpec, InputScope};
use bitrouter_regex_checker::adapter;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn invocation(fragments: &[&str]) -> v1::Request {
    let content = fragments
        .iter()
        .map(|text| ContentFragment {
            role: ContentRole::User,
            kind: ContentFragmentKind::Text,
            text: Some((*text).to_owned()),
        })
        .collect::<Vec<_>>();
    let text_bytes = fragments
        .iter()
        .fold(0_u64, |total, text| total.saturating_add(text.len() as u64));
    v1::Request {
        contract_version: v1::CONTRACT_VERSION,
        invocation_id: "invocation-1".to_owned(),
        request_id: "request-1".to_owned(),
        router_id: "coding".to_owned(),
        router_binding_digest: "router-digest".to_owned(),
        checker: CheckerBinding {
            checker_id: "guardrails".to_owned(),
            binding_digest: "checker-digest".to_owned(),
            max_input_bytes: text_bytes,
            timeout_ms: 500,
        },
        content,
        coverage: Coverage {
            scope: CoverageScope::EntryRequestText,
            text_bytes,
            text_fragments: fragments.len() as u64,
            excluded_media_fragments: 0,
            status: CoverageStatus::CompleteWithinScope,
        },
    }
}

async fn start(
    pattern: &str,
    credential: Option<adapter::BearerCredential>,
) -> Result<(String, tokio::task::JoinHandle<()>), Box<dyn std::error::Error>> {
    let config = InputGuardrailConfig {
        scope: InputScope::Input,
        rules: vec![InputRuleSpec {
            name: "private-rule".to_owned(),
            pattern: pattern.to_owned(),
            action: InputAction::Block,
        }],
    };
    let app = adapter::router(
        checker::callback(config.compile()?),
        credential,
        "fixture-v1".to_owned(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{address}/check"), task))
}

#[tokio::test]
async fn actual_http_service_preserves_ordered_newline_boundaries()
-> Result<(), Box<dyn std::error::Error>> {
    let (endpoint, server) = start(r"for\nbidden\n", None).await?;
    let request = invocation(&["for", "bidden"]);
    let encoded = v1::encode_request(&request)?;
    let response = reqwest::Client::new()
        .post(endpoint)
        .header("content-type", "application/json")
        .body(encoded)
        .send()
        .await?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body = response.bytes().await?;
    let decoded = v1::decode_response(&request.invocation_id, &body)?;
    assert_eq!(decoded.decision(), Decision::Deny);
    assert_eq!(decoded.reason_code(), Some("guardrail.input_blocked"));
    assert_eq!(decoded.implementation_version(), Some("fixture-v1"));
    assert!(!String::from_utf8_lossy(&body).contains("private-rule"));
    assert!(!String::from_utf8_lossy(&body).contains("forbidden"));
    server.abort();
    Ok(())
}

#[tokio::test]
async fn actual_http_service_authenticates_and_allows_clean_input()
-> Result<(), Box<dyn std::error::Error>> {
    let credential = adapter::BearerCredential::new("test-token".to_owned())?;
    let (endpoint, server) = start("blocked", Some(credential)).await?;
    let request = invocation(&["clean"]);
    let encoded = Arc::new(v1::encode_request(&request)?);
    let client = reqwest::Client::new();

    let unauthorized = client
        .post(&endpoint)
        .body(encoded.as_ref().clone())
        .send()
        .await?;
    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);
    let unauthorized_body = unauthorized.text().await?;
    assert!(!unauthorized_body.contains("test-token"));

    let allowed = client
        .post(endpoint)
        .header("authorization", "Bearer test-token")
        .body(encoded.as_ref().clone())
        .send()
        .await?;
    assert_eq!(allowed.status(), reqwest::StatusCode::OK);
    let response = v1::decode_response(&request.invocation_id, &allowed.bytes().await?)?;
    assert_eq!(response.decision(), Decision::Allow);
    server.abort();
    Ok(())
}

#[tokio::test]
async fn malformed_and_oversized_requests_never_reach_callback()
-> Result<(), Box<dyn std::error::Error>> {
    let called = Arc::new(AtomicBool::new(false));
    let callback_called = called.clone();
    let callback: Arc<capability::CheckCallback> = Arc::new(move |_| {
        callback_called.store(true, Ordering::Relaxed);
        capability::CheckDecision::Allow
    });
    let app = adapter::router(callback, None, "fixture-v1".to_owned());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let client = reqwest::Client::new();

    let malformed = client
        .post(format!("http://{address}/check"))
        .body(br#"{"contract_version":1,"unknown":"private-text"}"#.to_vec())
        .send()
        .await?;
    assert_eq!(malformed.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(!malformed.text().await?.contains("private-text"));

    let oversized = client
        .post(format!("http://{address}/check"))
        .body(vec![b'x'; v1::MAX_REQUEST_BYTES + 1])
        .send()
        .await?;
    assert_eq!(oversized.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
    assert!(!called.load(Ordering::Relaxed));
    server.abort();
    Ok(())
}

#[tokio::test]
async fn saturation_rejects_before_body_and_retains_cancelled_work_permits()
-> Result<(), Box<dyn std::error::Error>> {
    let entered = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Barrier::new(adapter::MAX_CONCURRENT_CHECKS + 1));
    let callback_entered = entered.clone();
    let callback_completed = completed.clone();
    let callback_release = release.clone();
    let callback: Arc<capability::CheckCallback> = Arc::new(move |_| {
        callback_entered.fetch_add(1, Ordering::SeqCst);
        callback_release.wait();
        callback_completed.fetch_add(1, Ordering::SeqCst);
        capability::CheckDecision::Allow
    });
    let app = adapter::router(callback, None, "fixture-v1".to_owned());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let request = invocation(&["clean"]);
    let body = Arc::new(v1::encode_request(&request)?);
    let client = reqwest::Client::new();
    let endpoint = format!("http://{address}/check");
    let mut callers = Vec::with_capacity(adapter::MAX_CONCURRENT_CHECKS);
    for _ in 0..adapter::MAX_CONCURRENT_CHECKS {
        let client = client.clone();
        let endpoint = endpoint.clone();
        let body = body.clone();
        callers.push(tokio::spawn(async move {
            client
                .post(endpoint)
                .body(body.as_ref().clone())
                .send()
                .await
        }));
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while entered.load(Ordering::SeqCst) < adapter::MAX_CONCURRENT_CHECKS {
            tokio::task::yield_now().await;
        }
    })
    .await?;

    // Dropping all HTTP callers must not release CPU permits owned by matching
    // callbacks that are already running.
    for caller in &callers {
        caller.abort();
    }
    tokio::task::yield_now().await;

    // Send headers declaring a body but deliberately withhold the body. A 503
    // proves saturation is checked before request-body buffering.
    let mut stream = tokio::net::TcpStream::connect(address).await?;
    let headers =
        format!("POST /check HTTP/1.1\r\nHost: {address}\r\nContent-Length: 1024\r\n\r\n");
    stream.write_all(headers.as_bytes()).await?;
    let mut response = [0_u8; 256];
    let read = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut response)).await??;
    assert!(String::from_utf8_lossy(&response[..read]).starts_with("HTTP/1.1 503"));

    release.wait();
    tokio::time::timeout(Duration::from_secs(10), async {
        while completed.load(Ordering::SeqCst) < adapter::MAX_CONCURRENT_CHECKS {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    server.abort();
    Ok(())
}
