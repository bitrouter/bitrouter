use super::*;
use bitrouter_sdk::{
    gateway::HttpVersion,
    tenancy::{TenancyClient, TenancyOptions},
};

#[tokio::test]
async fn actual_sdk_reads_standalone_profiles_over_h1_h2_and_retained_restart() -> Result<()> {
    let mut fixture = keyed_fixture().await?;
    let local = provision(
        &fixture,
        "local",
        "create-local",
        None,
        rules(1, Some(2), true),
    )
    .await?;
    let shared = provision(
        &fixture,
        "shared",
        "create-shared",
        None,
        rules(2, Some(3), true),
    )
    .await?;
    let mut previous_instance = None;
    for restart_required in [false, true] {
        if restart_required {
            restart(&mut fixture).await?;
        }
        let app = router(&fixture)?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let server = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, app).await
        }));
        for version in [
            HttpVersion::Http1Only,
            #[cfg(feature = "http2")]
            HttpVersion::Http2PriorKnowledge,
        ] {
            let client = TenancyClient::with_options(
                &endpoint,
                TOKEN,
                TenancyOptions {
                    http_version: version,
                    ..Default::default()
                },
            )?;
            let caps = client.admission_profile_observation_capabilities().await?;
            ensure!(
                caps.installation_id == fixture.installation
                    && caps.profile_versions == [1, 2]
                    && caps.observation_enabled
            );
            if let Some(previous) = &previous_instance {
                ensure!(*previous != caps.instance_id);
            }
            let input = Request {
                schema_version: 1,
                expected_authority_revision: caps.authority_revision,
                profiles: vec![shared.clone(), local.clone()],
            };
            let observed = client
                .observe_admission_profiles(&fixture.installation, &input)
                .await?;
            ensure!(
                observed.instance_id == caps.instance_id
                    && observed.profiles[0].profile == shared
                    && observed.profiles[1].profile == local
            );
            ensure!(
                observed.profiles[0]
                    .rate
                    .as_ref()
                    .context("rate")?
                    .member_rpm
                    == 3
            );
        }
        previous_instance = Some(fixture.service.instance.clone());
        drop(server);
    }
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn operator_only_http_and_direct_service_reject_tenant_authority_and_installation_transplants()
-> Result<()> {
    let fixture = keyed_fixture().await?;
    let pin = provision(&fixture, "member", "create", None, rules(2, None, false)).await?;
    let issued = fixture.issue("tenant-admin").await?;
    let input = request(&fixture, vec![pin])?;
    ensure!(
        matches!(fixture.service.profile_observation_read(&Caller::Credential(issued.credential_id.clone()),Some(&fixture.installation),Instant::now()+Duration::from_secs(5)),Err(error) if error.0.code==ErrorCode::Forbidden)
    );
    let app = router(&fixture)?;
    let before = durable(&fixture).await?;
    let mut tenant = http_request(&fixture, &input)?;
    tenant.headers_mut().insert(
        "authorization",
        format!("Bearer {}", issued.secret.context("secret")?).parse()?,
    );
    ensure!(app.clone().oneshot(tenant).await?.status() == StatusCode::UNAUTHORIZED);
    let mut wrong = http_request(&fixture, &input)?;
    wrong
        .headers_mut()
        .insert("bitrouter-installation-id", "other".parse()?);
    ensure!(app.clone().oneshot(wrong).await?.status() == StatusCode::CONFLICT);
    let mut missing = http_request(&fixture, &input)?;
    missing.headers_mut().remove("bitrouter-installation-id");
    ensure!(app.clone().oneshot(missing).await?.status() == StatusCode::BAD_REQUEST);
    let mut duplicate = http_request(&fixture, &input)?;
    duplicate
        .headers_mut()
        .append("authorization", format!("Bearer {TOKEN}").parse()?);
    ensure!(app.clone().oneshot(duplicate).await?.status() == StatusCode::BAD_REQUEST);
    let mut stale = input.clone();
    stale.expected_authority_revision = 0_u64.try_into()?;
    let response = app.clone().oneshot(http_request(&fixture, &stale)?).await?;
    ensure!(response.status() == StatusCode::CONFLICT);
    ensure!(response.headers()["cache-control"] == "no-store");
    let error: bitrouter_ai::tenancy::operation::Error =
        serde_json::from_slice(&axum::body::to_bytes(response.into_body(), 4096).await?)?;
    ensure!(
        error.code == ErrorCode::RevisionConflict
            && error.operation_id.is_none()
            && error.execution_id.is_none()
            && error.dispatch_state == bitrouter_ai::tenancy::operation::DispatchState::NotStarted
    );
    let discovery = HttpRequest::builder()
        .uri(CAPS)
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::empty())?;
    let response = app.oneshot(discovery).await?;
    ensure!(response.status() == StatusCode::OK);
    let _: Capabilities =
        serde_json::from_slice(&axum::body::to_bytes(response.into_body(), 1048576).await?)?;
    ensure!(before == durable(&fixture).await?);
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn actual_handler_refuses_held_authority_lock_before_legacy_middleware_can_block()
-> Result<()> {
    let fixture = keyed_fixture().await?;
    let pin = provision(&fixture, "member", "create", None, rules(2, None, false)).await?;
    let input = request(&fixture, vec![pin])?;
    let app = router(&fixture)?;
    let authority = fixture.service.authority.clone();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || -> Result<()> {
        authority
            .with_write(|| {
                ready_tx
                    .send(())
                    .map_err(|_| ErrorCode::AuthorityUnavailable)?;
                release_rx
                    .recv_timeout(Duration::from_secs(2))
                    .map_err(|_| ErrorCode::AuthorityUnavailable)?;
                Ok(())
            })
            .map_err(|code| anyhow::anyhow!("authority lock: {code:?}"))?;
        Ok(())
    });
    ready_rx.await?;
    let started = std::time::Instant::now();
    let response = app.clone().oneshot(http_request(&fixture, &input)?).await;
    let elapsed = started.elapsed();
    let release = release_tx.send(());
    let joined = worker
        .join()
        .map_err(|_| anyhow::anyhow!("lock holder failed"))?;
    release?;
    joined?;
    ensure!(
        elapsed < Duration::from_millis(500),
        "handler waited on a synchronous lock: {elapsed:?}"
    );
    ensure!(response?.status() == StatusCode::TOO_MANY_REQUESTS);
    ensure!(app.oneshot(http_request(&fixture, &input)?).await?.status() == StatusCode::OK);
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn original_body_deadline_returns_no_observations_and_releases_capacity() -> Result<()> {
    let fixture = keyed_fixture().await?;
    let pin = provision(&fixture, "member", "create", None, rules(2, None, false)).await?;
    let input = request(&fixture, vec![pin])?;
    let before = durable(&fixture).await?;
    let runtime = operational(&fixture)?;
    let app = router(&fixture)?;
    let mut request = http_request(&fixture, &input)?;
    *request.body_mut() = Body::from_stream(futures::stream::pending::<
        std::result::Result<Vec<u8>, std::io::Error>,
    >());
    let started = std::time::Instant::now();
    let response = app.clone().oneshot(request).await?;
    ensure!(
        started.elapsed() >= Duration::from_secs(5) && started.elapsed() < Duration::from_secs(7)
    );
    ensure!(response.status() == StatusCode::TOO_MANY_REQUESTS);
    ensure!(fixture.service.reads.available_permits() == 8);
    ensure!(app.oneshot(http_request(&fixture, &input)?).await?.status() == StatusCode::OK);
    ensure!(before == durable(&fixture).await? && runtime == operational(&fixture)?);
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn complete_256_entry_batches_and_raw_body_bounds_never_truncate() -> Result<()> {
    let fixture = keyed_fixture().await?;
    let pin = provision(&fixture, "member", "create", None, rules(2, None, false)).await?;
    let mut input = request(
        &fixture,
        (0..256)
            .map(|n| {
                let mut r = pin.clone();
                r.revision_id = format!("missing-{n}")
                    .try_into()
                    .map_err(anyhow::Error::from)?;
                Ok(r)
            })
            .collect::<Result<Vec<_>>>()?,
    )?;
    input.profiles[0] = pin.clone();
    let app = router(&fixture)?;
    let response = app.clone().oneshot(http_request(&fixture, &input)?).await?;
    ensure!(response.status() == StatusCode::OK);
    let observed: Snapshot =
        serde_json::from_slice(&axum::body::to_bytes(response.into_body(), 1048576).await?)?;
    ensure!(observed.profiles.len() == 256);
    for (entry, pin) in observed.profiles.iter().zip(&input.profiles) {
        ensure!(&entry.profile == pin);
    }
    ensure!(
        observed.profiles[1..]
            .iter()
            .all(|entry| entry.availability == AvailabilityObservation::AuthorityDenied)
    );
    input.profiles.push(pin);
    ensure!(
        app.clone()
            .oneshot(http_request(&fixture, &input)?)
            .await?
            .status()
            == StatusCode::BAD_REQUEST
    );
    input.profiles.truncate(1);
    let mut bytes = serde_json::to_vec(&input)?;
    bytes.resize(1048577, b' ');
    let mut oversized = http_request(&fixture, &input)?;
    *oversized.body_mut() = Body::from(bytes);
    ensure!(app.clone().oneshot(oversized).await?.status() == StatusCode::PAYLOAD_TOO_LARGE);
    let raw = serde_json::to_string(&input)?.replace(
        "\"schema_version\":1",
        "\"schema_version\":1,\"schema_version\":1",
    );
    let mut duplicate = http_request(&fixture, &input)?;
    *duplicate.body_mut() = Body::from(raw);
    ensure!(app.oneshot(duplicate).await?.status() == StatusCode::BAD_REQUEST);
    fixture.service.shutdown().await?;
    Ok(())
}
