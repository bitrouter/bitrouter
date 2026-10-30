//! The real host producer, Rust ACP child, provider and actor drain together.
use super::*;
async fn controller_case(fail_provider: bool) -> Result<()> {
    let executable = executable()?;
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join("paid-controller.json");
    let mut provider = Provider::start(false).await?;
    let mut judge = Judge::new(true).await?;
    let ready = Ready::new(&provider.url, &judge.url, true, LearningMode::Observe, 1).await?;
    let spies = Arc::new(HostSpies::default());
    let mut listener = None;
    let mut session = None;
    let mut opener_rejected = false;
    let checked = std::panic::AssertUnwindSafe(async {
        let app = Arc::new(
            host_app(&ready.fixture, &provider.url, spies.clone())?
                .with_tenancy(ready.fixture.service.clone()),
        );
        listener = Some(Listener::start(app.clone()).await?);
        let mut proposed = launch(&executable, &marker, &listener.as_ref().context("listener")?.origin, None);
        if fail_provider {
            let AcpTransport::Stdio { env, .. } = &mut proposed.agent.transport;
            env.insert("CAPTURE_FIXTURE_FAIL_PROVIDER".into(), "1".into());
        }
        // Timeout drops only the opener waiter. Service still owns any partial
        // child/driver, so all outcomes must reach the cleanup tail below.
        match tokio::time::timeout(TIMEOUT, ControlledSession::open_authenticated(
            app,
            ready.headers()?,
            proposed,
            CancellationToken::new(),
        )).await? {
            Ok(value) => session = Some(Arc::new(value)),
            Err(error) => {
                opener_rejected = true;
                return Err(error.context("actual controlled initialization rejected"));
            }
        }
        ensure!(!fail_provider, "provider failure fixture unexpectedly initialized");
        let session = session.as_ref().context("controlled session")?;
        let native = session.new_session(directory.path().to_owned()).await?;
        session.prompt(&native, "explicit stopped prefix before paid shutdown").await?;
        provider.receive().await?;
        drained(&ready).await?;
        let caller = session.clone();
        let task = AbortOnDropHandle::new(tokio::spawn(async move { caller.checkpoint(&native).await }));
        judge.receive().await?;
        ensure!(bodies(&ready.fixture, "owned_judge_pending").await?.len() == 1);
        let stopped = ready.fixture.service.host_stop();
        let shutdown = ready.fixture.service.shutdown();
        tokio::pin!(shutdown);
        tokio::select! {
            result = &mut shutdown => { result?; anyhow::bail!("real paid producer was reported drained"); }
            () = stopped.cancelled() => {}
        }
        ensure!(shutdown.as_mut().now_or_never().is_none(), "held paid provider lost its drain owner");
        judge.release.cancel();
        let _caller_result = tokio::time::timeout(TIMEOUT, task).await??;
        tokio::time::timeout(TIMEOUT, shutdown).await??;
        reaped(&marker).await?;
        let jobs = bodies(&ready.fixture, "owned_judge_job").await?;
        ensure!(jobs.len() == 1 && jobs[0]["status"] != "accepted");
        ensure!(bodies(&ready.fixture, "owned_judge_pending").await?.is_empty());
        let durable = receipt(&ready, jobs[0]["execution"].as_str().context("actual paid execution")?).await?;
        ensure!(durable["attempts"].as_array().context("attempts")?.len() == 1);
        ensure!(judge.calls.load(Ordering::SeqCst) == 1 && provider.calls() == 1);
        ensure!(spies.decisions.load(Ordering::SeqCst) == 0 && spies.transforms.load(Ordering::SeqCst) == 0);
        Ok(())
    }).catch_unwind().await;
    let checked = match checked {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("paid controller observation unwound")),
    };
    judge.release.cancel();
    drop(session);
    let closed = tokio::time::timeout(Duration::from_secs(40), ready.fixture.service.shutdown())
        .await
        .context("actual producer shutdown deadline")
        .and_then(|result| result);
    let reaped = match marker.try_exists() {
        Ok(true) => reaped(&marker).await,
        Ok(false) => Ok(()), // Pre-spawn failure has no PID; Service must still ACK cleanup.
        Err(error) => Err(error.into()),
    };
    let listener = match listener {
        Some(listener) => listener.finish().await,
        None => Ok(()),
    };
    let checked = if fail_provider
        && opener_rejected
        && checked.is_err()
        && closed.is_ok()
        && reaped.is_ok()
        && listener.is_ok()
    {
        // This expected-error control is green only after the actual provider
        // setup witness and the unconditional real cleanup results below pass.
        async {
            let process: Value = serde_json::from_slice(&tokio::fs::read(&marker).await?)?;
            ensure!(process["provider_set"] == 1);
            ensure!(judge.calls.load(Ordering::SeqCst) == 0 && provider.calls() == 0);
            ensure!(count(&ready.fixture, "acp_canonical_sessions").await? == 0);
            ensure!(bodies(&ready.fixture, "owned_judge_job").await?.is_empty());
            let row = ready
                .fixture
                .db
                .query_one(Statement::from_string(
                    DatabaseBackend::Sqlite,
                    "SELECT metadata_json FROM acp_capture_connections",
                ))
                .await?
                .context("real rejected capture connection")?;
            let metadata: Value =
                serde_json::from_str(&row.try_get::<String>("", "metadata_json")?)?;
            ensure!(metadata["closed"] == true);
            ready.fixture.service.cleanup.ensure_clean()?;
            ensure!(ready.fixture.service.host_slots_for_test() == 1);
            Ok(())
        }
        .await
    } else {
        checked
    };
    cleanup(
        &ready,
        &[],
        judge,
        provider,
        checked.and(closed).and(reaped).and(listener),
    )
    .await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn checked_controller_shutdown_retains_the_held_paid_execution_and_reaps_its_real_child()
-> Result<()> {
    controller_case(false).await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn rejected_paid_controller_initialization_still_drains_the_real_owner_and_reaps()
-> Result<()> {
    controller_case(true).await
}
