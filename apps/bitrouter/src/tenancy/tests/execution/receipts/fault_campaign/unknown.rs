//! Real receipt SQL failures keep the same actor/file owner until process exit.
use super::{
    fixture::{Harness, Protocol, Release, client, identity},
    *,
};
use crate::tenancy::transaction::{Disposition, Phase};
use std::{io::Write, path::Path, process::Stdio};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

const DATABASE: &str = "BITROUTER_TEST_RECEIPT035_FAULT_DATABASE";
const FAULT: &str = "BITROUTER_TEST_RECEIPT035_FAULT";
const RETAINED: &str = "receipt035-unknown-owner-retained";
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
            _ => anyhow::bail!("unsupported receipt fault"),
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
                // Deferred FK violation is accepted by INSERT and rejected by
                // actual COMMIT. The trigger itself does not raise an error.
                for sql in [
                    "PRAGMA foreign_keys=ON",
                    "CREATE TABLE receipt035_parent(id INTEGER PRIMARY KEY)",
                    "CREATE TABLE receipt035_child(parent INTEGER REFERENCES receipt035_parent(id) DEFERRABLE INITIALLY DEFERRED)",
                    "CREATE TRIGGER receipt035_commit_fault AFTER INSERT ON tenancy_execution_receipts BEGIN INSERT INTO receipt035_child(parent) VALUES (77); END",
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
                // SQLite ends the transaction, while SQLx's transaction depth
                // remains positive. The consuming ORM ROLLBACK then fails.
                harness.fixture.db.execute_unprepared("CREATE TRIGGER receipt035_rollback_fault BEFORE INSERT ON tenancy_execution_receipts BEGIN SELECT RAISE(ROLLBACK, 'receipt rollback fixture'); END").await?;
            }
            Self::Unwind => {
                // Ordinary DML refusal reaches the real rollback await. The
                // existing H gate then unwinds inside its supervised future.
                harness.fixture.db.execute_unprepared("CREATE TRIGGER receipt035_unwind_fault BEFORE INSERT ON tenancy_execution_receipts BEGIN SELECT RAISE(ABORT, 'receipt unwind fixture'); END").await?;
            }
        }
        Ok(())
    }
}
async fn exercise(path: &Path, fault: Fault) -> Result<()> {
    let mut harness = Harness::new(Protocol::Chat, Some(path)).await?;
    fault.install(&harness).await?;
    let http = client(false)?;
    let request = harness.request(&http, false);
    let caller = AbortOnDropHandle::new(tokio::spawn(async move {
        let mut response = request.send().await?;
        let id = identity(&response, false)?;
        let status = response.status();
        let mut size = 0usize;
        while let Some(bytes) = response.chunk().await? {
            size = size
                .checked_add(bytes.len())
                .context("fault response size")?;
            ensure!(size <= WIRE_LIMIT);
        }
        Ok::<_, anyhow::Error>((status, id))
    }));
    harness.provider.accepted().await?;
    // Chat has no034 publication. All credential reads precede this accepted
    // POST; the next consuming transaction can only be its actual035 receipt.
    let gate = harness.fixture.service.cleanup.hold(fault.phase());
    let release = Release(harness.fixture.service.clone());
    harness.provider.open.cancel();
    tokio::time::timeout(WAIT, gate.entered.notified()).await?;
    ensure!(harness.fixture.service.cleanup.disposition() == Disposition::Pending);
    if matches!(fault, Fault::Unwind) {
        harness.fixture.service.cleanup.unwind_gate()?;
    }
    drop(release);
    let (status, id) = tokio::time::timeout(WAIT, caller).await???;
    ensure!(
        !status.is_success(),
        "uncertain receipt exposed successful output"
    );
    tokio::time::timeout(WAIT, harness.fixture.service.cleanup.quarantined()).await?;
    ensure!(harness.fixture.service.cleanup.disposition() == Disposition::Ambiguous);
    ensure!(harness.fixture.service.cleanup.ensure_clean().is_err());
    ensure!(harness.fixture.service.current().is_err());
    ensure!(!harness.fixture.service.admission.receipts_ready());
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
        store::lookup(
            &harness.fixture.db,
            &harness.fixture.installation,
            &harness.fixture.tenant,
            &id
        )
        .await?
        .is_none()
    );
    ensure!(harness.count("tenancy_execution_receipts").await? == 0);
    ensure!(harness.count("provider_continuations").await? == 0);
    let mut rejected = harness.request(&http, false).send().await?;
    ensure!(!rejected.status().is_success());
    let mut size = 0usize;
    while let Some(bytes) = rejected.chunk().await? {
        size = size
            .checked_add(bytes.len())
            .context("rejected response size")?;
        ensure!(size <= WIRE_LIMIT);
    }
    ensure!(
        harness.provider.calls.load(Ordering::SeqCst) == 1,
        "uncertain receipt repeated inference"
    );
    // A zero row count is not a cleanup proof. Remove normal service, router,
    // request and pool handles before the parent probes actual lease custody.
    let db = harness.fixture.db.clone();
    let proof = harness.fixture.service.cleanup.clone();
    harness.close().await?;
    tokio::time::timeout(WAIT, db.close()).await??;
    ensure!(proof.disposition() == Disposition::Ambiguous && proof.ensure_clean().is_err());
    drop((proof, http, gate));
    Ok(())
}
#[tokio::test]
#[ignore = "invoked only by receipt fault parents with their owned file database"]
async fn receipt_unknown_owner_child() -> Result<()> {
    let path = std::env::var_os(DATABASE).context("isolated receipt database")?;
    let fault = Fault::parse(&std::env::var(FAULT)?)?;
    tokio::time::timeout(Duration::from_secs(25), exercise(Path::new(&path), fault)).await??;
    // Serial libtest writes a non-newline test-name prefix on stdout.
    println!("\n{RETAINED}:{}", fault.name());
    std::io::stdout().flush()?;
    let mut input = BufReader::new(tokio::io::stdin().take(32));
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(20), input.read_line(&mut line)).await??;
    Ok(())
}
async fn retained_owner(fault: Fault) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("receipt035-custody.sqlite");
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "tenancy::tests::execution::receipts::fault_campaign::unknown::receipt_unknown_owner_child", "--ignored", "--nocapture", "--test-threads=1"])
        .env(DATABASE, &database).env(FAULT, fault.name())
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).spawn()?;
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
                    "child exited before receipt custody acknowledgement"
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
                anyhow::bail!("uncertain receipt released its file owner");
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
            "receipt child diagnostics: {}",
            String::from_utf8_lossy(&stderr)
        )
    })?;
    ensure!(
        status?.success(),
        "receipt custody child failed: {}",
        String::from_utf8_lossy(&stderr)
    );
    drop(stdout);
    drop(crate::tenancy::ownership::acquire_path(&database, false)?);
    Ok(())
}
#[tokio::test]
async fn consuming_receipt_commit_error_keeps_actual_file_owner() -> Result<()> {
    retained_owner(Fault::Commit).await
}
#[tokio::test]
async fn consuming_receipt_rollback_error_keeps_actual_file_owner() -> Result<()> {
    retained_owner(Fault::Rollback).await
}
#[tokio::test]
async fn receipt_rollback_unwind_keeps_actual_file_owner() -> Result<()> {
    retained_owner(Fault::Unwind).await
}
