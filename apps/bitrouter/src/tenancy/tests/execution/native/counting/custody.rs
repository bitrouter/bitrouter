//! Actual count+generation terminal failures retain the same serving H/file owner.
use super::*;
use crate::tenancy::transaction::{Disposition, Phase};
use futures::FutureExt;
use sea_orm::{ConnectionTrait, Statement};
use std::{io::Write, path::Path, process::Stdio};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
const DATABASE: &str = "BITROUTER_TEST_COUNT_C1_DATABASE";
const FAULT: &str = "BITROUTER_TEST_COUNT_C1_FAULT";
const RETAINED: &str = "count_c1-unknown-owner-retained";
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
            _ => anyhow::bail!("unknown count fault"),
        }
    }
    fn phase(self) -> Phase {
        match self {
            Self::Commit => Phase::Committing,
            Self::Rollback | Self::Unwind => Phase::RollingBack,
        }
    }
    async fn install(self, fixture: &Fixture) -> Result<()> {
        match self {
            Self::Commit => {
                // Deferred FK is accepted by the auxiliary INSERT; consuming COMMIT fails.
                for sql in [
                    "PRAGMA foreign_keys=ON",
                    "CREATE TABLE count_c1_parent(id INTEGER PRIMARY KEY)",
                    "CREATE TABLE count_c1_child(parent INTEGER REFERENCES count_c1_parent(id) DEFERRABLE INITIALLY DEFERRED)",
                    "CREATE TRIGGER count_c1_commit_fault AFTER INSERT ON tenancy_receipt_auxiliary BEGIN INSERT INTO count_c1_child(parent) VALUES (77); END",
                ] {
                    fixture.db.execute_unprepared(sql).await?;
                }
                let row = fixture
                    .db
                    .query_one(Statement::from_string(
                        fixture.db.get_database_backend(),
                        "PRAGMA foreign_keys",
                    ))
                    .await?
                    .context("foreign key setting")?;
                ensure!(row.try_get::<i64>("", "foreign_keys")? == 1);
            }
            Self::Rollback => {
                fixture.db.execute_unprepared("CREATE TRIGGER count_c1_rollback_fault BEFORE INSERT ON tenancy_receipt_auxiliary BEGIN SELECT RAISE(ROLLBACK,'count rollback fixture'); END").await?;
            }
            Self::Unwind => {
                fixture.db.execute_unprepared("CREATE TRIGGER count_c1_unwind_fault BEFORE INSERT ON tenancy_receipt_auxiliary BEGIN SELECT RAISE(ABORT,'count unwind fixture'); END").await?;
            }
        }
        Ok(())
    }
}
async fn exercise(path: &Path, fault: Fault) -> Result<()> {
    let provider = Provider::start(false).await?;
    let owner = crate::tenancy::ownership::acquire_path(path, true)?;
    let db =
        crate::db::connect_tenancy(&format!("sqlite://{}", owner.path.display()), &owner).await?;
    let initial = Fixture::with_database(db, Default::default(), &mut |_| {}).await?;
    let (mut fixture, route, slot) = super::super::super::provision_protocol_with_fixture(
        initial,
        &provider.url,
        "responses",
        &mut |_| {},
    )
    .await?;
    let route = revised_route(
        &mut fixture,
        &route,
        &slot,
        &provider.url,
        &["responses"],
        true,
    )
    .await?;
    fixture.service.shutdown().await?;
    let snapshot = store::load(&fixture.db, chrono::Utc::now()).await?;
    fixture.service = Service::start_prepared(
        fixture.db.clone(),
        Some(owner),
        Default::default(),
        snapshot,
        Arc::new(Resources::with_key(fixture.installation.clone(), [29; 32])?),
    )
    .await?;
    client(&fixture, false)?;
    let (ingress, _) = ready(&fixture, &route).await?;
    let after_count = Arc::new(Gate::default());
    let release = after_count.release.clone().drop_guard();
    let control = Arc::new(CountControl {
        after_gate: Some(after_count.clone()),
        ..Default::default()
    });
    let observed = control.clone();
    let app = host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
        .with_tenancy(fixture.service.clone());
    let caller = AbortOnDropHandle::new(tokio::spawn(async move {
        app.execute_native_controlled_with_headers(
            prompt(),
            CallerContext::local(),
            ingress,
            control,
        )
        .await
    }));
    let checked = async {
        tokio::time::timeout(WAIT, after_count.entered.notified()).await?;
        ensure!(provider.calls() == 1);
        fault.install(&fixture).await?;
        let gate = fixture.service.cleanup.hold(fault.phase());
        after_count.release.cancel();
        tokio::time::timeout(WAIT, gate.entered.notified()).await?;
        ensure!(
            fixture.service.cleanup.disposition() == Disposition::Pending && provider.calls() == 2
        );
        if matches!(fault, Fault::Unwind) {
            fixture.service.cleanup.unwind_gate()?;
        }
        fixture.service.cleanup.release_gate();
        ensure!(tokio::time::timeout(WAIT, caller).await??.is_err());
        tokio::time::timeout(WAIT, fixture.service.cleanup.quarantined()).await?;
        ensure!(fixture.service.cleanup.disposition() == Disposition::Ambiguous);
        ensure!(fixture.service.current().is_err() && !fixture.service.admission.receipts_ready());
        ensure!(
            fixture
                .service
                .admission
                .wait_drained()
                .now_or_never()
                .is_none()
        );
        ensure!(
            tokio::time::timeout(WAIT, fixture.service.shutdown())
                .await?
                .is_err()
        );
        let id: ExecutionId = observed
            .reports
            .lock()
            .await
            .first()
            .context("actual counted execution")?
            .request_id
            .as_str()
            .try_into()?;
        ensure!(observed.attempts.lock().await.len() == 1 && provider.calls() == 2);
        ensure!(
            receipts::lookup(&fixture.db, &fixture.installation, &fixture.tenant, &id)
                .await?
                .is_none()
        );
        for table in ["tenancy_execution_receipts", "tenancy_receipt_auxiliary"] {
            let row = fixture
                .db
                .query_one(Statement::from_string(
                    fixture.db.get_database_backend(),
                    format!("SELECT COUNT(*) AS n FROM {table}"),
                ))
                .await?
                .context("counted rows")?;
            ensure!(row.try_get::<i64>("", "n")? == 0);
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    drop(release);
    fixture.service.cleanup.release_gate();
    let stopped = provider.finish().await;
    let db = fixture.db.clone();
    let proof = fixture.service.cleanup.clone();
    drop(fixture);
    let closed = tokio::time::timeout(WAIT, db.close()).await;
    checked?;
    stopped?;
    closed??;
    ensure!(proof.disposition() == Disposition::Ambiguous && proof.ensure_clean().is_err());
    drop(proof);
    Ok(())
}
#[tokio::test]
#[ignore = "invoked only by count fault parents with their owned file database"]
async fn count_unknown_owner_child() -> Result<()> {
    let path = std::env::var_os(DATABASE).context("isolated count database")?;
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
    let database = directory.path().join("count_c1-custody.sqlite");
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "tenancy::tests::execution::native::counting::custody::count_unknown_owner_child",
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
                    "child exited before count custody acknowledgement"
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
                anyhow::bail!("uncertain count released its file owner");
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
            "count child diagnostics: {}",
            String::from_utf8_lossy(&stderr)
        )
    })?;
    ensure!(
        status?.success(),
        "count custody child failed: {}",
        String::from_utf8_lossy(&stderr)
    );
    drop(stdout);
    drop(crate::tenancy::ownership::acquire_path(&database, false)?);
    Ok(())
}
#[tokio::test]
async fn consuming_count_commit_error_keeps_actual_file_owner() -> Result<()> {
    retained_owner(Fault::Commit).await
}
#[tokio::test]
async fn consuming_count_rollback_error_keeps_actual_file_owner() -> Result<()> {
    retained_owner(Fault::Rollback).await
}
#[tokio::test]
async fn count_rollback_unwind_keeps_actual_file_owner() -> Result<()> {
    retained_owner(Fault::Unwind).await
}
