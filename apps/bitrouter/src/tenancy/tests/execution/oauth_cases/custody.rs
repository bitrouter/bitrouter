//! Actual replacement transaction failures retain the same serving H/file owner.
use super::*;
use crate::tenancy::transaction::{Disposition, Phase};
use futures::FutureExt;
use sea_orm::{ConnectionTrait, Statement};
use std::{io::Write, path::Path, process::Stdio};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
const DATABASE: &str = "BITROUTER_TEST_OAUTH037_DATABASE";
const FAULT: &str = "BITROUTER_TEST_OAUTH037_FAULT";
const RETAINED: &str = "oauth037-unknown-owner-retained";
const WAIT: Duration = TIMEOUT;
#[derive(Clone, Copy)]
enum Fault {
    Commit,
    Rollback,
    Unwind,
}
impl Fault {
    fn name(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Rollback => "rollback",
            Self::Unwind => "unwind",
        }
    }
    fn parse(value: &str) -> Result<Self> {
        match value {
            "commit" => Ok(Self::Commit),
            "rollback" => Ok(Self::Rollback),
            "unwind" => Ok(Self::Unwind),
            _ => anyhow::bail!("unknown OAuth fault"),
        }
    }
    fn phase(self) -> Phase {
        match self {
            Self::Commit => Phase::Committing,
            Self::Rollback | Self::Unwind => Phase::RollingBack,
        }
    }
    async fn install(self, harness: &Harness) -> Result<()> {
        match self {
            Self::Commit => {
                // Deferred FK is accepted by UPDATE; the actual consuming COMMIT fails.
                for sql in [
                    "PRAGMA foreign_keys=ON",
                    "CREATE TABLE oauth037_parent(id INTEGER PRIMARY KEY)",
                    "CREATE TABLE oauth037_child(parent INTEGER REFERENCES oauth037_parent(id) DEFERRABLE INITIALLY DEFERRED)",
                    "CREATE TRIGGER oauth037_commit_fault AFTER UPDATE ON tenancy_oauth_slots WHEN NEW.generation=1 BEGIN INSERT INTO oauth037_child(parent) VALUES (77); END",
                ] {
                    harness.fixture.db.execute_unprepared(sql).await?;
                }
                let row = harness
                    .fixture
                    .db
                    .query_one(Statement::from_string(
                        harness.fixture.db.get_database_backend(),
                        "PRAGMA foreign_keys",
                    ))
                    .await?
                    .context("foreign key setting")?;
                ensure!(row.try_get::<i64>("", "foreign_keys")? == 1);
            }
            Self::Rollback => {
                harness.fixture.db.execute_unprepared("CREATE TRIGGER oauth037_rollback_fault BEFORE UPDATE ON tenancy_oauth_slots WHEN NEW.generation=1 BEGIN SELECT RAISE(ROLLBACK,'OAuth rollback fixture'); END").await?;
            }
            Self::Unwind => {
                harness.fixture.db.execute_unprepared("CREATE TRIGGER oauth037_unwind_fault BEFORE UPDATE ON tenancy_oauth_slots WHEN NEW.generation=1 BEGIN SELECT RAISE(ABORT,'OAuth unwind fixture'); END").await?;
            }
        }
        Ok(())
    }
}
async fn exercise(path: &Path, fault: Fault) -> Result<()> {
    let mut harness = Harness::with_path(true, false, Some(path)).await?;
    let client = Harness::client(false)?;
    let request = harness.send(&client, false);
    let caller = AbortOnDropHandle::new(tokio::spawn(async move {
        let response = request.send().await?;
        let status = response.status();
        response.bytes().await?;
        Ok::<_, anyhow::Error>(status)
    }));
    tokio::time::timeout(WAIT, harness.refresh_seen.recv())
        .await?
        .context("actual refresh POST")?;
    // The durable start has already committed. Faults affect generation1 only,
    // so neither startup nor unrelated authority commands can satisfy this test.
    fault.install(&harness).await?;
    let gate = harness.fixture.service.cleanup.hold(fault.phase());
    harness.provider.refresh_gate.add_permits(1);
    tokio::time::timeout(WAIT, gate.entered.notified()).await?;
    ensure!(harness.fixture.service.cleanup.disposition() == Disposition::Pending);
    if matches!(fault, Fault::Unwind) {
        harness.fixture.service.cleanup.unwind_gate()?;
    }
    harness.fixture.service.cleanup.release_gate();
    let status = tokio::time::timeout(WAIT, caller).await???;
    ensure!(!status.is_success());
    tokio::time::timeout(WAIT, harness.fixture.service.cleanup.quarantined()).await?;
    ensure!(harness.fixture.service.cleanup.disposition() == Disposition::Ambiguous);
    ensure!(
        harness
            .fixture
            .service
            .admission
            .wait_drained()
            .now_or_never()
            .is_none()
    );
    ensure!(
        tokio::time::timeout(WAIT, harness.fixture.service.shutdown())
            .await?
            .is_err()
    );
    ensure!(
        harness.provider.refreshes.load(Ordering::SeqCst) == 1
            && harness.provider.models.load(Ordering::SeqCst) == 0
    );
    ensure!(harness.row().await?.metadata.generation == 0);
    let db = harness.fixture.db.clone();
    let proof = harness.fixture.service.cleanup.clone();
    drop(client);
    harness.close().await?;
    tokio::time::timeout(WAIT, db.close()).await??;
    ensure!(proof.disposition() == Disposition::Ambiguous && proof.ensure_clean().is_err());
    drop((proof, gate));
    Ok(())
}
#[tokio::test]
#[ignore = "invoked only by OAuth fault parents with their owned file database"]
async fn oauth_unknown_owner_child() -> Result<()> {
    let path = std::env::var_os(DATABASE).context("isolated OAuth database")?;
    let fault = Fault::parse(&std::env::var(FAULT)?)?;
    tokio::time::timeout(Duration::from_secs(25), exercise(Path::new(&path), fault)).await??;
    println!("\n{RETAINED}:{}", fault.name());
    std::io::stdout().flush()?;
    let mut input = BufReader::new(tokio::io::stdin().take(32));
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(20), input.read_line(&mut line)).await??;
    Ok(())
}
async fn retained_owner(fault: Fault) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("oauth037-custody.sqlite");
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "tenancy::tests::execution::oauth_cases::custody::oauth_unknown_owner_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(DATABASE, &database)
        .env(FAULT, fault.name())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdout = BufReader::new(child.stdout.take().context("child stdout")?.take(8193));
    let stderr = child.stderr.take().context("child stderr")?;
    let diagnostics = AbortOnDropHandle::new(tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.take(16_385).read_to_end(&mut bytes).await?;
        Ok::<_, std::io::Error>(bytes)
    }));
    let result: Result<()> = async {
        let expected = format!("{RETAINED}:{}", fault.name());
        tokio::time::timeout(Duration::from_secs(30), async {
            let mut total = 0usize;
            loop {
                let mut line = String::new();
                let count = stdout.read_line(&mut line).await?;
                ensure!(
                    count != 0,
                    "child exited before OAuth custody acknowledgement"
                );
                total = total.checked_add(count).context("diagnostic size")?;
                ensure!(total <= 8192, "child diagnostics exceeded bound");
                if line.trim() == expected {
                    break Ok::<_, anyhow::Error>(());
                }
            }
        })
        .await??;
        match crate::tenancy::ownership::acquire_path(&database, false) {
            Err(error) => ensure!(
                matches!(
                    error.downcast_ref::<std::fs::TryLockError>(),
                    Some(std::fs::TryLockError::WouldBlock)
                ),
                "unrelated ownership probe error"
            ),
            Ok(owner) => {
                drop(owner);
                anyhow::bail!("uncertain OAuth released its file owner");
            }
        }
        Ok(())
    }
    .await;
    // Both assertion outcomes release the child waiter and reap it. Forced exit
    // is containment only and cannot turn a failed custody assertion into pass.
    drop(child.stdin.take());
    let status = match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(status) => status.map_err(anyhow::Error::from),
        Err(error) => {
            child.kill().await?;
            tokio::time::timeout(WAIT, child.wait()).await??;
            Err(error.into())
        }
    };
    let stderr = tokio::time::timeout(WAIT, diagnostics).await???;
    ensure!(stderr.len() <= 16_384, "child stderr exceeded bound");
    result.with_context(|| {
        format!(
            "OAuth child diagnostics: {}",
            String::from_utf8_lossy(&stderr)
        )
    })?;
    ensure!(
        status?.success(),
        "OAuth custody child failed: {}",
        String::from_utf8_lossy(&stderr)
    );
    drop(stdout);
    drop(crate::tenancy::ownership::acquire_path(&database, false)?);
    Ok(())
}
#[tokio::test]
async fn consuming_oauth_commit_error_keeps_actual_file_owner() -> Result<()> {
    retained_owner(Fault::Commit).await
}
#[tokio::test]
async fn consuming_oauth_rollback_error_keeps_actual_file_owner() -> Result<()> {
    retained_owner(Fault::Rollback).await
}
#[tokio::test]
async fn oauth_rollback_unwind_keeps_actual_file_owner() -> Result<()> {
    retained_owner(Fault::Unwind).await
}
