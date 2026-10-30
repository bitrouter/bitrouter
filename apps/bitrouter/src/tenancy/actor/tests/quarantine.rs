//! A child process permits testing permanent quarantine without an escape hatch.

use super::*;
use std::io::Write;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

const CHILD_DATABASE: &str = "BITROUTER_TEST_CUSTODY_DATABASE";
const CHILD_FAULT: &str = "BITROUTER_TEST_CUSTODY_FAULT";
const READY: &str = "custody-owner-retained";

#[tokio::test]
async fn quarantined_owner_child() -> Result<()> {
    let Some(path) = std::env::var_os(CHILD_DATABASE) else {
        return Ok(());
    };
    let path = std::path::PathBuf::from(path);
    let owner = ownership::acquire_path(&path, true)?;
    let url = format!("sqlite://{}", path.display());
    let db = crate::db::connect_tenancy(&url, &owner).await?;
    let fixture = Fixture::open(db, Some(owner)).await?;
    let fault = std::env::var(CHILD_FAULT)?;
    let response = match fault.as_str() {
        "commit" => {
            super::hardening::commit_fault(&fixture.db).await?;
            fixture.enqueue(super::hardening::principal(&fixture, "deferred-commit")?)?
        }
        "unwind" => {
            let gate = fixture
                .service
                .cleanup
                .hold(crate::tenancy::transaction::Phase::Active);
            let response =
                fixture.enqueue(super::hardening::principal(&fixture, "unwound-command")?)?;
            tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await?;
            fixture.service.cleanup.unwind_gate()?;
            response
        }
        _ => anyhow::bail!("unknown custody fault"),
    };
    ensure!(
        tokio::time::timeout(Duration::from_secs(5), response)
            .await??
            .is_err()
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        fixture.service.cleanup.quarantined(),
    )
    .await?;
    ensure!(fixture.service.shutdown().await.is_err());
    // Close the pool and remove every normal Service/db owner. The quarantined
    // actor still must retain authority ownership after those normal handles close.
    tokio::time::timeout(Duration::from_secs(5), fixture.db.clone().close()).await??;
    drop(fixture);
    println!("{READY}");
    std::io::stdout().flush()?;
    let mut input = BufReader::new(tokio::io::stdin().take(32));
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(20), input.read_line(&mut line)).await??;
    Ok(())
}

async fn retained_owner(fault: &str) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("quarantine.sqlite");
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "tenancy::actor::tests::quarantine::quarantined_owner_child",
            "--nocapture",
        ])
        .env(CHILD_DATABASE, &database)
        .env(CHILD_FAULT, fault)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let output = child.stdout.take().context("child stdout missing")?;
    let mut output = BufReader::new(output.take(8193));
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut bytes = 0;
        loop {
            let mut line = String::new();
            let read = output.read_line(&mut line).await?;
            ensure!(read != 0, "child exited before retaining owner");
            bytes += read;
            ensure!(bytes <= 8192, "child diagnostics exceeded bound");
            if line.trim() == READY {
                return Ok::<(), anyhow::Error>(());
            }
        }
    })
    .await??;
    ensure!(
        ownership::acquire_path(&database, false).is_err(),
        "quarantine released the owner after failed transaction work"
    );
    // EOF lets only the test child process exit; no production un-fence is added.
    drop(child.stdin.take());
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait()).await??;
    ensure!(status.success(), "quarantine child failed: {status}");
    drop(output);
    let next = ownership::acquire_path(&database, false)?;
    drop(next);
    Ok(())
}

#[tokio::test]
async fn failed_commit_and_unwind_keep_owner_after_normal_handles_close() -> Result<()> {
    retained_owner("commit").await?;
    retained_owner("unwind").await
}
