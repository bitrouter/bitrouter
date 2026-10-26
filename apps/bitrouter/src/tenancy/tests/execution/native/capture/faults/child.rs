use super::*;

pub(super) async fn exercise(root: &Path, mode: &str) -> Result<Option<tokio::net::UnixStream>> {
    let program = executable()?;
    let provider = Provider::start(false).await?;
    let (fixture, route) = file_fixture(&provider, root).await?;
    let (headers, _) = prepared(&fixture, &route, LearningMode::Observe).await?;
    let marker = root.join("child.json");
    let socket = root.join("initialize.sock");
    let initialized = tokio::net::UnixListener::bind(&socket)?;
    let app = Arc::new(
        host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
            .with_tenancy(fixture.service.clone()),
    );
    let listener = Listener::start(app.clone()).await?;
    let gate = fixture
        .service
        .hold_host(crate::tenancy::hosts::HostBoundary::Cleanup)?;
    let proposed = launch(&program, &marker, &listener.origin, Some(&socket));
    let opener = AbortOnDropHandle::new(tokio::spawn(async move {
        ControlledSession::open_authenticated(app, headers, proposed, CancellationToken::new())
            .await
    }));
    let (mut peer, _) = tokio::time::timeout(TIMEOUT, initialized.accept()).await??;
    let mut ready = [0; 5];
    tokio::time::timeout(TIMEOUT, peer.read_exact(&mut ready)).await??;
    ensure!(&ready == b"ready", "real child never received initialize");
    let actual = pid(&marker)?;
    unconfigured(&marker)?;
    ensure!(rustix::process::test_kill_process_group(actual).is_ok());
    ensure!(count(&fixture, "acp_capture_connections").await? == 1);
    ensure!(fixture.service.current().is_ok() && fixture.service.host_slots_for_test() == 0);
    ensure!(!opener.is_finished() && provider.calls() == 0);

    // Abandon only the public response waiter. The existing initializer socket
    // stays open and unreleased, so the real child cannot have exited gracefully.
    opener.abort();
    let aborted = opener.await.err().context("opener was not cancelled")?;
    ensure!(aborted.is_cancelled());
    tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
    ensure!(rustix::process::test_kill_process_group(actual).is_ok());
    ensure!(fixture.service.host_slots_for_test() == 0);
    if mode == "reaper-unwind" {
        // resume_unwind occurs inside own_child's actual caught future. No test
        // task panic, outer JoinHandle abort, or fake cleanup flag stands in for it.
        gate.unwind();
        tokio::time::timeout(TIMEOUT, fixture.service.cleanup.quarantined()).await?;
        retained(&fixture)?;
        no_clean_drain(&fixture, false).await?;
        ensure!(rustix::process::test_kill_process_group(actual).is_ok());
    } else {
        // Deadline expiration while the same reaper/Child remains alive must
        // fence, not release capacity or report completed host ownership.
        no_clean_drain(&fixture, true).await?;
        tokio::time::timeout(TIMEOUT, fixture.service.cleanup.quarantined()).await?;
        retained(&fixture)?;
        ensure!(rustix::process::test_kill_process_group(actual).is_ok());
        gate.release();
        tokio::time::timeout(TIMEOUT, async {
            loop {
                match rustix::process::test_kill_process_group(actual) {
                    Err(rustix::io::Errno::SRCH) => return Ok::<_, anyhow::Error>(()),
                    Ok(()) => tokio::time::sleep(Duration::from_millis(5)).await,
                    Err(error) => return Err(error.into()),
                }
            }
        })
        .await??;
        // Actual OS cleanup after the missed deadline is not permission to
        // clear sticky quarantine or to forge the remaining durable close ACK.
        no_clean_drain(&fixture, false).await?;
    }
    retained(&fixture)?;
    ensure!(provider.calls() == 0 && pid(&marker)? == actual);
    unconfigured(&marker)?;
    let row = fixture
        .db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT state, metadata_json FROM acp_capture_connections",
        ))
        .await?
        .context("actual capture connection")?;
    let state: Value = serde_json::from_str(&row.try_get::<String>("", "metadata_json")?)?;
    ensure!(state["complete"] == false);
    ensure!(row.try_get::<String>("", "state")? != "complete");
    listener.finish().await?;
    tokio::time::timeout(TIMEOUT, fixture.db.clone().close()).await??;
    provider.finish().await?;
    // This fixture socket is not a production owner. Keep it in the bounded
    // child-test frame through the external observation so EOF cannot make the
    // blocked peer exit independently of the retained production Child.
    Ok((mode == "reaper-unwind").then_some(peer))
}
