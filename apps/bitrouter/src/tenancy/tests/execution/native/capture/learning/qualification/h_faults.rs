//! Actual paid Prepare/Accept transactions, with the existing H fault seams.
use super::*;
const CHILD: &str =
    "tenancy::tests::execution::native::capture::learning::qualification::h_faults::owner_child";
async fn exercise(path: &Path, mode: &str) -> Result<()> {
    let provider = Provider::start(false).await?;
    let judge = Judge::new(false).await?;
    let ready = file_ready(path, &provider.url, &judge.url).await?;
    let (binding, port) = ready.port_pair().await?;
    let summary = frozen(&binding).await?;
    let proof = ready.fixture.service.cleanup.clone();
    let reads = ready.fixture.service.reads.available_permits();
    if mode == "rollback" {
        // DML ends SQLite's transaction; the following consuming ORM rollback
        // genuinely fails. Read-only Prepare rollbacks are unaffected.
        ready.fixture.db.execute_unprepared("CREATE TRIGGER paid_gap_rollback BEFORE INSERT ON checkpoint_evolution WHEN NEW.kind='owned_judge_job' BEGIN SELECT RAISE(ROLLBACK, 'paid acceptance rollback fixture'); END").await?;
    }
    let gate = match mode {
        "begin" => Some(proof.hold(Phase::Beginning)),
        "unwind" => Some(proof.hold(Phase::Active)),
        "rollback" => None,
        _ => anyhow::bail!("invalid paid fault mode"),
    };
    let task = AbortOnDropHandle::new(tokio::spawn(async move {
        crate::tenancy::learning::checkpoint(binding, native(), &summary).await
    }));
    if let Some(gate) = &gate {
        tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        ensure!(proof.disposition() == Disposition::Pending && !task.is_finished());
        if mode == "begin" {
            // One physical tenancy connection. SQLx has not incremented its
            // depth; the actual SeaORM BEGIN now receives SQLite's nested-BEGIN error.
            ready.fixture.db.execute_unprepared("BEGIN").await?;
            proof.release_gate();
        } else {
            proof.unwind_gate()?;
        }
    }
    ensure!(tokio::time::timeout(TIMEOUT, task).await??.is_err());
    tokio::time::timeout(TIMEOUT, proof.quarantined()).await?;
    ensure!(proof.disposition() == Disposition::Ambiguous && proof.ensure_clean().is_err());
    ensure!(ready.fixture.service.current().is_err());
    ensure!(ready.fixture.service.reads.available_permits() == reads - 1);
    ensure!(judge.calls.load(Ordering::SeqCst) == 0 && provider.calls() == 0);
    // Attempt every normal cleanup. A timeout/error is not clean retirement;
    // the parent probes the file only after this function drops all handles.
    let capture = tokio::time::timeout(Duration::from_secs(3), port.finish()).await;
    let shutdown =
        tokio::time::timeout(Duration::from_secs(3), ready.fixture.service.shutdown()).await;
    let closed = tokio::time::timeout(TIMEOUT, ready.fixture.db.clone().close()).await;
    let judge_stop = judge.stop().await;
    let provider_stop = provider.finish().await;
    ensure!(
        !matches!(shutdown, Ok(Ok(()))),
        "unknown actor reported clean"
    );
    let _capture_unconfirmed = capture;
    closed??;
    judge_stop?;
    provider_stop?;
    ensure!(proof.disposition() == Disposition::Ambiguous);
    Ok(())
}
#[tokio::test]
#[ignore = "only bounded Rust parents invoke this actual-file H fault child"]
async fn owner_child() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(process::ROOT).context("owned root")?);
    let mode = std::env::var(process::MODE)?;
    tokio::time::timeout(
        Duration::from_secs(30),
        exercise(&root.join("owned.db"), &mode),
    )
    .await??;
    process::witness_and_wait().await
}
async fn retained(mode: &str) -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("owned.db");
    let mut child = process::Child::start(CHILD, root.path(), mode, None)?;
    let checked = async {
        child.ready().await?;
        file_held(&path)
    }
    .await;
    child.finish(false, checked).await?;
    drop(crate::tenancy::ownership::acquire_path(&path, false)?);
    Ok(())
}
#[tokio::test]
async fn paid_prepare_real_begin_error_keeps_file_custody() -> Result<()> {
    retained("begin").await
}
#[tokio::test]
async fn paid_accept_real_consuming_rollback_error_keeps_file_custody() -> Result<()> {
    retained("rollback").await
}
#[tokio::test]
async fn paid_prepare_caught_unwind_keeps_file_custody() -> Result<()> {
    retained("unwind").await
}
