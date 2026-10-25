//! Actual native host + MCP process + H transaction ownership, in isolated runtimes.
mod http;
mod journal;

use super::*;
use crate::{
    config::mcp::{McpServerConfig, McpTransport},
    tenancy::transaction::{Disposition, Phase},
};
use std::{io::Write, path::Path, process::Stdio};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

const ROOT: &str = "BITROUTER_TEST_NATIVE36_CUSTODY_ROOT";
const MODE: &str = "BITROUTER_TEST_NATIVE36_CUSTODY_MODE";
const READY: &str = "native36-custody-checked";

fn config() -> Result<HarnessConfig> {
    // Never start Cargo from this test. The parent builds the existing fixture
    // before entering any runtime deadline, as the other MCP process tests do.
    let directory = std::env::var_os("BITROUTER_TEST_EXAMPLES_DIR")
        .context("prebuild harness_mcp_fixture and set BITROUTER_TEST_EXAMPLES_DIR")?;
    let executable = std::path::PathBuf::from(directory)
        .join(format!(
            "harness_mcp_fixture{}",
            std::env::consts::EXE_SUFFIX
        ))
        .canonicalize()?;
    ensure!(executable.is_file(), "prebuilt MCP fixture missing");
    Ok(HarnessConfig {
        servers: vec![McpServerConfig::with_defaults(
            "retained",
            McpTransport::Stdio {
                command: executable.to_string_lossy().into_owned(),
                args: vec![],
                env: Default::default(),
            },
        )],
        ..HarnessConfig::default()
    })
}

fn child_pid(workspace: &Path) -> Result<rustix::process::Pid> {
    let bytes = std::fs::read(workspace.join("mcp.pid"))?;
    ensure!(bytes.len() <= 32, "fixture PID bound");
    rustix::process::Pid::from_raw(std::str::from_utf8(&bytes)?.parse()?)
        .context("invalid MCP fixture PID")
}

async fn no_clean_drain(fixture: &Fixture) -> Result<()> {
    ensure!(fixture.service.cleanup.disposition() == Disposition::Ambiguous);
    let stop = fixture.service.host_stop();
    tokio::time::pause();
    let checked = async {
        let shutdown = fixture.service.shutdown();
        tokio::pin!(shutdown);
        let early = tokio::select! {
            biased;
            result = &mut shutdown => Some(result),
            () = stop.cancelled() => None,
        };
        let early = early.or_else(|| shutdown.as_mut().now_or_never());
        ensure!(stop.is_cancelled(), "host drain did not close producers");
        // Existing quarantine can return Err immediately. Only Ok would claim
        // clean drain; a still-pending drain must also fail at its deadline.
        match early {
            Some(result) => ensure!(result.is_err(), "quarantined host reported clean shutdown"),
            None => {
                tokio::time::advance(Duration::from_secs(31)).await;
                ensure!(
                    shutdown.await.is_err(),
                    "quarantined host reported clean shutdown"
                );
            }
        }
        ensure!(fixture.service.cleanup.disposition() == Disposition::Ambiguous);
        Ok(())
    }
    .await;
    tokio::time::resume();
    checked
}

async fn exercise(root: &Path, mode: &str) -> Result<()> {
    if mode.starts_with("journal-") {
        return journal::exercise(root, mode).await;
    }
    if mode.starts_with("http-") {
        return http::exercise(root, mode).await;
    }
    let provider = Provider::start(false).await?;
    let workspace = root.join("workspace");
    std::fs::create_dir(&workspace)?;
    let (fixture, route) = file_fixture(&provider, root).await?;
    let (headers, key) = ready(&fixture, &route).await?;
    let app = Arc::new(
        host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
            .with_tenancy(fixture.service.clone()),
    );
    if mode == "commit-error" {
        // Actual deferred SQLite constraint failure at COMMIT, after native DML.
        // https://www.sqlite.org/foreignkeys.html#fk_deferred
        for sql in [
            "PRAGMA foreign_keys=ON",
            "CREATE TABLE native36_parent (id INTEGER PRIMARY KEY)",
            "CREATE TABLE native36_child (parent INTEGER REFERENCES native36_parent(id) DEFERRABLE INITIALLY DEFERRED)",
            "CREATE TRIGGER native36_commit_fault AFTER INSERT ON bro_managed_sessions BEGIN INSERT INTO native36_child(parent) VALUES (77); END",
        ] {
            fixture.db.execute_unprepared(sql).await?;
        }
    } else if mode == "dml-rollback" {
        fixture.db.execute_unprepared("CREATE TRIGGER native36_dml_fault BEFORE INSERT ON bro_managed_sessions BEGIN SELECT RAISE(ABORT, 'native journal DML failure'); END").await?;
    }
    let point = if mode == "revoked-before-spawn" {
        crate::tenancy::hosts::HostBoundary::BeforeTransport
    } else if matches!(mode, "discovery-unwind" | "discovery-cleanup-unknown") {
        crate::tenancy::hosts::HostBoundary::Transport
    } else {
        crate::tenancy::hosts::HostBoundary::Resources
    };
    let gate = fixture.service.hold_host(point)?;
    let owned_workspace = workspace.clone();
    let mut config = config()?;
    if mode == "discovery-cleanup-unknown" {
        config
            .servers
            .first_mut()
            .context("MCP server")?
            .tool_prefix = Some("x".repeat(64));
    }
    let opener = AbortOnDropHandle::new(tokio::spawn(async move {
        NativeSession::open_authenticated(
            app,
            headers,
            "custody",
            &owned_workspace,
            ToolMode::Coding,
            &config,
        )
        .await
    }));
    tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
    if mode == "revoked-before-spawn" {
        change_authority(&fixture, &key, &route, "revoke").await?;
        gate.release();
        ensure!(tokio::time::timeout(TIMEOUT, opener).await??.is_err());
        tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await??;
        ensure!(fixture.service.host_slots_for_test() == 1);
        ensure!(
            !workspace.join("mcp.pid").exists(),
            "revoked discovery started the real MCP child"
        );
        ensure!(fixture.service.cleanup.disposition() != Disposition::Ambiguous);
        ensure!(provider.calls() == 0);
        fixture.db.clone().close().await?;
        provider.finish().await?;
        return Ok(());
    }
    let pid = child_pid(&workspace)?;
    ensure!(
        rustix::process::test_kill_process(pid).is_ok(),
        "discovered child is not alive"
    );
    ensure!(fixture.service.host_slots_for_test() == 0);
    ensure!(!opener.is_finished());
    if mode == "discovery-cleanup-unknown" {
        let weak = gate
            .mcp
            .get()
            .context("missing actual MCP owner observer")?
            .clone();
        let cleanup = fixture
            .service
            .hold_host(crate::tenancy::hosts::HostBoundary::Cleanup)?;
        gate.release();
        tokio::time::timeout(TIMEOUT, cleanup.entered.notified()).await?;
        // Real tools/list output plus its configured prefix exceeds the bounded
        // catalog name. The actual cleanup path now holds the actual child.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(6)).await;
        tokio::time::resume();
        ensure!(tokio::time::timeout(TIMEOUT, opener).await??.is_err());
        tokio::time::timeout(TIMEOUT, fixture.service.cleanup.quarantined()).await?;
        ensure!(fixture.service.host_slots_for_test() == 0 && fixture.service.current().is_err());
        let retained = weak
            .upgrade()
            .context("cleanup_unknown dropped the actual partial MCP owner")?;
        let ids = tokio::time::timeout(TIMEOUT, async {
            retained.lock().await.retained_processes_for_test().await
        })
        .await?;
        ensure!(
            ids == vec![u32::try_from(pid.as_raw_nonzero().get())?],
            "retained owner lost the same child/reaper"
        );
        no_clean_drain(&fixture).await?;
        // Explicit test inspection joins the SAME retained cleanup after the
        // condition clears. It does not reset H, retry an effect, or acknowledge
        // Service cleanup. A permit/boolean-only owner cannot satisfy this.
        cleanup.release();
        tokio::time::timeout(TIMEOUT, async { retained.lock().await.shutdown().await })
            .await?
            .map_err(anyhow::Error::msg)?;
        ensure!(
            retained
                .lock()
                .await
                .retained_processes_for_test()
                .await
                .is_empty()
        );
        ensure!(rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH));
        drop(retained);
        ensure!(
            weak.upgrade().is_some(),
            "test accidentally became the only retained owner"
        );
        ensure!(fixture.service.cleanup.disposition() == Disposition::Ambiguous);
        ensure!(fixture.service.host_slots_for_test() == 0);
        ensure!(fixture.service.completion.clone().now_or_never().is_none());
        fixture.db.clone().close().await?;
        ensure!(provider.calls() == 0);
        provider.finish().await?;
        return Ok(());
    }
    match mode {
        "host-unwind" | "discovery-unwind" => gate.unwind(),
        "transaction-unwind" => {
            let transaction = fixture.service.cleanup.hold(Phase::Committing);
            gate.release();
            tokio::time::timeout(TIMEOUT, transaction.entered.notified()).await?;
            fixture.service.cleanup.unwind_gate()?;
        }
        "commit-error" => gate.release(),
        "dml-rollback" => {
            // Install after the initial scoped read rollback, before the write.
            let rollback = fixture.service.cleanup.hold(Phase::RollingBack);
            gate.release();
            tokio::time::timeout(TIMEOUT, rollback.entered.notified()).await?;
            ensure!(fixture.service.cleanup.disposition() == Disposition::Pending);
            ensure!(fixture.service.host_slots_for_test() == 0);
            ensure!(rustix::process::test_kill_process(pid).is_ok());
            // The child runs on a current-thread runtime. Keep shutdown locally
            // polled so CloseProducer cannot overwrite this operation's outcome
            // before the held rollback is witnessed. Joining a spawned shutdown
            // and then reading the last-operation disposition would race it.
            let mut shutdown = Box::pin(fixture.service.shutdown());
            ensure!(futures::poll!(&mut shutdown).is_pending());
            ensure!(fixture.service.host_stop().is_cancelled());
            fixture.service.cleanup.release_gate();
            tokio::time::timeout(
                TIMEOUT,
                std::future::poll_fn(|cx| {
                    if fixture.service.cleanup.disposition() == Disposition::RolledBack {
                        return std::task::Poll::Ready(Ok(()));
                    }
                    let progress = std::future::Future::poll(shutdown.as_mut(), cx);
                    if fixture.service.cleanup.disposition() == Disposition::RolledBack {
                        return std::task::Poll::Ready(Ok(()));
                    }
                    match progress {
                        std::task::Poll::Pending => std::task::Poll::Pending,
                        std::task::Poll::Ready(result) => {
                            std::task::Poll::Ready(result.and_then(|()| {
                                anyhow::bail!(
                                    "native drain completed without its required rollback"
                                )
                            }))
                        }
                    }
                }),
            )
            .await??;
            ensure!(fixture.service.cleanup.disposition() == Disposition::RolledBack);
            ensure!(tokio::time::timeout(TIMEOUT, opener).await??.is_err());
            tokio::time::timeout(TIMEOUT, &mut shutdown).await??;
            fixture.service.cleanup.ensure_clean()?;
            ensure!(fixture.service.host_slots_for_test() == 1);
            ensure!(rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH));
            ensure!(provider.calls() == 0);
            fixture.db.clone().close().await?;
            provider.finish().await?;
            return Ok(());
        }
        _ => anyhow::bail!("unknown custody scenario"),
    }
    ensure!(tokio::time::timeout(TIMEOUT, opener).await??.is_err());
    tokio::time::timeout(TIMEOUT, fixture.service.cleanup.quarantined()).await?;
    ensure!(fixture.service.current().is_err());
    ensure!(fixture.service.cleanup.disposition() == Disposition::Ambiguous);
    ensure!(fixture.service.host_slots_for_test() == 0);
    ensure!(
        rustix::process::test_kill_process(pid).is_ok(),
        "unwind dropped the actual child owner"
    );
    no_clean_drain(&fixture).await?;
    ensure!(fixture.service.host_slots_for_test() == 0);
    ensure!(fixture.service.completion.clone().now_or_never().is_none());
    // Let any driver-owned Drop rollback complete. This is deliberately NOT
    // cleanup proof, and neither the host nor H may change its disposition.
    let row = fixture
        .db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM bro_managed_sessions",
        ))
        .await?
        .context("journal count")?;
    ensure!(row.try_get::<i64>("", "count")? == 0);
    fixture.db.clone().close().await?;
    ensure!(fixture.service.cleanup.disposition() == Disposition::Ambiguous);
    ensure!(rustix::process::test_kill_process(pid).is_ok());
    ensure!(provider.calls() == 0);
    provider.finish().await?;
    // Every normal Service/App/proof handle drops here. Only the production
    // native owner and H actor retain the actual child and file lease.
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native36_custody_child() -> Result<()> {
    let Some(root) = std::env::var_os(ROOT) else {
        return Ok(());
    };
    let mode = std::env::var(MODE)?;
    // The parent process imposes a wall-clock bound. An outer Tokio deadline
    // would be advanced by the deliberate virtual-time shutdown probe below.
    exercise(Path::new(&root), &mode).await?;
    // libtest can leave its test-name prefix on the current line.
    println!("\n{READY}");
    std::io::stdout().flush()?;
    let mut input = BufReader::new(tokio::io::stdin().take(32));
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(20), input.read_line(&mut line)).await??;
    Ok(())
}

#[tokio::test]
#[ignore = "requires prebuilt harness_mcp_fixture via BITROUTER_TEST_EXAMPLES_DIR"]
async fn native36_real_child_and_file_owner_follow_actual_cleanup_disposition() -> Result<()> {
    config()?;
    for mode in [
        "revoked-before-spawn",
        "discovery-unwind",
        "discovery-cleanup-unknown",
        "host-unwind",
        "transaction-unwind",
        "commit-error",
        "dml-rollback",
    ] {
        isolated(mode).await?;
    }
    Ok(())
}

async fn isolated(mode: &str) -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "tenancy::tests::execution::native::session::custody::native36_custody_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ROOT, root.path())
        .env(MODE, mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut output = BufReader::new(child.stdout.take().context("child stdout")?.take(8193));
    let stderr = child.stderr.take().context("child stderr")?;
    let diagnostics = AbortOnDropHandle::new(tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.take(16_385).read_to_end(&mut bytes).await?;
        Ok::<_, std::io::Error>(bytes)
    }));
    let signal = tokio::time::timeout(Duration::from_secs(40), async {
        let mut bytes = 0;
        loop {
            let mut line = String::new();
            let read = output.read_line(&mut line).await?;
            ensure!(read != 0, "{mode}: child exited before custody signal");
            bytes += read;
            ensure!(bytes <= 8192, "child diagnostics bound");
            if line.trim() == READY {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await
    .context("native custody signal deadline")
    .and_then(|result| result);
    if let Err(error) = signal {
        let detail = child_diagnostics(diagnostics).await;
        return Err(error.context(format!("{mode}: {detail}")));
    }
    let owner = crate::tenancy::ownership::acquire_path(&root.path().join("owned.db"), false);
    if matches!(
        mode,
        "dml-rollback" | "revoked-before-spawn" | "http-confirmed" | "journal-exact"
    ) {
        drop(owner?);
    } else {
        let failure = owner
            .err()
            .context("native unknown/unwind released file owner")?;
        ensure!(
            matches!(
                failure.downcast_ref::<std::fs::TryLockError>(),
                Some(std::fs::TryLockError::WouldBlock)
            ),
            "file probe failed for a reason other than retained ownership"
        );
        if !mode.starts_with("http-") && !mode.starts_with("journal-") {
            let pid = child_pid(&root.path().join("workspace"))?;
            if mode == "discovery-cleanup-unknown" {
                ensure!(rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH));
            } else {
                ensure!(rustix::process::test_kill_process(pid).is_ok());
            }
        }
    }
    // EOF ends the isolated runtime, not an API acknowledgement of cleanup.
    // The fixture has no descendants and exits on the owner's stdin closure.
    drop(child.stdin.take());
    let status = tokio::time::timeout(TIMEOUT, child.wait()).await;
    let detail = child_diagnostics(diagnostics).await;
    let status = status
        .context("native custody child exit deadline")
        .and_then(|result| result.map_err(anyhow::Error::from))
        .with_context(|| format!("{mode}: {detail}"))?;
    ensure!(
        status.success(),
        "{mode}: custody child failed: {status}; {detail}"
    );
    drop(output);
    drop(crate::tenancy::ownership::acquire_path(
        &root.path().join("owned.db"),
        false,
    )?);
    Ok(())
}

// Read only bounded diagnostics. This observer neither joins production owners
// nor acknowledges cleanup. No raw stderr is emitted: errors can contain issued
// credentials, provider payloads or journal material, including unexpected ones.
async fn child_diagnostics(task: AbortOnDropHandle<std::io::Result<Vec<u8>>>) -> String {
    match tokio::time::timeout(Duration::from_secs(2), task).await {
        Ok(Ok(Ok(bytes))) => {
            let text = String::from_utf8_lossy(&bytes);
            let category = [
                (
                    "no such table: tenancy_installation",
                    "missing tenancy installation table",
                ),
                (
                    "no such table: bro_managed_sessions",
                    "missing native journal table",
                ),
                ("database is locked", "SQLite database locked"),
                (
                    "FOREIGN KEY constraint failed",
                    "SQLite foreign-key failure",
                ),
                (
                    "host drain did not close producers",
                    "host producer-close assertion",
                ),
                (
                    "quarantined host reported clean shutdown",
                    "quarantine drain assertion",
                ),
                ("deadline has elapsed", "child deadline elapsed"),
                ("tenancy failure:", "typed tenancy failure"),
                ("panicked at", "child unwound"),
            ]
            .into_iter()
            .find_map(|(needle, label)| text.contains(needle).then_some(label))
            .unwrap_or("unclassified child failure");
            format!(
                "stderr class={category}, bytes={}, truncated={}, sha256={} (raw redacted)",
                bytes.len(),
                bytes.len() > 16_384,
                crate::storage::checkpoint::sha256(&bytes)
            )
        }
        Ok(Ok(Err(error))) => format!("stderr read failed: {:?}", error.kind()),
        Ok(Err(_)) => "stderr observer did not join".into(),
        Err(_) => "stderr observer exceeded its 2s diagnostic deadline".into(),
    }
}
