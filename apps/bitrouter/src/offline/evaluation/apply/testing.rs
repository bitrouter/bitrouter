//! Per-invocation scheduling/fault only; the production owner/SQL path is retained.
use super::*;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
#[derive(Default)]
struct Facts {
    before: [i64; 5],
    commit_observed: bool,
    sqlite_code: Option<i32>,
}

pub(super) struct Schedule {
    entered: Option<oneshot::Sender<()>>,
    release: Option<oneshot::Receiver<()>>,
    commit_violation: bool,
    facts: Arc<Mutex<Facts>>,
}
impl Schedule {
    pub(super) async fn entered(&mut self) -> Result<()> {
        if let Some(send) = self.entered.take() {
            send.send(())
                .map_err(|_| anyhow::anyhow!("entry witness closed"))?;
        }
        if let Some(wait) = self.release.take() {
            wait.await?;
        }
        Ok(())
    }
    pub(super) fn observe_commit(
        &self,
        result: &std::result::Result<
            sea_orm::sqlx::sqlite::SqliteQueryResult,
            sea_orm::sqlx::Error,
        >,
    ) -> Result<()> {
        let mut facts = self
            .facts
            .lock()
            .map_err(|_| anyhow::anyhow!("facts mutex"))?;
        facts.commit_observed = true;
        facts.sqlite_code = result
            .as_ref()
            .err()
            .and_then(|e| e.as_database_error())
            .and_then(|e| e.code())
            .and_then(|c| c.parse().ok());
        Ok(())
    }
    pub(super) async fn before_commit(&self, db: &mut SqliteConnection) -> Result<()> {
        if self.commit_violation {
            let mut before = [0_i64; 5];
            for (i, table) in rows::TABLES.into_iter().chain([PROOFS]).enumerate() {
                before[i] =
                    sea_orm::sqlx::query_scalar(&format!("SELECT count(*) FROM {}", table.name))
                        .fetch_one(&mut *db)
                        .await?;
            }
            self.facts
                .lock()
                .map_err(|_| anyhow::anyhow!("facts mutex"))?
                .before = before;
            // A genuine deferred-FK COMMIT error from this exact SQLx worker.
            // TEMP objects do not modify target schema or persisted provenance.
            // https://www.sqlite.org/foreignkeys.html#fk_deferred
            for sql in [
                "CREATE TEMP TABLE eval_fault_parent(id INTEGER PRIMARY KEY)",
                "CREATE TEMP TABLE eval_fault_child(id INTEGER REFERENCES eval_fault_parent(id) DEFERRABLE INITIALLY DEFERRED)",
                "INSERT INTO eval_fault_child VALUES(1)",
            ] {
                sea_orm::sqlx::query(sql).execute(&mut *db).await?;
            }
        }
        Ok(())
    }
}

#[tokio::test]
async fn cancelled_reply_retains_real_writer_until_explicit_rollback_and_close() -> Result<()> {
    use super::super::tests::{Fixture, TIMEOUT};
    tokio::time::timeout(TIMEOUT, async {
        let fixture = Fixture::new().await?;
        let before = std::fs::read(&fixture.target)?;
        let (path, anchor, report) = fixture.write_plan("cancelled")?;
        let (entered, wait) = oneshot::channel();
        let (release, resume) = oneshot::channel();
        let task = tokio::spawn(async move {
            run_inner(
                &path,
                &anchor,
                &report,
                Some(Schedule {
                    entered: Some(entered),
                    release: Some(resume),
                    commit_violation: false,
                    facts: Arc::new(Mutex::new(Facts::default())),
                }),
            )
            .await
        });
        wait.await?;
        ensure!(
            crate::tenancy::ownership::acquire_path(&fixture.target, false).is_err(),
            "active writer not retained"
        );
        task.abort();
        ensure!(task.await.is_err_and(|e| e.is_cancelled()));
        ensure!(
            crate::tenancy::ownership::acquire_path(&fixture.target, false).is_err(),
            "abandoned waiter released active writer"
        );
        release
            .send(())
            .map_err(|_| anyhow::anyhow!("owner lost pending await"))?;
        let owner = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Ok(owner) = crate::tenancy::ownership::acquire_path(&fixture.target, false) {
                    break owner;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        ensure!(
            std::fs::read(&fixture.target)? == before,
            "cancelled empty transaction wrote history"
        );
        let report = fixture.directory.path().join("cancelled");
        ensure!(report.join("INCOMPLETE").exists() && !report.join("RECORDED").exists());
        drop(owner);
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

// The unknown-COMMIT case is isolated because conservative custody deliberately
// keeps this real file lease until process exit. No timeout constitutes success.
#[tokio::test]
#[ignore = "internal child; invoked only by the bounded parent"]
async fn consuming_commit_error_retains_destination_child() -> Result<()> {
    let Some(root) = std::env::var_os("BITROUTER_EVAL_IMPORT_FAULT_ROOT") else {
        return Ok(());
    };
    let root = PathBuf::from(root);
    files::private_directory(&root)?;
    ensure!(
        std::fs::read_dir(&root)?.next().is_none(),
        "fresh child directory required"
    );
    let fixture =
        super::super::tests::Fixture::in_directory(tempfile::TempDir::new_in(&root)?).await?;
    let (path, anchor, report) = fixture.write_plan("unknown")?;
    let facts = Arc::new(Mutex::new(Facts::default()));
    let outcome = run_inner(
        &path,
        &anchor,
        &report,
        Some(Schedule {
            entered: None,
            release: None,
            commit_violation: true,
            facts: facts.clone(),
        }),
    )
    .await;
    // Preserve files before any fallible assertion while the quarantined lease
    // still lives. Only the parent may remove its directory after actual exit.
    let retained = fixture.directory.keep();
    let error = outcome
        .err()
        .ok_or_else(|| anyhow::anyhow!("COMMIT unexpectedly acknowledged"))?;
    let facts = facts.lock().map_err(|_| anyhow::anyhow!("facts mutex"))?;
    ensure!(
        facts.commit_observed && facts.sqlite_code == Some(787) && facts.before == [2, 2, 2, 2, 2],
        "did not reach consuming COMMIT with actual complete graph and SQLITE_CONSTRAINT_FOREIGNKEY"
    );
    ensure!(
        error.to_string().contains("outcome/cleanup unconfirmed"),
        "wrong failure boundary"
    );
    ensure!(
        crate::tenancy::ownership::acquire_path(&fixture.target, false).is_err(),
        "unknown COMMIT released lease"
    );
    ensure!(report.join("INCOMPLETE").exists() && !report.join("RECORDED").exists());
    // Exact full-line acknowledgement is emitted only after all custody checks.
    files::write_new(
        &root.join("retained-name"),
        retained
            .file_name()
            .ok_or_else(|| anyhow::anyhow!("fixture directory name"))?
            .as_encoded_bytes(),
    )?;
    println!("\nEVAL_IMPORT_COMMIT_UNKNOWN_RETAINED");
    Ok(())
}
#[tokio::test]
async fn actual_failed_commit_keeps_owner_until_child_exit_then_preserves_empty_target()
-> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::AsyncReadExt;
    // tempfile directories otherwise inherit 0777 & umask. The child accepts
    // only a private evidence root, independent of the test runner's umask.
    let root = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let selector =
        "offline::evaluation::apply::testing::consuming_commit_error_retains_destination_child";
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            selector,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("BITROUTER_EVAL_IMPORT_FAULT_ROOT", root.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("child stdout"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("child stderr"))?;
    let mut bytes = Vec::new();
    let mut errors = Vec::new();
    let result = tokio::time::timeout(std::time::Duration::from_secs(110), async {
        // Keep both bounded drains and the same real child wait in flight.
        // A fixture error must not disappear into a generic missing-marker error
        // or block on a full stderr pipe. Overflow still fails the parent.
        let (_, _, status) = tokio::try_join!(
            async {
                (&mut stdout).take(65537).read_to_end(&mut bytes).await?;
                ensure!(bytes.len() <= 65536, "child stdout evidence bound");
                Ok::<_, anyhow::Error>(())
            },
            async {
                (&mut stderr).take(65537).read_to_end(&mut errors).await?;
                ensure!(errors.len() <= 65536, "child stderr diagnostic bound");
                Ok::<_, anyhow::Error>(())
            },
            async { child.wait().await.map_err(anyhow::Error::from) }
        )?;
        Ok::<_, anyhow::Error>(status)
    })
    .await;
    let diagnostic = || {
        format!(
            "child stdout ({} bytes):\n{}\nchild stderr ({} bytes):\n{}",
            bytes.len(),
            String::from_utf8_lossy(&bytes),
            errors.len(),
            String::from_utf8_lossy(&errors),
        )
    };
    if !matches!(&result, Ok(Ok(_))) {
        let _ = child.start_kill();
        child.wait().await.map_err(|error| {
            anyhow::anyhow!(
                "failed to reap Eval import child: {error}; {}",
                diagnostic()
            )
        })?;
    }
    let status = result
        .map_err(|error| {
            anyhow::anyhow!(
                "Eval import child watchdog elapsed: {error}; {}",
                diagnostic()
            )
        })?
        .map_err(|error| {
            anyhow::anyhow!(
                "Eval import child output/wait failed: {error:#}; {}",
                diagnostic()
            )
        })?;
    ensure!(
        status.success(),
        "Eval import child process/fixture failed: status={status}; {}",
        diagnostic()
    );
    let output = std::str::from_utf8(&bytes).map_err(|error| {
        anyhow::anyhow!(
            "Eval import child stdout is not UTF-8: {error}; {}",
            diagnostic()
        )
    })?;
    let markers = output
        .lines()
        .filter(|line| *line == "EVAL_IMPORT_COMMIT_UNKNOWN_RETAINED")
        .count();
    ensure!(
        markers == 1,
        "successful Eval import child missing exact outcome: markers={markers}; {}",
        diagnostic()
    );
    let name = String::from_utf8(files::read(&root.path().join("retained-name"), 256)?)?;
    ensure!(
        !name.contains('/') && !name.contains('\\') && name.starts_with(".tmp"),
        "fixture basename"
    );
    let path = root.path().join(name).join("target.sqlite");
    let owner = crate::tenancy::ownership::acquire_path(&path, false)?;
    let mut connections = Connections::new(CancellationToken::new(), Some(owner));
    connections.open(&path, false).await?;
    let checked = async {
        let db = connections
            .source
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("target"))?;
        for table in rows::TABLES.into_iter().chain([PROOFS]) {
            let count: i64 =
                sea_orm::sqlx::query_scalar(&format!("SELECT count(*) FROM {}", table.name))
                    .fetch_one(&mut *db)
                    .await?;
            ensure!(count == 0, "failed deferred COMMIT persisted rows");
        }
        let operations: i64 =
            sea_orm::sqlx::query_scalar("SELECT count(*) FROM tenancy_operations")
                .fetch_one(db)
                .await?;
        ensure!(operations == 8, "fresh provisioning history lost");
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let close = connections.close().await;
    drop(connections);
    checked?;
    close?;
    Ok(())
}
