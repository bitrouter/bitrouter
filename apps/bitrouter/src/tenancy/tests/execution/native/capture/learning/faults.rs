//! Paid-acceptance H faults use actual SQL and an isolated runtime/file lease.
use super::*;
use crate::tenancy::{
    capture::queue::{Reply, Request},
    transaction::Phase,
};
use std::{io::Write, process::Stdio};
use tokio::io::{AsyncBufReadExt, BufReader};

const CHILD: &str =
    "tenancy::tests::execution::native::capture::learning::faults::paid_acceptance_child";
const ROOT: &str = "BITROUTER_PAID_ACCEPTANCE_ROOT";
const MODE: &str = "BITROUTER_PAID_ACCEPTANCE_MODE";
const WITNESS: &str = "paid-acceptance-owner-retained";

async fn exercise(root: &Path, mode: &str) -> Result<()> {
    let database = root.join("owned.db");
    let url = format!("sqlite://{}", database.display());
    let owner = crate::tenancy::ownership::acquire(&url)?;
    let db = crate::db::connect_tenancy(&url, &owner).await?;
    let fixture = Fixture::with_database(db, OperationLimits::default(), &mut |_| {}).await?;
    let provider = Provider::start(false).await?;
    let judge = Judge::new(false).await?;
    let ready = Ready::from_fixture(
        fixture,
        Some(owner),
        &provider.url,
        &judge.url,
        true,
        LearningMode::Observe,
        1,
    )
    .await?;
    ensure!(std::fs::metadata(&database)?.len() > 0);
    let (binding, port) = ready.port_pair().await?;
    // Freeze with the existing actor API first, so the gate below is precisely
    // paid acceptance COMMIT, not capture/checkpoint creation or read rollback.
    let result = binding
        .enqueue(Request::Freeze(native().into()))
        .await
        .map_err(failure)?
        .await?;
    let summary = match result.result.map_err(failure)? {
        Reply::Frozen(Some(summary)) => summary,
        _ => anyhow::bail!("actual stopped checkpoint missing"),
    };
    if mode == "commit-error" {
        for sql in [
            "PRAGMA foreign_keys=ON",
            "CREATE TABLE paid_fault_parent(id INTEGER PRIMARY KEY)",
            "CREATE TABLE paid_fault_child(parent INTEGER REFERENCES paid_fault_parent(id) DEFERRABLE INITIALLY DEFERRED)",
            "CREATE TRIGGER paid_fault_commit AFTER INSERT ON checkpoint_evolution WHEN NEW.kind='owned_judge_job' BEGIN INSERT INTO paid_fault_child(parent) VALUES(19); END",
        ] {
            ready.fixture.db.execute_unprepared(sql).await?;
        }
        let row = ready
            .fixture
            .db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "PRAGMA foreign_keys",
            ))
            .await?
            .context("foreign keys")?;
        ensure!(row.try_get::<i64>("", "foreign_keys")? == 1);
    }
    let gate = ready.fixture.service.cleanup.hold(Phase::Committing);
    let task = AbortOnDropHandle::new(tokio::spawn(async move {
        crate::tenancy::learning::checkpoint(binding, native(), &summary).await
    }));
    tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
    ensure!(
        judge.calls.load(Ordering::SeqCst) == 0 && provider.calls() == 0 && !task.is_finished()
    );
    if mode == "late-commit" {
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(61)).await;
        tokio::time::resume();
        tokio::time::timeout(TIMEOUT, ready.fixture.service.cleanup.wait_fenced()).await?;
    }
    ready.fixture.service.cleanup.release_gate();
    ensure!(tokio::time::timeout(TIMEOUT, task).await??.is_err());
    ensure!(ready.fixture.service.current().is_err());
    let jobs = bodies(&ready.fixture, "owned_judge_job").await?;
    if mode == "commit-error" {
        ensure!(jobs.is_empty());
    } else {
        ensure!(jobs.len() == 1 && jobs[0]["status"] == "accepted");
    }
    ensure!(judge.calls.load(Ordering::SeqCst) == 0);
    // Try every cleanup action; neither an error nor an elapsed timeout proves
    // retirement. The parent verifies the file lease after all these handles drop.
    let _capture = port.finish().await;
    let shutdown =
        tokio::time::timeout(Duration::from_secs(2), ready.fixture.service.shutdown()).await;
    ensure!(
        !matches!(shutdown, Ok(Ok(()))),
        "uncertain paid acceptance was reported clean"
    );
    tokio::time::timeout(TIMEOUT, ready.fixture.db.clone().close()).await??;
    judge.stop().await?;
    provider.finish().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "only the bounded parent invokes this isolated paid-acceptance runtime"]
async fn paid_acceptance_child() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(ROOT).context("fault root")?);
    let mode = std::env::var(MODE)?;
    ensure!(matches!(mode.as_str(), "commit-error" | "late-commit"));
    exercise(&root, &mode).await?;
    println!("\n{WITNESS}");
    std::io::stdout().flush()?;
    let mut input = BufReader::new(tokio::io::stdin().take(32));
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(20), input.read_line(&mut line)).await??;
    Ok(())
}

async fn isolated(mode: &str) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            CHILD,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ROOT, directory.path())
        .env(MODE, mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let errors = child.stderr.take().map(|stream| {
        AbortOnDropHandle::new(tokio::spawn(async move {
            let mut bytes = Vec::new();
            stream.take(8193).read_to_end(&mut bytes).await?;
            Ok::<_, anyhow::Error>(bytes)
        }))
    });
    let stdout = child.stdout.take().context("child stdout")?;
    let mut output = BufReader::new(stdout.take(8193));
    let mut total = 0;
    let checked: Result<()> = async {
        tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                let mut line = String::new();
                let read = output.read_line(&mut line).await?;
                total += read;
                ensure!(
                    read > 0 && total <= 8192,
                    "child witness missing or diagnostic bound exceeded"
                );
                if line.trim() == WITNESS {
                    break;
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        ensure!(
            crate::tenancy::ownership::acquire_path(&directory.path().join("owned.db"), false)
                .is_err(),
            "unknown actor released actual file custody"
        );
        child
            .stdin
            .as_mut()
            .context("child stdin")?
            .write_all(b"done\n")
            .await?;
        Ok(())
    }
    .await;
    if checked.is_err() {
        let _kill = child.start_kill();
    }
    // Keep the same bounded pipe reader through ACK and the actual child reap.
    // Drain its terminal libtest output concurrently so neither EOF nor a full
    // pipe can be mistaken for confirmed retirement.
    let mut tail = Vec::new();
    let status = tokio::time::timeout(Duration::from_secs(25), async {
        tokio::join!(child.wait(), output.read_to_end(&mut tail))
    })
    .await;
    if status.is_err() {
        let _kill = child.start_kill();
        let _reaped = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    }
    let diagnostics = match errors {
        Some(task) => tokio::time::timeout(TIMEOUT, task).await???,
        None => Vec::new(),
    };
    checked.with_context(|| format!("{mode}: {}", String::from_utf8_lossy(&diagnostics)))?;
    let (exit, drained) = status?;
    drained?;
    ensure!(total + tail.len() <= 8192 && diagnostics.len() <= 8192);
    ensure!(exit?.success());
    drop(output);
    let _released =
        crate::tenancy::ownership::acquire_path(&directory.path().join("owned.db"), false)?;
    Ok(())
}
#[tokio::test]
async fn actual_consuming_acceptance_commit_error_retains_custody_without_a_judge_send()
-> Result<()> {
    isolated("commit-error").await
}
#[tokio::test]
async fn actual_late_acceptance_commit_is_not_refunded_or_replayed() -> Result<()> {
    isolated("late-commit").await
}
