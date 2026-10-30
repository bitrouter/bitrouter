//! Authenticated consumer reaches a real exited-but-unreaped child, without
//! injecting a signal result or replacing any production cleanup owner.
use super::*;
use rustix::process::{WaitId, WaitIdOptions, waitid};

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn exited_child_is_reaped_before_owned_capture_cleanup_ack() -> Result<()> {
    exercise(false).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn exited_leader_does_not_skip_owned_live_descendant_cleanup() -> Result<()> {
    exercise(true).await
}

async fn exercise(with_descendant: bool) -> Result<()> {
    let program = executable()?;
    let root = tempfile::tempdir()?;
    let provider = Provider::start(false).await?;
    let (fixture, route) = file_fixture(&provider, root.path()).await?;
    let (headers, _) = prepared(&fixture, &route, LearningMode::Observe).await?;
    let authority = fixture.service.host_authority(&headers).map_err(failure)?;
    let marker = root.path().join("child.json");
    let socket = root.path().join("initialize.sock");
    let initialize = tokio::net::UnixListener::bind(&socket)?;
    let descendant_socket = root.path().join("descendant.sock");
    let descendants = tokio::net::UnixListener::bind(&descendant_socket)?;
    let app = Arc::new(
        host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
            .with_tenancy(fixture.service.clone()),
    );
    let listener = Listener::start(app.clone()).await?;
    let gate = fixture
        .service
        .hold_host(crate::tenancy::hosts::HostBoundary::Cleanup)?;
    let mut proposed = launch(&program, &marker, &listener.origin, Some(&socket));
    let AcpTransport::Stdio { env, .. } = &mut proposed.agent.transport;
    env.insert("CAPTURE_FIXTURE_EXIT_AT_INITIALIZE".into(), "1".into());
    if with_descendant {
        env.insert(
            "CAPTURE_FIXTURE_DESCENDANT_GATE".into(),
            descendant_socket.to_string_lossy().into_owned(),
        );
    }
    let mut opener = Some(AbortOnDropHandle::new(tokio::spawn(async move {
        ControlledSession::open_authenticated(app, headers, proposed, CancellationToken::new())
            .await
    })));
    let mut initializer = None;
    let mut descendant = None;
    let checked: Result<()> = async {
        initializer = Some(tokio::time::timeout(TIMEOUT, initialize.accept()).await??.0);
        let peer = initializer.as_mut().context("initializer")?;
        let mut ready = [0; 5];
        tokio::time::timeout(TIMEOUT, peer.read_exact(&mut ready)).await??;
        ensure!(&ready == b"ready");
        let child = pid(&marker)?;
        let leaf_pid = if with_descendant {
            descendant = Some(
                tokio::time::timeout(TIMEOUT, descendants.accept())
                    .await??
                    .0,
            );
            let mut raw = [0; 4];
            tokio::time::timeout(
                TIMEOUT,
                descendant
                    .as_mut()
                    .context("descendant")?
                    .read_exact(&mut raw),
            )
            .await??;
            Some(
                rustix::process::Pid::from_raw(i32::try_from(u32::from_be_bytes(raw))?)
                    .context("descendant PID")?,
            )
        } else {
            None
        };
        unconfigured(&marker)?;
        ensure!(rustix::process::test_kill_process_group(child).is_ok());
        ensure!(count(&fixture, "acp_capture_connections").await? == 1);
        let task = opener.take().context("retained opener")?;
        task.abort();
        ensure!(
            task.await
                .err()
                .context("opener not cancelled")?
                .is_cancelled()
        );
        tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
        ensure!(waitid(WaitId::Pid(child), options)?.is_none());
        peer.write_all(&[1]).await?;
        // WNOWAIT observes kernel exit state without stealing the production
        // Child's wait status. The existing cleanup gate prevents its wait from
        // running during this observation; no sleep is accepted as exit proof.
        // https://docs.rs/rustix/1.1.4/rustix/process/fn.waitid.html
        let status = tokio::time::timeout(TIMEOUT, async {
            loop {
                if let Some(status) = waitid(WaitId::Pid(child), options)? {
                    return Ok::<_, anyhow::Error>(status);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await??;
        ensure!(status.exit_status() == Some(0));
        if let Some(pid) = leaf_pid {
            let peer = descendant.as_mut().context("live descendant")?;
            tokio::time::timeout(TIMEOUT, async {
                peer.write_all(b"p").await?;
                let mut reply = [0];
                peer.read_exact(&mut reply).await?;
                ensure!(reply == *b"p");
                Ok::<_, anyhow::Error>(())
            })
            .await??;
            ensure!(pid != child && rustix::process::getpgid(Some(pid))? == child);
            ensure!(rustix::process::test_kill_process_group(child).is_ok());
        } else {
            // Require the actual OS response, not a cfg(test) EPERM injection.
            // Linux permits a zombie group probe; Darwin filters this member.
            #[cfg(target_os = "macos")]
            ensure!(
                rustix::process::test_kill_process_group(child) == Err(rustix::io::Errno::PERM)
            );
            #[cfg(target_os = "linux")]
            ensure!(rustix::process::test_kill_process_group(child).is_ok());
        }
        ensure!(fixture.service.current().is_ok());
        ensure!(fixture.service.host_slots_for_test() == 0);
        let locked = crate::tenancy::ownership::acquire_path(&root.path().join("owned.db"), false)
            .err()
            .context("file owner released before actual reap")?;
        ensure!(matches!(
            locked.downcast_ref::<std::fs::TryLockError>(),
            Some(std::fs::TryLockError::WouldBlock)
        ));
        gate.release();
        tokio::time::timeout(TIMEOUT, async {
            loop {
                fixture.service.current().map_err(failure)?;
                if fixture.service.host_slots_for_test() == 1 {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await??;
        ensure!(
            fixture
                .service
                .host_cleanup_unknown_site_for_test()
                .is_none()
        );
        reaped(&marker).await?;
        if let Some(peer) = &mut descendant {
            let mut byte = [0];
            ensure!(
                tokio::time::timeout(TIMEOUT, peer.read(&mut byte)).await?? == 0,
                "descendant remained alive after production cleanup ACK"
            );
        }
        ensure!(matches!(
            waitid(WaitId::Pid(child), options),
            Err(rustix::io::Errno::CHILD)
        ));
        let slot = fixture.service.host_slot(&authority).map_err(failure)?;
        drop(slot);
        let rows = fixture
            .db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT state, metadata_json FROM acp_capture_connections",
            ))
            .await?;
        ensure!(rows.len() == 1);
        ensure!(rows[0].try_get::<String>("", "state")? == "interrupted");
        let state: Value = serde_json::from_str(&rows[0].try_get::<String>("", "metadata_json")?)?;
        ensure!(state["closed"] == true && state["complete"] == false);
        ensure!(provider.calls() == 0);
        unconfigured(&marker)?;
        Ok(())
    }
    .await;
    // Always release the real reaper and fixture peer, even on failed setup or
    // observation. Containment cannot turn checked failure into successful ACK.
    gate.release();
    drop(initializer.take());
    drop(descendant.take());
    if let Some(task) = opener.take() {
        task.abort();
        let _cancelled = task.await;
    }
    let stopped = listener.finish().await;
    let finished = finish(&fixture, provider, checked.and(stopped)).await;
    let closed = tokio::time::timeout(TIMEOUT, fixture.db.clone().close()).await;
    drop(fixture);
    drop(authority);
    let closed: Result<()> = match closed {
        Ok(result) => result.map_err(Into::into),
        Err(error) => Err(error.into()),
    };
    let released =
        crate::tenancy::ownership::acquire_path(&root.path().join("owned.db"), false).map(drop);
    let failures = [finished, closed, released]
        .into_iter()
        .filter_map(Result::err)
        .map(|error| format!("{error:#}"))
        .collect::<Vec<_>>();
    if !failures.is_empty() {
        let retained = root.keep();
        anyhow::bail!(
            "{}; fixture retained at {}",
            failures.join("; "),
            retained.display()
        );
    }
    Ok(())
}
