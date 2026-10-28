//! Real facade/H failures in a bounded child. A failed cleanup never means clean.
use super::*;
use std::{io::Write, process::Stdio};
use tokio::io::{AsyncBufReadExt, BufReader};

const CHILD: &str = "tenancy::tests::execution::native::capture::learning::human::required::faults::human_owner_child";
const ROOT: &str = "BITROUTER_HUMAN_H_ROOT";
const MODE: &str = "BITROUTER_HUMAN_H_MODE";
const WITNESS: &str = "human-real-actor-owner-retained";

async fn exercise(root: &Path, mode: &str) -> Result<()> {
    let directory = tempfile::Builder::new().prefix("human-").tempdir_in(root)?;
    let mut host = Host::in_directory(false, usize::MAX, directory).await?;
    let path = host.directory.path().join("owned-human.sqlite");
    let checked = async {
        let native = host.open().await?;
        let current = host.current()?;
        current.prompt(&native, "real human H fault").await?;
        current.checkpoint(&native).await?;
        drained(&host.ready).await?;
        let input = submission(&current.review_checkpoint(&native, None).await?, "human-h-owner");
        let db = &host.ready.fixture.db;
        match mode {
            "rollback" => {
                db.execute_unprepared("CREATE TRIGGER human_h_rollback BEFORE INSERT ON acp_assessment_revisions BEGIN SELECT RAISE(ROLLBACK, 'human rollback fixture'); END").await?;
            }
            "commit" => {
                for sql in [
                    "PRAGMA foreign_keys=ON",
                    "CREATE TABLE human_h_parent(id INTEGER PRIMARY KEY)",
                    "CREATE TABLE human_h_child(parent INTEGER REFERENCES human_h_parent(id) DEFERRABLE INITIALLY DEFERRED)",
                    "CREATE TRIGGER human_h_commit AFTER INSERT ON acp_assessment_revisions BEGIN INSERT INTO human_h_child(parent) VALUES(19); END",
                ] { db.execute_unprepared(sql).await?; }
                let row = db.query_one(Statement::from_string(DatabaseBackend::Sqlite, "PRAGMA foreign_keys"))
                    .await?.context("foreign keys projection")?;
                ensure!(row.try_get::<i64>("", "foreign_keys")? == 1);
            }
            "begin" | "unwind" => {}
            _ => anyhow::bail!("unknown human H case"),
        }
        let proof = &host.ready.fixture.service.cleanup;
        let reads = host.ready.fixture.service.reads.available_permits();
        let gate = match mode {
            "begin" => Some(proof.hold(Phase::Beginning)),
            "unwind" => Some(proof.hold(Phase::Active)),
            "commit" => Some(proof.hold(Phase::Committing)),
            _ => None,
        };
        let mut work = Box::pin(current.submit_assessment(&native, input));
        if let Some(gate) = gate {
            tokio::select! {
                reply = &mut work => anyhow::bail!("public human work missed H gate: {reply:?}"),
                entered = tokio::time::timeout(TIMEOUT, gate.entered.notified()) => { entered?; }
            }
            ensure!(proof.disposition() == Disposition::Pending);
            if mode == "begin" {
                // The actual single SQLx connection receives a nested-BEGIN
                // failure; no test seam fabricates a successful rollback.
                db.execute_unprepared("BEGIN").await?;
                proof.release_gate();
            } else if mode == "unwind" {
                proof.unwind_gate()?;
            } else {
                proof.release_gate();
            }
        }
        ensure!(tokio::time::timeout(TIMEOUT, &mut work).await?.is_err());
        drop(work);
        tokio::time::timeout(TIMEOUT, proof.quarantined()).await?;
        ensure!(proof.disposition() == Disposition::Ambiguous && proof.ensure_clean().is_err()
            && host.ready.fixture.service.current().is_err()
            && host.ready.fixture.service.reads.available_permits() + 1 == reads);
        ensure!(host.judge.calls.load(Ordering::SeqCst) == 0);
        Ok(())
    }.await;

    // Attempt ALL existing owner cleanup paths even when the checked operation
    // failed. In particular shutdown may encounter the already-consumed Err
    // receiver: it must retain uncertainty, never poll that oneshot twice.
    host.ready.fixture.service.cleanup.release_gate();
    let controller = match host.session.take() {
        Some(session) => tokio::time::timeout(Duration::from_secs(3), session.shutdown()).await,
        None => Ok(Ok(())),
    };
    let listener = host.listener.finish().await;
    let service = tokio::time::timeout(
        Duration::from_secs(3),
        host.ready.fixture.service.shutdown(),
    )
    .await;
    let child = reaped(&host.marker).await;
    let close = tokio::time::timeout(
        Duration::from_secs(3),
        host.ready.fixture.db.clone().close(),
    )
    .await;
    let judge = host.judge.stop().await;
    let provider = host.provider.finish().await;
    let directory = host.directory.keep();
    let original_proof = host.ready.fixture.service.cleanup.clone();
    drop((host.app, host.ready));
    checked?;
    ensure!(
        !matches!(service, Ok(Ok(()))),
        "ambiguous human H retired its Service"
    );
    ensure!(
        !matches!(controller, Ok(Ok(()))),
        "ambiguous Human Port reported clean controller shutdown"
    );
    // Pool-close timeout/error is explicitly not retirement evidence. H and the
    // actual checked host retain it; the parent probes only after these locals
    // and the original proof below have been dropped.
    let _unconfirmed_pool_close = close;
    listener?;
    child?;
    judge?;
    provider?;
    ensure!(original_proof.disposition() == Disposition::Ambiguous && path.is_file());
    drop(original_proof);
    ensure!(directory.parent() == Some(root));
    std::fs::write(
        root.join("database-path"),
        path.as_os_str().as_encoded_bytes(),
    )?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "only exact bounded parent launches the actual Human H child"]
async fn human_owner_child() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(ROOT).context("human H root")?);
    let mode = std::env::var(MODE)?;
    tokio::time::timeout(Duration::from_secs(45), exercise(&root, &mode)).await??;
    println!("\n{WITNESS}");
    std::io::stdout().flush()?;
    let mut input = BufReader::new(tokio::io::stdin().take(32));
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(20), input.read_line(&mut line)).await??;
    ensure!(line == "done\n", "missing exact parent acknowledgment");
    Ok(())
}

async fn retained(mode: &str) -> Result<()> {
    executable()?;
    let root = tempfile::tempdir()?;
    let root_path = root.path().canonicalize()?;
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            CHILD,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ROOT, &root_path)
        .env(MODE, mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stderr = child.stderr.take().context("child stderr")?;
    let errors = AbortOnDropHandle::new(tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.take(8193).read_to_end(&mut bytes).await?;
        Ok::<_, anyhow::Error>(bytes)
    }));
    let mut output = BufReader::new(child.stdout.take().context("child stdout")?.take(8193));
    let mut total = 0usize;
    let mut database = None;
    let checked = async {
        tokio::time::timeout(Duration::from_secs(50), async {
            loop {
                let mut line = String::new();
                let read = output.read_line(&mut line).await?;
                total = total.checked_add(read).context("stdout count overflow")?;
                ensure!(
                    read != 0 && total <= 8192,
                    "missing bounded Human H witness"
                );
                if line.trim_end_matches(['\r', '\n']) == WITNESS {
                    break;
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        let transfer = std::fs::read(root_path.join("database-path"))?;
        ensure!(transfer.len() < 4096);
        let path = PathBuf::from(String::from_utf8(transfer)?);
        ensure!(
            path.file_name() == Some(std::ffi::OsStr::new("owned-human.sqlite"))
                && path.parent().and_then(Path::parent) == Some(root_path.as_path())
                && path.canonicalize()? == path
                && std::fs::metadata(&path)?.len() > 0
        );
        match crate::tenancy::ownership::acquire_path(&path, false) {
            Err(error) => ensure!(matches!(
                error.downcast_ref::<std::fs::TryLockError>(),
                Some(std::fs::TryLockError::WouldBlock)
            )),
            Ok(owner) => {
                drop(owner);
                anyhow::bail!("unknown Human owner released its actual file");
            }
        }
        database = Some(path);
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
    drop(child.stdin.take());
    let mut tail = Vec::new();
    let waited = tokio::time::timeout(Duration::from_secs(25), async {
        tokio::join!(child.wait(), output.read_to_end(&mut tail))
    })
    .await;
    if waited.is_err() {
        let _kill = child.start_kill();
        let _reap = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    }
    let errors = tokio::time::timeout(TIMEOUT, errors).await???;
    checked.with_context(|| format!("{mode}: {}", String::from_utf8_lossy(&errors)))?;
    let (status, drained) = waited?;
    drained?;
    ensure!(
        errors.is_empty(),
        "unexpected Human fault panic/diagnostic: {}",
        String::from_utf8_lossy(&errors)
    );
    ensure!(
        total + tail.len() <= 8192 && errors.len() <= 8192 && status?.success(),
        "Human H child failed or exceeded diagnostics bound"
    );
    let path = database.context("actual checked database absent")?;
    drop(crate::tenancy::ownership::acquire_path(&path, false)?);
    Ok(())
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn public_human_real_begin_failure_retains_owner() -> Result<()> {
    retained("begin").await
}
#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn public_human_real_consuming_rollback_failure_retains_owner() -> Result<()> {
    retained("rollback").await
}
#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn public_human_real_consuming_commit_failure_retains_owner() -> Result<()> {
    retained("commit").await
}
#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn public_human_caught_unwind_retains_owner() -> Result<()> {
    retained("unwind").await
}
