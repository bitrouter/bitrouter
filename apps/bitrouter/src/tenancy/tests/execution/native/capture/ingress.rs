//! Real child stdout crosses the production raw reader while H holds a write.
use super::*;
use crate::harness::acp::up::owned::ingress::{
    Limit, MAX_BYTES, MAX_FRAME_BYTES, MAX_FRAMES, READ_CHUNK,
};
use futures::StreamExt;

async fn attack(mode: &str, expected: Limit) -> Result<()> {
    let program = executable()?;
    let provider = Provider::start(false).await?;
    let (fixture, route, _) = provision_protocol(&provider.url, "chat_completions").await?;
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join("child.json");
    let socket = directory.path().join("ingress.sock");
    let gate = tokio::net::UnixListener::bind(&socket)?;
    let app = Arc::new(
        host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
            .with_tenancy(fixture.service.clone()),
    );
    let listener = Listener::start(app.clone()).await?;
    let mut session = None;
    let mut prompt = None;
    let mut statistics = None;
    let mut updates = None;
    let mut rejected = None;
    let mut authority = None;
    let checked: Result<()> = async {
        let (headers, _) = prepared(&fixture, &route, LearningMode::Observe).await?;
        authority = Some(fixture.service.host_authority(&headers).map_err(failure)?);
        let mut proposed = launch(&program, &marker, &listener.origin, None);
        let AcpTransport::Stdio { env, .. } = &mut proposed.agent.transport;
        env.insert("CAPTURE_FIXTURE_INGRESS_ATTACK".into(), mode.into());
        env.insert(
            "CAPTURE_FIXTURE_INGRESS_GATE".into(),
            socket.to_string_lossy().into_owned(),
        );
        session = Some(Arc::new(
            tokio::time::timeout(
                TIMEOUT,
                ControlledSession::open_authenticated(
                    app,
                    headers,
                    proposed,
                    CancellationToken::new(),
                ),
            )
            .await??,
        ));
        let current = session.as_ref().context("opened session")?;
        let id = current.new_session(directory.path().to_path_buf()).await?;
        updates = Some(current.subscribe_updates());
        let observed = current.ingress_statistics();
        statistics = Some(observed.clone());
        let child = current.clone();
        prompt = Some(AbortOnDropHandle::new(tokio::spawn(async move {
            child.prompt(&id, "bounded ingress probe").await
        })));
        let (mut peer, _) = tokio::time::timeout(TIMEOUT, gate.accept()).await??;
        let mut ready = [0; 5];
        tokio::time::timeout(TIMEOUT, peer.read_exact(&mut ready)).await??;
        ensure!(&ready == b"ready", "child did not reach real prompt");
        let held = fixture
            .service
            .cleanup
            .hold(crate::tenancy::transaction::Phase::Active);
        peer.write_all(&[1]).await?;
        tokio::time::timeout(TIMEOUT, held.entered.notified()).await?;
        let before = observed.snapshot();
        ensure!(
            before.frames >= 5 && before.reason.is_none(),
            "initialize/provider/session traffic did not cross ingress"
        );
        ensure!(
            fixture.service.current().is_ok(),
            "authority already unavailable before releasing raw flood; H={:?}, host_unknown={:?}",
            fixture.service.cleanup.disposition(),
            fixture.service.host_cleanup_unknown_site_for_test(),
        );
        let flood_started = tokio::time::Instant::now();
        peer.write_all(&[2]).await?;
        tokio::time::timeout(TIMEOUT, observed.rejected()).await?;
        let snapshot = observed.snapshot();
        let boundary = observed
            .rejection_boundary()
            .context("actual raw rejection boundary was not observed")?;
        ensure!(
            snapshot.reason == Some(expected),
            "wrong raw rejection: {snapshot:?}"
        );
        ensure!(
            snapshot.read > before.read && snapshot.frames >= before.frames,
            "fixture supplied no raw flood"
        );
        ensure!(
            snapshot.bytes <= MAX_BYTES
                && snapshot.frames <= MAX_FRAMES
                && snapshot.largest <= MAX_FRAME_BYTES,
            "raw input was admitted beyond a limit"
        );
        ensure!(
            snapshot.read <= MAX_BYTES + READ_CHUNK,
            "raw read exceeded bounded overshoot"
        );
        if expected == Limit::Frame {
            ensure!(
                snapshot.frames == before.frames,
                "unterminated line unexpectedly completed a frame"
            );
            ensure!(snapshot.read <= before.read + MAX_FRAME_BYTES + READ_CHUNK);
        }
        ensure!(
            !matches!(
                updates.as_mut().context("updates")?.next().now_or_never(),
                Some(Some(_))
            ),
            "notification forwarded while its real capture writer was held"
        );
        ensure!(provider.calls() == 0);
        let occupied = fixture
            .service
            .host_slot(authority.as_ref().context("authority")?)
            .err()
            .context("capture dropped host slot before settling held SQL")?;
        ensure!(
            occupied.0.code == ErrorCode::CapacityExceeded,
            "held capture slot: code={:?}, elapsed={:?}, raw_rejected_after={:?}, H={:?}, host_unknown_before_raw={:?}, host_unknown_now={:?}, reaper_failure={:?}, slots={}, ingress_before={before:?}, ingress={snapshot:?}",
            occupied.0.code,
            flood_started.elapsed(),
            boundary.at.saturating_duration_since(flood_started),
            fixture.service.cleanup.disposition(),
            boundary.host_unknown,
            fixture.service.host_cleanup_unknown_site_for_test(),
            observed.reaper_failure(),
            fixture.service.host_slots_for_test(),
        );
        rejected = Some(snapshot);
        Ok(())
    }
    .await;
    // Unconditional release/join even on a failed observation. No aborted owner,
    // simulated reaper proof or nested build is used for a passing test.
    fixture.service.cleanup.release_gate();
    let prompted: Result<()> = match prompt.take() {
        Some(task) => match tokio::time::timeout(TIMEOUT, task).await {
            Ok(Ok(Err(_))) => Ok(()),
            Ok(Ok(Ok(()))) => Err(anyhow::anyhow!("overflowed prompt succeeded")),
            Ok(Err(error)) => Err(error.into()),
            Err(error) => Err(error.into()),
        },
        None => Ok(()),
    };
    let closed = match session.take() {
        Some(session) => match Arc::try_unwrap(session) {
            Ok(session) => session.shutdown().await,
            Err(_) => Err(anyhow::anyhow!("prompt still retains session after join")),
        },
        None => Ok(()),
    };
    let stopped = listener.finish().await;
    let after: Result<()> = async {
        checked?;
        prompted?;
        closed?;
        stopped?;
        reaped(&marker).await?;
        ensure!(
            statistics.context("statistics")?.snapshot()
                == rejected.context("observed rejection")?,
            "raw reads continued after rejection"
        );
        ensure!(
            !matches!(
                updates.as_mut().context("updates")?.next().now_or_never(),
                Some(Some(_))
            ),
            "queued raw data forwarded after rejection"
        );
        let rows = fixture
            .db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT state, metadata_json FROM acp_capture_connections",
            ))
            .await?;
        ensure!(
            rows.len() == 1,
            "missing capture must not pass as interruption"
        );
        ensure!(rows[0].try_get::<String>("", "state")? == "interrupted");
        let metadata: Value =
            serde_json::from_str(&rows[0].try_get::<String>("", "metadata_json")?)?;
        ensure!(
            metadata["connected"] == true
                && metadata["closed"] == true
                && metadata["complete"] == false
        );
        ensure!(
            count(&fixture, "acp_canonical_sessions").await? == 1
                && count(&fixture, "acp_checkpoints").await? == 0
                && provider.calls() == 0
        );
        let rows = fixture
            .db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT event_json FROM acp_capture_events ORDER BY sequence DESC LIMIT 1",
            ))
            .await?;
        ensure!(rows.len() == 1);
        let terminal: Value = serde_json::from_str(&rows[0].try_get::<String>("", "event_json")?)?;
        ensure!(
            terminal["method"] == "controller/disconnect" && terminal["payload"]["clean"] == false
        );
        // A successful slot acquisition is possible only after the real host
        // joined its controller, client driver, reaper and pending Capture job.
        let slot = fixture
            .service
            .host_slot(authority.as_ref().context("authority")?)
            .map_err(failure)?;
        drop(slot);
        Ok(())
    }
    .await;
    finish(&fixture, provider, after).await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn real_unterminated_child_line_is_refused_before_sdk_accumulation() -> Result<()> {
    attack("unterminated", Limit::Frame).await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn real_small_frame_flood_is_bounded_while_capture_writer_is_held() -> Result<()> {
    attack("frames", Limit::Frames).await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn real_byte_flood_exhausts_cumulative_allowance_without_frame_reset() -> Result<()> {
    attack("bytes", Limit::Bytes).await
}
