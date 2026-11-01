//! Exercise real admission, provider transport, receipt commit and cancellation.
use super::*;
use bitrouter_ai::{
    tenancy::execution::{Outcome, ReceiptState},
    types::ApiProtocol,
};
#[cfg(unix)]
mod production;

#[tokio::test]
async fn unread_durable_json_outlives_settlement_then_idle_or_shutdown_releases_capacity()
-> Result<()> {
    for shutdown in [false, true] {
        let harness = Harness::new(
            "chat_completions",
            StatusCode::OK,
            json_response("chat_completions", full_usage("chat_completions"))?,
        )
        .await?;
        let response = tokio::time::timeout(TIMEOUT, call(&harness)?).await?;
        ensure!(response.status() == StatusCode::OK);
        let page = harness.page().await?;
        ensure!(page.execution_ids.len() == 1);
        let id = &page.execution_ids[0];
        let before = store::lookup(
            &harness.fixture.db,
            &harness.fixture.installation,
            &harness.fixture.tenant,
            id,
        )
        .await?
        .context("receipt must precede JSON delivery")?;
        ensure!(
            before.outcome == Outcome::Completed
                && before.state == ReceiptState::Committed
                && before.usage.complete
                && before.attempts.len() == 1
        );
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(31)).await;
        tokio::time::resume();
        ensure!(
            harness
                .fixture
                .service
                .admission
                .ingress_for_test()
                .map_err(|code| anyhow::anyhow!("{code:?}"))?
                .2
                == 1,
            "test did not retain an unread delivery"
        );
        if shutdown {
            tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await??;
        } else {
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(90)).await;
            tokio::time::resume();
        }
        tokio::time::timeout(TIMEOUT, harness.fixture.service.admission.wait_drained()).await?;
        ensure!(
            harness.fixture.service.admission.receipts_ready(),
            "settled output fenced admission"
        );
        let after = store::lookup(
            &harness.fixture.db,
            &harness.fixture.installation,
            &harness.fixture.tenant,
            id,
        )
        .await?
        .context("durable receipt survived output cancellation")?;
        ensure!(serde_json::to_value(before)? == serde_json::to_value(after)?);
        ensure!(harness.calls.load(Ordering::SeqCst) == 1);
        ensure!(
            axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await?
                .is_empty()
        );
        if !shutdown {
            // Prove both receipt admission and the registry slot remain usable.
            let next = tokio::time::timeout(TIMEOUT, call(&harness)?).await?;
            ensure!(next.status() == StatusCode::OK);
            ensure!(
                !axum::body::to_bytes(next.into_body(), 64 * 1024)
                    .await?
                    .is_empty()
            );
        }
        tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await??;
    }
    Ok(())
}

#[cfg(all(unix, feature = "core", feature = "harness"))]
#[tokio::test]
async fn genuine_cleanup_expiry_retains_custody_until_confirmed_late_join() -> Result<()> {
    let harness = Harness::new(
        "chat_completions",
        StatusCode::OK,
        json_response("chat_completions", full_usage("chat_completions"))?,
    )
    .await?;
    let response = tokio::time::timeout(TIMEOUT, call(&harness)?).await?;
    let page = harness.page().await?;
    ensure!(page.execution_ids.len() == 1);
    let (owner, tasks) = harness
        .fixture
        .service
        .pipelines
        .callback_owner_for_test(&page.execution_ids[0])?;
    let gate = Arc::new(Semaphore::new(0));
    let held = gate.clone();
    let (stopped, joined) = tokio::sync::oneshot::channel();
    tasks.spawn_settlement(async move {
        if let Ok(permit) = held.acquire().await {
            permit.forget();
        }
        let _ = stopped.send(());
    });
    owner.cancel();
    let deadline = owner.cancellation_deadline();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::time::resume();
    ensure!(
        tokio::time::timeout(TIMEOUT, harness.fixture.service.pipelines.close_and_drain())
            .await?
            .is_err()
    );
    ensure!(!harness.fixture.service.admission.receipts_ready());
    ensure!(owner.durable() && owner.cancellation_deadline() == deadline);
    ensure!(
        harness
            .fixture
            .service
            .admission
            .ingress_for_test()
            .map_err(|code| anyhow::anyhow!("{code:?}"))?
            .2
            == 1
    );
    gate.add_permits(1);
    // Join the test producer before retrying the expired deadline. No new
    // grace is granted, and only an actually ready drain can release the lease.
    tokio::time::timeout(TIMEOUT, joined).await??;
    tokio::time::timeout(
        TIMEOUT,
        harness
            .fixture
            .service
            .pipelines
            .join_pipeline_for_test(&page.execution_ids[0]),
    )
    .await??;
    tokio::time::timeout(TIMEOUT, harness.fixture.service.pipelines.close_and_drain()).await??;
    tokio::time::timeout(TIMEOUT, harness.fixture.service.admission.wait_drained()).await?;
    ensure!(owner.cancellation_deadline() == deadline);
    ensure!(
        !harness.fixture.service.admission.receipts_ready(),
        "genuine failure lost its fence"
    );
    drop(response);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

fn call(harness: &Harness) -> Result<impl std::future::Future<Output = Response> + Send + 'static> {
    call_with(
        harness.fixture.service.clone(),
        harness.headers.clone(),
        harness.request.clone(),
    )
}

fn call_with(
    service: crate::tenancy::Service,
    headers: HeaderMap,
    request: Value,
) -> Result<impl std::future::Future<Output = Response> + Send + 'static> {
    let submission = service.submission(&headers).map_err(failure)?;
    let input = crate::gateway::http::parse_input(&ApiProtocol::ChatCompletions, request.clone())?;
    let ingress = service
        .admission
        .ingress(serde_json::to_vec(&request)?.len())
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    Ok(async move {
        crate::gateway::http::handle_tenant(
            &service,
            submission,
            headers,
            ApiProtocol::ChatCompletions,
            input,
            ingress,
        )
        .await
    })
}

async fn gated() -> Result<(Harness, Arc<Semaphore>)> {
    let gate = Arc::new(Semaphore::new(0));
    let harness = Harness::with_reply(
        "chat_completions",
        StatusCode::OK,
        serde_json::to_string(&json_response(
            "chat_completions",
            full_usage("chat_completions"),
        )?)?,
        false,
        Some(gate.clone()),
    )
    .await?;
    Ok((harness, gate))
}

#[tokio::test]
async fn admitted_completion_delivers_after_caller_revoke_but_cannot_submit_again() -> Result<()> {
    let (mut harness, gate) = gated().await?;
    let task = AbortOnDropHandle::new(tokio::spawn(call(&harness)?));
    tokio::time::timeout(TIMEOUT, harness.received.recv())
        .await?
        .context("provider accepted the admitted request")?;
    harness
        .fixture
        .service
        .execute(
            harness.fixture.operator.clone(),
            crate::tenancy::command::Command::Revoke(
                harness.fixture.tenant.clone(),
                harness.credential.credential_id.clone(),
                bitrouter_ai::tenancy::operation::Mutation {
                    operation_id: "completion-revoke-caller".try_into()?,
                    expected_revision: harness.credential.credential_revision.clone(),
                    installation_id: harness.fixture.installation.clone(),
                },
            ),
        )
        .await
        .map_err(failure)?;
    gate.add_permits(1);
    let response = tokio::time::timeout(TIMEOUT, task).await??;
    ensure!(
        response.status() == StatusCode::OK,
        "paid completion required a new caller grant"
    );
    let id = response
        .headers()
        .get("x-bitrouter-request-id")
        .context("execution identity")?
        .to_str()?
        .try_into()?;
    let body: Value =
        serde_json::from_slice(&axum::body::to_bytes(response.into_body(), 64 * 1024).await?)?;
    ensure!(body["choices"][0]["message"]["content"] == "receipt reply");
    let receipt = store::lookup(
        &harness.fixture.db,
        &harness.fixture.installation,
        &harness.fixture.tenant,
        &id,
    )
    .await?
    .context("receipt before successful delivery")?;
    ensure!(receipt.outcome == Outcome::Completed && receipt.state == ReceiptState::Committed);
    ensure!(
        harness
            .fixture
            .service
            .submission(&harness.headers)
            .is_err_and(|failure| failure.0.code
                == bitrouter_ai::tenancy::operation::ErrorCode::Unauthorized)
    );
    ensure!(harness.calls.load(Ordering::SeqCst) == 1);
    tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await??;
    Ok(())
}

#[tokio::test]
async fn active_stream_progress_survives_multiple_submission_windows() -> Result<()> {
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
    let (fixture, route, _) = provision_protocol(&url, "chat_completions").await?;
    let revision = draft(&fixture, &route, "long-stream", 25).await?;
    let credential = fixture.issue("long-stream-caller").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let headers = headers(
        &fixture,
        credential.secret.as_deref().context("secret")?,
        &revision.revision_id,
    )?;
    send.send("data: {\"id\":\"chatcmpl-receipts\",\"object\":\"chat.completion.chunk\",\"model\":\"model-a\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n".into()).await?;
    let task = tokio::spawn(call_with(
        fixture.service.clone(),
        headers,
        json!({
            "model": MODEL, "messages": [{"role": "user", "content": "hello"}], "stream": true
        }),
    )?);
    let response = tokio::time::timeout(TIMEOUT, task).await??;
    ensure!(response.status() == StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    for index in 0..4 {
        let marker = format!("progress-{index}");
        let frame = json!({"id":"chatcmpl-receipts", "object":"chat.completion.chunk", "model":"model-a",
            "choices":[{"index":0,"delta":{"content":marker},"finish_reason":null}]});
        send.send(format!("data: {frame}\n\n")).await?;
        tokio::time::timeout(TIMEOUT, async {
            let mut bytes = Vec::new();
            loop {
                bytes.extend_from_slice(
                    &body
                        .next()
                        .await
                        .context("stream ended before progress")??,
                );
                if String::from_utf8_lossy(&bytes).contains(&marker) {
                    break;
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(40)).await;
        tokio::time::resume();
    }
    let terminal = super::wire::chat_stream(full_usage("chat_completions"), true)?;
    send.send(
        terminal
            .split_once("\n\n")
            .context("terminal frames")?
            .1
            .to_owned(),
    )
    .await?;
    drop(send);
    let mut terminal_bytes = Vec::new();
    tokio::time::timeout(TIMEOUT, async {
        while let Some(chunk) = body.next().await {
            terminal_bytes.extend_from_slice(&chunk?);
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    ensure!(String::from_utf8_lossy(&terminal_bytes).contains("[DONE]"));
    let caller = fixture
        .service
        .authenticate(credential.secret.as_deref().context("secret")?)
        .map_err(failure)?;
    let bytes = fixture
        .service
        .read_receipt(
            caller,
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
    .context("durable stream receipt")?;
    ensure!(receipt.outcome == Outcome::Completed && receipt.attempts.len() == 1);
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn configured_and_default_execution_lifetimes_accept_provider_work_after_sixty_seconds()
-> Result<()> {
    for total in [None, Some(180)] {
        let (mut harness, gate) = gated().await?;
        harness
            .fixture
            .service
            .execution_settings
            .execution_timeout_secs = total;
        let task = tokio::spawn(call(&harness)?);
        tokio::time::timeout(TIMEOUT, harness.received.recv())
            .await?
            .context("provider request")?;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(75)).await;
        tokio::time::resume();
        ensure!(
            !task.is_finished(),
            "active execution ended at the submission deadline"
        );
        gate.add_permits(1);
        let response = tokio::time::timeout(TIMEOUT, task).await??;
        ensure!(response.status() == StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024).await?;
        ensure!(!bytes.is_empty());
        let page = harness.page().await?;
        ensure!(page.execution_ids.len() == 1 && harness.calls.load(Ordering::SeqCst) == 1);
        let receipt = harness
            .receipt(&Harness::client(false)?, &page.execution_ids[0])
            .await?;
        ensure!(receipt.outcome == Outcome::Completed && receipt.state != ReceiptState::Unknown);
        tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await??;
    }
    Ok(())
}

#[tokio::test]
async fn total_expiry_cancels_an_armed_request_without_success_or_replay() -> Result<()> {
    let (mut harness, gate) = gated().await?;
    harness
        .fixture
        .service
        .execution_settings
        .execution_timeout_secs = Some(2);
    let task = tokio::spawn(call(&harness)?);
    tokio::time::timeout(TIMEOUT, harness.received.recv())
        .await?
        .context("provider request")?;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(3)).await;
    tokio::time::resume();
    let response = tokio::time::timeout(TIMEOUT, task).await??;
    ensure!(response.status() != StatusCode::OK);
    tokio::time::timeout(TIMEOUT, harness.fixture.service.admission.wait_drained()).await?;
    let page = harness.page().await?;
    ensure!(page.execution_ids.len() == 1);
    let receipt = harness
        .receipt(&Harness::client(false)?, &page.execution_ids[0])
        .await?;
    ensure!(receipt.state == ReceiptState::Unknown && receipt.outcome != Outcome::Completed);
    ensure!(receipt.attempts.len() == 1 && receipt.attempts[0].outcome == Outcome::Unknown);
    ensure!(harness.calls.load(Ordering::SeqCst) == 1);
    gate.add_permits(1);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn disconnected_waiter_without_total_cancels_before_provider_headers() -> Result<()> {
    let (mut harness, gate) = gated().await?;
    let task = tokio::spawn(call(&harness)?);
    tokio::time::timeout(TIMEOUT, harness.received.recv())
        .await?
        .context("provider request")?;
    task.abort();
    ensure!(task.await.is_err_and(|error| error.is_cancelled()));
    tokio::time::timeout(TIMEOUT, harness.fixture.service.admission.wait_drained()).await?;
    let page = harness.page().await?;
    ensure!(page.execution_ids.len() == 1);
    let receipt = harness
        .receipt(&Harness::client(false)?, &page.execution_ids[0])
        .await?;
    ensure!(receipt.state == ReceiptState::Unknown && receipt.outcome != Outcome::Completed);
    ensure!(harness.calls.load(Ordering::SeqCst) == 1 && gate.available_permits() == 0);
    gate.add_permits(1);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn no_total_deadline_still_releases_an_unpolled_stream_on_output_idle() -> Result<()> {
    let mut harness = Harness::with_reply(
        "chat_completions",
        StatusCode::OK,
        super::wire::chat_stream(full_usage("chat_completions"), true)?,
        true,
        None,
    )
    .await?;
    harness
        .fixture
        .service
        .execution_settings
        .stream_idle_timeout_secs = 2;
    let response = tokio::time::timeout(TIMEOUT, call(&harness)?).await?;
    ensure!(response.status() == StatusCode::OK);
    tokio::time::timeout(TIMEOUT, harness.fixture.service.admission.wait_drained()).await?;
    ensure!(
        axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await?
            .is_empty()
    );
    let page = harness.page().await?;
    ensure!(page.execution_ids.len() == 1 && harness.calls.load(Ordering::SeqCst) == 1);
    let receipt = harness
        .receipt(&Harness::client(false)?, &page.execution_ids[0])
        .await?;
    ensure!(receipt.outcome != Outcome::Completed);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn shutdown_cancels_unlimited_execution_and_reopen_retains_unknown_receipt() -> Result<()> {
    let (mut harness, gate) = gated().await?;
    let task = tokio::spawn(call(&harness)?);
    tokio::time::timeout(TIMEOUT, harness.received.recv())
        .await?
        .context("provider request")?;
    tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await??;
    let response = tokio::time::timeout(TIMEOUT, task).await??;
    ensure!(response.status() != StatusCode::OK);
    let snapshot = crate::tenancy::store::load(&harness.fixture.db, chrono::Utc::now()).await?;
    let reopened = crate::tenancy::Service::start_prepared(
        harness.fixture.db.clone(),
        None,
        Default::default(),
        snapshot,
        harness.fixture.service.resources.clone(),
    )
    .await?;
    let caller = reopened
        .authenticate(harness.credential.secret.as_deref().context("credential")?)
        .map_err(failure)?;
    let bytes = reopened
        .read_receipt(
            caller,
            harness.fixture.tenant.clone(),
            read::Request::Page {
                correlation: "same-correlation".try_into()?,
                after: None,
                limit: 100,
            },
        )
        .await
        .map_err(failure)?;
    let page: ExecutionPage = serde_json::from_slice(&bytes)?;
    ensure!(page.execution_ids.len() == 1);
    let receipt = store::lookup(
        &harness.fixture.db,
        &harness.fixture.installation,
        &harness.fixture.tenant,
        &page.execution_ids[0],
    )
    .await?
    .context("recovered receipt")?;
    ensure!(receipt.state == ReceiptState::Unknown && receipt.outcome != Outcome::Completed);
    ensure!(receipt.attempts.len() == 1 && receipt.attempts[0].outcome == Outcome::Unknown);
    ensure!(harness.calls.load(Ordering::SeqCst) == 1 && gate.available_permits() == 0);
    reopened.shutdown().await?;
    gate.add_permits(1);
    Ok(())
}
