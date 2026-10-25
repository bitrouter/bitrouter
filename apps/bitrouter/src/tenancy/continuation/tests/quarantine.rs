//! Real consuming COMMIT failure; only child-process exit may release quarantine.
use super::*;
use std::{io::Write, process::Stdio};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

const DATABASE: &str = "BITROUTER_TEST_NATIVE_EPOCH_DATABASE";
const READY: &str = "native-epoch-owner-retained";

#[tokio::test]
async fn epoch_commit_error_child() -> Result<()> {
    let Some(path) = std::env::var_os(DATABASE) else {
        return Ok(());
    };
    let path = std::path::PathBuf::from(path);
    let owner = crate::tenancy::ownership::acquire_path(&path, true)?;
    let db = crate::db::connect_tenancy(&format!("sqlite://{}", path.display()), &owner).await?;
    crate::db::run_migrations(&db).await?;
    let initial = store::load(&db, chrono::Utc::now()).await?;
    for sql in [
        "PRAGMA foreign_keys=ON",
        "CREATE TABLE native_epoch_parent (id INTEGER PRIMARY KEY)",
        "CREATE TABLE native_epoch_child (parent INTEGER REFERENCES native_epoch_parent(id) DEFERRABLE INITIALLY DEFERRED)",
        "CREATE TRIGGER native_epoch_commit_fault AFTER INSERT ON tenancy_continuation_epoch BEGIN INSERT INTO native_epoch_child(parent) VALUES (77); END",
    ] {
        db.execute_unprepared(sql).await?;
    }
    let resources = Arc::new(Resources::with_key(initial.installation.clone(), [63; 32])?);
    ensure!(
        tokio::time::timeout(
            Duration::from_secs(5),
            Service::start_prepared(
                db.clone(),
                Some(owner),
                OperationLimits::default(),
                initial,
                resources,
            )
        )
        .await?
        .is_err()
    );
    // Close normal pool handles after the driver has processed its Drop rollback.
    // A successful close is not permission to release the quarantined actor.
    tokio::time::timeout(Duration::from_secs(5), db.close()).await??;
    println!("{READY}");
    std::io::stdout().flush()?;
    let mut input = BufReader::new(tokio::io::stdin().take(32));
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(20), input.read_line(&mut line)).await??;
    Ok(())
}

#[tokio::test]
async fn epoch_commit_error_retains_file_owner_until_process_exit() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("epoch-quarantine.sqlite");
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "tenancy::continuation::tests::quarantine::epoch_commit_error_child",
            "--nocapture",
        ])
        .env(DATABASE, &database)
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
            ensure!(read != 0, "child exited before owner signal");
            bytes += read;
            ensure!(bytes <= 8192, "child diagnostics exceeded bound");
            if line.trim() == READY {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;
    ensure!(crate::tenancy::ownership::acquire_path(&database, false).is_err());
    drop(child.stdin.take());
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait()).await??;
    ensure!(status.success(), "native epoch custody child failed");
    drop(output);
    drop(crate::tenancy::ownership::acquire_path(&database, false)?);
    Ok(())
}
