use super::*;

async fn restart(mut h: OrderedHarness) -> Result<OrderedHarness> {
    h._gateway.abort();
    let stopped = tokio::time::timeout(TIMEOUT, &mut h._gateway).await?;
    ensure!(stopped.is_err_and(|error| error.is_cancelled()));
    let Fixture {
        service,
        db,
        operator,
        installation,
        tenant,
        principal,
        binding,
        binding_result,
    } = h.fixture;
    service.shutdown().await?;
    drop(service);
    // The pool retains the actual file lease beyond Service::shutdown. Close
    // and drop every old owner before acquiring a genuinely new instance.
    tokio::time::timeout(TIMEOUT, db.close()).await??;
    let url = format!(
        "sqlite://{}",
        h._directory.path().join("gateway.sqlite").display()
    );
    let (db, owner, claim) = tokio::time::timeout(TIMEOUT, async {
        loop {
            match crate::db::connect_owned_tenancy(&url).await {
                Ok(database) => break database.claim(),
                Err(error)
                    if matches!(
                        error.downcast_ref::<std::fs::TryLockError>(),
                        Some(std::fs::TryLockError::WouldBlock)
                    ) =>
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => break Err(error),
            }
        }
    })
    .await??;
    let resources = crate::tenancy::resources::Resources::with_key(installation.clone(), [61; 32])?;
    let initial = crate::tenancy::store::load(&db, chrono::Utc::now()).await?;
    let service = crate::tenancy::Service::start_resources_with_continuations(
        db.clone(),
        Some(owner),
        Default::default(),
        initial,
        Arc::new(resources),
        (
            None,
            &crate::config::ContinuationConfig::default(),
            Some(claim),
            crate::telemetry::receipt_latency::Sink::default(),
        ),
        None,
    )
    .await?;
    h.fixture = Fixture {
        service,
        db,
        operator,
        installation,
        tenant,
        principal,
        binding,
        binding_result,
    };
    h.fixture.service.execution_settings.ordered_native_controls = true;
    h.fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let auth = crate::remote_control::auth::ControlAuth::from_lookup(
        &crate::config::ControlConfig {
            credentials: vec![crate::config::ControlCredentialConfig {
                // Restart retains the resource-owning installation operator;
                // only the scoped caller credential rotates in this test.
                id: "test-operator".into(),
                token_env: "NATIVE_TEST_OPERATOR".into(),
                scopes: vec![ControlScope::Tenancy],
            }],
            ..Default::default()
        },
        |_| Some(OPERATOR.into()),
    )?;
    let dispatch = Router::new()
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(axum::middleware::from_fn_with_state(
            h.fixture.service.clone(),
            crate::tenancy::http::dispatch,
        ));
    let app = crate::tenancy::http::router(h.fixture.service.clone(), auth).merge(dispatch);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    h.endpoint = format!("http://{}", listener.local_addr()?);
    h._gateway = AbortOnDropHandle::new(tokio::spawn(
        async move { axum::serve(listener, app).await },
    ));
    Ok(h)
}

#[tokio::test]
async fn native_current_caller_rotation_restart_and_new_defaults_preserve_only_current_authority()
-> Result<()> {
    let mut h = harness(&["responses"], true, None).await?;
    h.controls.stateful.store(true, Ordering::SeqCst);
    h.revision = draft_with_policy(&h.fixture, &h.route, "native-config-32", 32, None).await?;
    let original = h.binding(vec![0], GenerationProtocol::Responses).await?;
    invoke(
        &h,
        &original,
        GenerationProtocol::Responses,
        json!({"input":"remember:violet"}),
        None,
    )
    .await?;
    let old_headers = caller_timeout::ordered_headers(&h, &original)?;
    let mut rotation = h.fixture.issuance("native-rotate")?;
    rotation.rotate_credential_id = Some(h.credential.credential_id.clone());
    let rotated = h
        .control(true, HttpVersion::Http1Only)?
        .issue_credential(&h.fixture.tenant, &rotation)
        .await?;
    ensure!(rotated.credential_id != h.credential.credential_id);
    h.credential = rotated;
    let mut headers = old_headers;
    headers.insert(HEADER, PROVIDER_NATIVE_V1.parse()?);
    let denied = raw(
        &h,
        headers,
        json!({"model":MODEL,"input":"recall","previous_response_id":"resp_receipts"}),
    )
    .await?;
    ensure!(denied.status() == StatusCode::UNAUTHORIZED && h.observed()?.len() == 1);
    h.revision = draft_with_policy(&h.fixture, &h.route, "native-config-17", 17, None).await?;
    ensure!(h.revision.revision_id != original.execution_revision);
    h = restart(h)
        .await
        .context("restart after releasing all old owners")?;
    let binding = h
        .binding(vec![0], GenerationProtocol::Responses)
        .await
        .context("observe current configuration with the original installation operator")?;
    ensure!(binding.instance_id != original.instance_id);
    let (id, reply) = invoke(
        &h,
        &binding,
        GenerationProtocol::Responses,
        json!({"input":"recall","previous_response_id":"resp_receipts"}),
        None,
    )
    .await
    .context("provider-native followup under rotated caller after restart")?;
    ensure!(reply.contains("violet") && h.receipt(&id).await?.outcome == Outcome::Completed);
    let captured = bodies(&h)?;
    ensure!(
        captured.len() == 2
            && captured[0]["max_output_tokens"] == 32
            && captured[1]["max_output_tokens"] == 17
    );
    ensure!(!captured[1].to_string().contains("violet"));
    // Each new invocation also binds its own request-local defaults digest.
    // Neither the old configuration nor the previous envelope is an affinity key.
    for tokens in [32, 17] {
        let envelope = defaults(tokens, json!({}))?;
        let bound = binding
            .clone()
            .with_generation_defaults_envelope_v2(&envelope)?;
        let (id, reply) = invoke(
            &h,
            &bound,
            GenerationProtocol::Responses,
            json!({"input":"recall","previous_response_id":"resp_receipts"}),
            Some(envelope),
        )
        .await?;
        ensure!(reply.contains("violet") && h.receipt(&id).await?.outcome == Outcome::Completed);
    }
    let captured = bodies(&h)?;
    ensure!(
        captured.len() == 4
            && captured[2]["max_output_tokens"] == 32
            && captured[3]["max_output_tokens"] == 17
    );
    // Raw IDs grant no access after the live route/credential scope changes.
    authorization::narrow_binding(&h, &[]).await?;
    let mut headers = caller_timeout::ordered_headers(&h, &binding)?;
    headers.insert(HEADER, PROVIDER_NATIVE_V1.parse()?);
    let denied = raw(
        &h,
        headers,
        json!({"model":MODEL,"input":"recall","previous_response_id":"resp_receipts"}),
    )
    .await?;
    ensure!(denied.status() == StatusCode::FORBIDDEN && h.observed()?.len() == 4);
    h.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn native_partial_stream_cancellation_never_replays_or_reaches_the_next_candidate()
-> Result<()> {
    let h = harness(&["responses", "responses"], true, None).await?;
    let binding = h.binding(vec![0, 1], GenerationProtocol::Responses).await?;
    let gate = Arc::new(barrier::Gate {
        entered: tokio::sync::Notify::new(),
        release: Semaphore::new(0),
    });
    *h.controls
        .held_stream
        .lock()
        .map_err(|_| anyhow::anyhow!("stream gate"))? = Some(gate.clone());
    let caps = h
        .control(false, HttpVersion::Http1Only)?
        .ordered_native_controls_capabilities()
        .await?;
    let client = GatewayClient::new(&h.endpoint, None)?;
    let reply = client
        .ordered_tenant(h.token()?, &binding)?
        .with_native_controls(&caps)?
        .responses_reply(&serde_json::from_value(
            json!({"model":MODEL,"input":"next",
            "previous_response_id":"opaque","stream":true}),
        )?)
        .await?;
    let id = reply.metadata.execution_id.context("execution")?;
    let Body::Stream(mut stream) = reply.body else {
        anyhow::bail!("expected SSE");
    };
    ensure!(stream.next().await.context("first event")?.is_ok());
    tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
    drop(stream);
    tokio::time::timeout(TIMEOUT, h.fixture.service.admission.wait_drained()).await?;
    let receipt = h.receipt(&id).await?;
    ensure!(receipt.state == ReceiptState::Unknown && receipt.outcome == Outcome::Unknown);
    ensure!(receipt.attempts.len() == 1 && receipt.attempts[0].candidate_index == 0);
    ensure!(h.observed()? == ["model-0"] && gate.release.available_permits() == 0);
    gate.release.close();
    h.fixture.service.shutdown().await?;
    Ok(())
}
