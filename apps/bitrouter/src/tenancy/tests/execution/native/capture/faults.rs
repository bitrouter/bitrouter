//! Actual capture owner/actor faults in bounded isolated runtimes.
mod child;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod reap;
mod transaction;

use super::*;
use crate::tenancy::transaction::{Disposition, Phase};
use futures::FutureExt;
use std::{io::Write, process::Stdio};
use tokio::io::{AsyncBufReadExt, BufReader};

const ROOT: &str = "BITROUTER_CAPTURE_FAULT_ROOT";
const MODE: &str = "BITROUTER_CAPTURE_FAULT_MODE";
const READY: &str = "capture-fault-owner-checked";
const CHILD: &str = "tenancy::tests::execution::native::capture::faults::capture_fault_child";

async fn file_fixture(provider: &Provider, root: &Path) -> Result<(Fixture, ResourceRef)> {
    let database = root.join("owned.db");
    let url = format!("sqlite://{}", database.display());
    let owner = crate::tenancy::ownership::acquire(&url)?;
    let db = crate::db::connect_tenancy(&url, &owner).await?;
    // Provision directly on the leased file. VACUUM INTO from an in-memory
    // connection can inherit SQLITE_OPEN_MEMORY instead of producing a file.
    let fixture = Fixture::with_database(db, OperationLimits::default(), &mut |_| {}).await?;
    let (mut fixture, route, _) =
        crate::tenancy::tests::execution::provision_protocol_with_fixture(
            fixture,
            &provider.url,
            "chat_completions",
            &mut |_| {},
        )
        .await?;
    fixture.service.shutdown().await?;
    let metadata = std::fs::metadata(&owner.path)?;
    ensure!(
        metadata.is_file() && metadata.len() > 0,
        "authority file was not materialized"
    );
    // Verify SQLite's actual main filename, not only a sidecar ownership lock.
    // https://www.sqlite.org/pragma.html#pragma_database_list
    let databases = fixture
        .db
        .query_all(Statement::from_string(
            DatabaseBackend::Sqlite,
            "PRAGMA database_list",
        ))
        .await?;
    let mut main = None;
    for row in databases {
        if row.try_get::<String>("", "name")? == "main" {
            ensure!(main.is_none(), "duplicate main database");
            main = Some(row.try_get::<String>("", "file")?);
        }
    }
    let main = main.context("SQLite main database missing")?;
    ensure!(!main.is_empty() && Path::new(&main).canonicalize()? == owner.path);
    let initial = store::load(&fixture.db, chrono::Utc::now()).await?;
    ensure!(
        initial.installation == fixture.installation,
        "persisted installation changed"
    );
    fixture.service = Service::start_prepared(
        fixture.db.clone(),
        Some(owner),
        OperationLimits::default(),
        initial,
        Arc::new(Resources::with_key(fixture.installation.clone(), [29; 32])?),
    )
    .await?;
    Ok((fixture, route))
}

async fn no_clean_drain(fixture: &Fixture, must_wait: bool) -> Result<()> {
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
        // Already quarantined shutdown may fail synchronously. Only advance
        // time if the SAME shutdown future is still pending after closing hosts.
        let early = early.or_else(|| shutdown.as_mut().now_or_never());
        ensure!(stop.is_cancelled(), "shutdown did not stop host producers");
        match early {
            Some(result) => {
                ensure!(result.is_err(), "unconfirmed capture reported clean drain");
                ensure!(
                    !must_wait,
                    "held reaper shutdown failed before its cleanup deadline"
                );
            }
            None => {
                tokio::time::advance(Duration::from_secs(31)).await;
                ensure!(
                    shutdown.await.is_err(),
                    "unconfirmed capture reported clean drain"
                );
            }
        }
        Ok(())
    }
    .await;
    tokio::time::resume();
    checked?;
    tokio::time::timeout(TIMEOUT, fixture.service.cleanup.quarantined()).await?;
    retained(fixture)
}

fn retained(fixture: &Fixture) -> Result<()> {
    ensure!(
        fixture.service.current().is_err(),
        "unknown cleanup left authority live"
    );
    ensure!(fixture.service.cleanup.disposition() == Disposition::Ambiguous);
    ensure!(
        fixture.service.host_slots_for_test() == 0,
        "capture host slot returned early"
    );
    ensure!(fixture.service.completion.clone().now_or_never().is_none());
    Ok(())
}

fn pid(marker: &Path) -> Result<rustix::process::Pid> {
    let bytes = std::fs::read(marker)?;
    ensure!(bytes.len() <= 1024, "fixture marker bound");
    let value: Value = serde_json::from_slice(&bytes)?;
    let raw = i32::try_from(value["pid"].as_u64().context("actual child PID")?)?;
    rustix::process::Pid::from_raw(raw).context("valid child PID")
}

fn unconfigured(marker: &Path) -> Result<()> {
    let value: Value = serde_json::from_slice(&std::fs::read(marker)?)?;
    ensure!(value["provider_set"] == 0 && value["inherited_home"] == false);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "only the bounded parent invokes this isolated custody runtime"]
async fn capture_fault_child() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(ROOT).context("fault root")?);
    let mode = std::env::var(MODE)?;
    let _initializer = match mode.as_str() {
        "commit-error" | "late-commit" => {
            transaction::exercise(&root, &mode).await?;
            None
        }
        "reaper-unwind" | "reaper-held" => child::exercise(&root, &mode).await?,
        _ => anyhow::bail!("unknown capture fault case"),
    };
    // All ordinary fixture/App/Service/DB/proof handles have now dropped. Only
    // production H and host custody may keep the canonical file owner alive.
    println!("{READY}");
    std::io::stdout().flush()?;
    let mut input = BufReader::new(tokio::io::stdin().take(32));
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(20), input.read_line(&mut line)).await??;
    Ok(())
}

#[tokio::test]
async fn capture_consuming_commit_error_keeps_the_real_actor_file_owner() -> Result<()> {
    isolated("commit-error").await
}

#[tokio::test]
async fn capture_late_commit_is_durable_but_cannot_claim_clean_host_completion() -> Result<()> {
    isolated("late-commit").await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn capture_caught_reaper_unwind_retains_the_actual_child_and_file_owner() -> Result<()> {
    executable()?;
    isolated("reaper-unwind").await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn capture_missed_child_cleanup_keeps_owner_even_after_the_same_child_is_reaped() -> Result<()>
{
    executable()?;
    isolated("reaper-held").await
}

pub(super) async fn diagnostics(reader: impl tokio::io::AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(8193).read_to_end(&mut bytes).await?;
    ensure!(
        bytes.len() <= 8192,
        "fault child diagnostic bound: {}",
        String::from_utf8_lossy(&bytes[..8192.min(bytes.len())])
    );
    Ok(bytes)
}

async fn isolated(mode: &str) -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut process = tokio::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            CHILD,
            "--ignored",
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
    let mut output = process
        .stdout
        .take()
        .map(|stdout| BufReader::new(stdout.take(8193)));
    let errors = process
        .stderr
        .take()
        .map(|stderr| AbortOnDropHandle::new(tokio::spawn(diagnostics(stderr))));
    let checked: Result<()> = async {
        // Capture even pipe setup failures; every post-spawn Result path below
        // must still attempt runtime reaping and the distinct ACP group cleanup.
        let output = output.as_mut().context("retained child stdout")?;
        ensure!(errors.is_some(), "child stderr missing");
        tokio::time::timeout(Duration::from_secs(50), async {
            let mut bytes = 0;
            loop {
                let mut line = String::new();
                let read = output.read_line(&mut line).await?;
                ensure!(read != 0, "{mode}: exited before ownership witness");
                bytes += read;
                ensure!(bytes <= 8192, "fault child stdout bound");
                // A single-thread libtest runner may prefix this same line
                // with the current test name before the child's println.
                if line.trim_end().ends_with(READY) {
                    return Ok::<_, anyhow::Error>(());
                }
            }
        })
        .await??;
        let error = crate::tenancy::ownership::acquire_path(&root.path().join("owned.db"), false)
            .err()
            .context("ordinary handle drop released unconfirmed capture owner")?;
        ensure!(matches!(
            error.downcast_ref::<std::fs::TryLockError>(),
            Some(std::fs::TryLockError::WouldBlock)
        ));
        if mode.starts_with("reaper-") {
            let process = pid(&root.path().join("child.json"))?;
            unconfigured(&root.path().join("child.json"))?;
            if mode == "reaper-unwind" {
                ensure!(
                    rustix::process::test_kill_process_group(process).is_ok(),
                    "caught unwind dropped the actual child"
                );
            } else {
                ensure!(
                    rustix::process::test_kill_process_group(process)
                        == Err(rustix::io::Errno::SRCH)
                );
            }
        }
        Ok(())
    }
    .await;
    // Always end/reap the isolated runtime, including assertion failures. EOF is
    // containment, never an API acknowledgement that quarantined cleanup passed.
    drop(process.stdin.take());
    let runtime = reap_runtime(&mut process).await;
    // The ACP process has its own group. Neither runtime exit nor a failed
    // kill/wait can skip this independent bounded containment attempt.
    let reaped = cleanup_peer(&root.path().join("child.json")).await;
    let diagnostic: Result<Vec<u8>> = async {
        let Some(errors) = errors else {
            return Ok(Vec::new());
        };
        tokio::time::timeout(Duration::from_secs(5), errors)
            .await
            .context("fault child diagnostics deadline")?
            .context("fault child diagnostic task failed")?
    }
    .await;
    let released = crate::tenancy::ownership::acquire_path(&root.path().join("owned.db"), false)
        .map(drop)
        .context("file owner remained after isolated runtime containment");
    let mut failures = [checked, runtime, reaped, released]
        .into_iter()
        .filter_map(Result::err)
        .map(|error| format!("{error:#}"))
        .collect::<Vec<_>>();
    let diagnostic = match diagnostic {
        Ok(bytes) => {
            if mode == "commit-error" && !bytes.is_empty() {
                failures.push("commit-error child wrote unexpected stderr".to_owned());
            }
            String::from_utf8_lossy(&bytes).into_owned()
        }
        Err(error) => {
            failures.push(format!("{error:#}"));
            String::new()
        }
    };
    if !failures.is_empty() {
        // Keep the marker and exact filename if any cleanup remains unknown;
        // a failed kill request never becomes a production cleanup ACK.
        let retained = root.keep();
        anyhow::bail!(
            "{mode}: {}; fixture retained at {}; child diagnostics: {diagnostic}",
            failures.join("; "),
            retained.display()
        );
    }
    Ok(())
}

pub(super) async fn reap_runtime(process: &mut tokio::process::Child) -> Result<()> {
    let first = match tokio::time::timeout(Duration::from_secs(5), process.wait()).await {
        Ok(Ok(status)) => {
            ensure!(status.success(), "fault child failed: {status}");
            return Ok(());
        }
        Ok(Err(error)) => format!("isolated runtime wait failed: {error}"),
        Err(_) => "isolated runtime did not exit within five seconds".to_owned(),
    };
    let mut failures = vec![first];
    if let Err(error) = process.start_kill() {
        failures.push(format!("isolated runtime kill failed: {error}"));
    }
    // Still await the same Child after a failed kill request. Retain every
    // error for the caller, which attempts ACP cleanup before propagating it.
    match tokio::time::timeout(Duration::from_secs(5), process.wait()).await {
        Ok(Ok(status)) => {
            if !status.success() {
                failures.push(format!("isolated runtime forced exit: {status}"));
            }
        }
        Ok(Err(error)) => failures.push(format!("isolated runtime reap failed: {error}")),
        Err(_) => failures.push("isolated runtime forced reap deadline exceeded".into()),
    }
    anyhow::bail!("{}", failures.join("; "))
}

pub(super) async fn cleanup_peer(marker: &Path) -> Result<()> {
    if !marker.exists() {
        return Ok(());
    }
    let child = pid(marker)?;
    // The quarantined child has no descendants. On failure/runtime teardown,
    // explicitly contain its real process group as well as the test runtime.
    match rustix::process::kill_process_group(child, rustix::process::Signal::KILL) {
        Ok(()) | Err(rustix::io::Errno::SRCH) => {}
        Err(error) => return Err(error.into()),
    }
    tokio::time::timeout(TIMEOUT, async {
        loop {
            match rustix::process::test_kill_process_group(child) {
                Err(rustix::io::Errno::SRCH) => return Ok::<_, anyhow::Error>(()),
                Ok(()) => tokio::time::sleep(Duration::from_millis(5)).await,
                Err(error) => return Err(error.into()),
            }
        }
    })
    .await?
}
