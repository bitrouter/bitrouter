//! Exercise configured transport construction without mutating process globals.
use super::*;
use base64::Engine;

const KEY: &str = "BITROUTER_TEST_LIFETIME_WRAPPING_KEY";
const ACK: &str = "BITROUTER_TEST_LIFETIME_ACK";

#[tokio::test]
#[ignore = "child of configured_provider_idle_is_applied_by_production_service"]
async fn configured_provider_idle_child() -> Result<()> {
    use futures::StreamExt;
    let (send, receive) = mpsc::channel::<String>(1);
    let receive = Arc::new(tokio::sync::Mutex::new(Some(receive)));
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let router = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let receive = receive.clone();
            let observed = observed.clone();
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                let Some(mut receive) = receive.lock().await.take() else {
                    return StatusCode::CONFLICT.into_response();
                };
                let body = axum::body::Body::from_stream(async_stream::stream! {
                    while let Some(frame) = receive.recv().await {
                        yield Ok::<_, std::io::Error>(frame);
                    }
                });
                ([("content-type", "text/event-stream")], body).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/v1", listener.local_addr()?);
    let _server =
        AbortOnDropHandle::new(tokio::spawn(
            async move { axum::serve(listener, router).await },
        ));
    let (mut fixture, route, _) = provision_protocol(&url, "chat_completions").await?;
    let revision = draft(&fixture, &route, "configured-idle", 25).await?;
    let credential = fixture.issue("configured-idle-caller").await?;
    fixture.service.shutdown().await?;
    // This is the production config/executor path, not a replaced test executor
    // or a mutation of settings after construction.
    fixture.service = crate::tenancy::Service::open_profile(
        fixture.db.clone(),
        None,
        &crate::config::tenancy::TenancyConfig {
            enabled: true,
            wrapping_key_env: Some(KEY.into()),
            stream_idle_timeout_secs: 2,
            ..Default::default()
        },
        &crate::config::ContinuationConfig::default(),
    )
    .await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let headers = headers(
        &fixture,
        credential.secret.as_deref().context("secret")?,
        &revision.revision_id,
    )?;
    send.send("data: {\"id\":\"chatcmpl-idle\",\"object\":\"chat.completion.chunk\",\"model\":\"model-a\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"first-progress\"},\"finish_reason\":null}]}\n\n".into()).await?;
    let response = tokio::time::timeout(TIMEOUT, call_with(
        fixture.service.clone(), headers,
        json!({"model": MODEL, "messages": [{"role":"user", "content":"hello"}], "stream": true}),
    )?).await?;
    ensure!(response.status() == StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    tokio::time::timeout(TIMEOUT, async {
        let mut seen = Vec::new();
        loop {
            seen.extend_from_slice(&body.next().await.context("initial stream progress")??);
            if String::from_utf8_lossy(&seen).contains("first-progress") {
                break;
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    // Keep the caller actively reading. The stalled provider body must hit the
    // configured two-second read budget, not the output-slot idle watchdog.
    let reader = tokio::spawn(async move {
        let mut bytes = Vec::new();
        while let Some(chunk) = body.next().await {
            bytes.extend_from_slice(&chunk?);
        }
        Ok::<_, anyhow::Error>(bytes)
    });
    tokio::task::yield_now().await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(3)).await;
    tokio::time::resume();
    let bytes = tokio::time::timeout(TIMEOUT, reader).await???;
    // Chat Completions also terminates error streams with [DONE]. Require the
    // error frame itself and reject a successful finish reason.
    let terminal = String::from_utf8_lossy(&bytes);
    ensure!(terminal.contains("\"error\"") && !terminal.contains("\"finish_reason\":\"stop\""));
    tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
    let bytes = fixture
        .service
        .read_receipt(
            fixture.operator.clone(),
            fixture.tenant.clone(),
            read::Request::Page {
                correlation: "same-correlation".try_into()?,
                after: None,
                limit: 100,
            },
        )
        .await
        .map_err(failure)?;
    let page: ExecutionPage = serde_json::from_slice(&bytes)?;
    ensure!(page.execution_ids.len() == 1 && calls.load(Ordering::SeqCst) == 1);
    let receipt = store::lookup(
        &fixture.db,
        &fixture.installation,
        &fixture.tenant,
        &page.execution_ids[0],
    )
    .await?
    .context("durable idle failure")?;
    ensure!(receipt.outcome != Outcome::Completed && receipt.state == ReceiptState::Unknown);
    ensure!(receipt.attempts.len() == 1 && receipt.attempts[0].outcome == Outcome::Unknown);
    drop(send);
    tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await??;
    std::fs::write(
        std::env::var_os(ACK).context("child ACK path")?,
        b"configured-idle-passed",
    )?;
    Ok(())
}

#[tokio::test]
async fn configured_provider_idle_is_applied_by_production_service() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let ack = directory.path().join("idle-passed");
    let status = tokio::time::timeout(Duration::from_secs(40), tokio::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "tenancy::tests::execution::receipts::lifetime::production::configured_provider_idle_child", "--ignored", "--nocapture"])
        .env(KEY, base64::engine::general_purpose::STANDARD.encode([29; 32]))
        .env(ACK, &ack)
        .kill_on_drop(true).status()).await??;
    ensure!(status.success(), "configured idle child failed: {status}");
    ensure!(std::fs::read(ack)? == b"configured-idle-passed");
    Ok(())
}
