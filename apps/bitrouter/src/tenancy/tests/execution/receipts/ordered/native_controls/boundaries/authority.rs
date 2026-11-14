use super::*;

/// Uses the same real sender, original issuance and original actor transaction.
/// No manual authority/receipt write and no replacement Proof participates.
async fn rotation_with_optional_intent_order(hold_optional_during_intent: bool) -> Result<()> {
    use crate::tenancy::actor::provider_technical::tests::RotationOverlap;
    use sea_orm::{ConnectionTrait, Statement};

    let h = harness(&["responses"], true, None).await?;
    h.controls.stateful.store(true, Ordering::SeqCst);
    let original = h.binding(vec![0], GenerationProtocol::Responses).await?;
    let old_headers = caller_timeout::ordered_headers(&h, &original)?;
    let mut input = h.fixture.issuance("native-rotate-overlap").await?;
    input.rotate_credential_id = Some(h.credential.credential_id.clone());
    let before = crate::tenancy::store::startup::read_header_in(&h.fixture.db, 4096)
        .await?
        .context("rotation baseline header")?
        .header;
    let mode = h
        .fixture
        .db
        .query_one(Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "PRAGMA journal_mode",
        ))
        .await?
        .context("rotation journal mode")?
        .try_get::<String>("", "journal_mode")?;
    ensure!(mode == "wal", "rotation overlap requires actual WAL");

    let end = tokio::time::Instant::now() + TIMEOUT;
    let probe = RotationOverlap::new(
        h.fixture.installation.clone(),
        input.operation_id.clone(),
        end.min(tokio::time::Instant::now() + crate::tenancy::MANAGEMENT_TIMEOUT),
    );
    h.fixture
        .service
        .technical_io
        .arm_rotation_overlap(probe.clone())?;
    h.fixture
        .service
        .cleanup
        .install_rotation_overlap_for_test(probe.clone())?;
    // Every early error/unwind releases BOTH holds before shutdown. The native
    // callback also has the same original five-second bound, not a new window.
    let release = (
        probe.commit_release.clone().drop_guard(),
        probe.header_release.clone().drop_guard(),
    );
    let checked = tokio::time::timeout_at(probe.deadline, async {
        eprintln!("rotation_overlap stage=sender_start");
        let (id, _) = invoke(
            &h,
            &original,
            GenerationProtocol::Responses,
            json!({"input":"remember:violet"}),
            None,
        )
        .await?;
        eprintln!("rotation_overlap stage=sender_complete");
        probe.commit_entered.cancelled().await;
        eprintln!("rotation_overlap stage=optional_commit_entered");
        ensure!(probe.execution()? == id, "optional worker execution mismatch");

        let proof = &h.fixture.service.cleanup;
        let (entered, acquired) = proof.write_intent_counts_for_test();
        let client = h.control(true, HttpVersion::Http1Only)?;
        let rotation = client.issue_credential(&h.fixture.tenant, &input);
        tokio::pin!(rotation);
        if hold_optional_during_intent {
            tokio::select! {biased;
                () = probe.target_store_entered.cancelled() => {},
                _ = &mut rotation => anyhow::bail!("rotation completed before its actual store entry"),
            }
            // This exact operation is now inside the original serial actor's
            // store call. No maintenance job can own the following counter.
            tokio::select! {biased;
                result = proof.wait_write_intent_entered_for_test(entered) => result?,
                _ = &mut rotation => anyhow::bail!("rotation completed before write-intent entry"),
            }
            let counts = proof.write_intent_counts_for_test();
            ensure!(counts.0 > entered && counts.1 == acquired);
            ensure!(
                !probe.header_entered.is_cancelled()
                    && !probe.commit_release.is_cancelled()
                    && !probe.commit_expired.load(Ordering::Acquire),
                "main reached snapshot/acquisition while optional commit was held"
            );
            // entered means real BEGIN/entry to DML, not a SQLite lock ACK.
            // There is no sleep, optional-ACK join or new exclusion mechanism.
            eprintln!("rotation_overlap stage=intent_entered_before_snapshot");
            probe.commit_release.cancel();
            proof
                .wait_write_intent_acquired_for_test(acquired)
                .await?;
            probe.header_entered.cancelled().await;
            ensure!(proof.write_intent_counts_for_test().1 > acquired);
            eprintln!("rotation_overlap stage=intent_acquired_then_authority_snapshot");
            h.fixture.service.technical_io.wait(&id).await?;
            eprintln!("rotation_overlap stage=optional_worker_closed");
            probe.header_release.cancel();
        } else {
            // Control: the SAME worker commits/closes before rotation is polled.
            probe.commit_release.cancel();
            h.fixture.service.technical_io.wait(&id).await?;
            eprintln!("rotation_overlap stage=optional_worker_closed");
            probe.header_release.cancel();
        }
        let rotated = rotation.await;
        let observed = probe.outcome()?;
        let after = crate::tenancy::store::startup::read_header_in(&h.fixture.db, 4096)
            .await?
            .context("rotation final header")?
            .header;
        let rows = h
            .fixture
            .db
            .query_one(Statement::from_sql_and_values(
                sea_orm::DatabaseBackend::Sqlite,
                "SELECT count(*) AS n FROM tenancy_provider_technical_facts \
                 WHERE installation_id=? AND tenant_id=? AND execution_id=?",
                [
                    h.fixture.installation.as_str().into(),
                    h.fixture.tenant.as_str().into(),
                    id.as_str().into(),
                ],
            ))
            .await?
            .context("actual optional row observation")?
            .try_get::<i64>("", "n")?;
        let preempt = probe.preempt_seen.load(Ordering::Acquire);
        // Fixed stage/numeric SQLite outcome only; no error text, SQL, secrets,
        // provider body, credential or identifying request bytes are printed.
        eprintln!(
            "rotation_overlap stage=write_complete held_during_intent={} ok={} sqlite_extended={:?} preempt={} optional_rows={}",
            hold_optional_during_intent, observed.ok, observed.sqlite_code, preempt, rows
        );
        ensure!(
            !probe.commit_expired.load(Ordering::Acquire) && rows == 1,
            "optional native commit/closed-row witness missing"
        );
        if hold_optional_during_intent {
            ensure!(preempt, "original Slot cancellation was not observed");
        }
        if rotated.is_err() {
            ensure!(
                !observed.ok && observed.sqlite_code == Some(517),
                "rotation failed without typed snapshot witness"
            );
            ensure!(
                after.revision == before.revision
                    && after.operation_ids == before.operation_ids
                    && after.safety_operation_ids == before.safety_operation_ids,
                "failed rotation changed authority header"
            );
        }
        // Regression oracle: optional diagnostics must not break the actual
        // rotation. Same success oracle, now with actual intent-before-snapshot.
        ensure!(
            rotated.is_ok() && observed.ok && observed.sqlite_code.is_none(),
            "optional COMMIT invalidated actual actor rotation; sqlite_extended={:?}",
            observed.sqlite_code
        );
        let rotated = rotated.map_err(|_| anyhow::anyhow!("actor rotation failed"))?;
        ensure!(rotated.credential_id != h.credential.credential_id);
        let mut headers = old_headers;
        headers.insert(HEADER, PROVIDER_NATIVE_V1.parse()?);
        let denied = raw(
            &h,
            headers,
            json!({"model":MODEL,"input":"recall","previous_response_id":"resp_receipts"}),
        )
        .await?;
        ensure!(denied.status() == StatusCode::UNAUTHORIZED && h.observed()?.len() == 1);
        Ok::<(), anyhow::Error>(())
    })
    .await;
    drop(release);
    let shutdown = tokio::time::timeout_at(end, h.fixture.service.shutdown()).await;
    // Observe shutdown even when the regression fails.
    shutdown??;
    checked??;
    Ok(())
}

#[tokio::test]
async fn optional_commit_before_authority_snapshot_preserves_actual_rotation() -> Result<()> {
    Box::pin(rotation_with_optional_intent_order(false)).await
}

#[tokio::test]
async fn write_intent_precedes_authority_snapshot_during_owned_optional_commit() -> Result<()> {
    Box::pin(rotation_with_optional_intent_order(true)).await
}

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
    let resources = service.resources.clone();
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
    let initial = crate::tenancy::store::load(&db, chrono::Utc::now()).await?;
    let service = crate::tenancy::Service::start_resources_with_continuations(
        db.clone(),
        Some(owner),
        initial,
        resources,
        crate::tenancy::Provisioning {
            import: None,
            continuations: &crate::config::ContinuationConfig::default(),
            claim: Some(claim),
            receipt_observer: crate::telemetry::receipt_latency::Sink::default(),
            config: &crate::config::tenancy::TenancyConfig::default(),
        },
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
    let mut rotation = h.fixture.issuance("native-rotate").await?;
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
