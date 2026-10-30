//! A real SQLite consuming COMMIT error, isolated to a child process. No test
//! claims a driver's eventual Drop rollback is acknowledged H cleanup.
use super::*;
use crate::tenancy::{Service, resources::Resources, transaction::Disposition};
use std::{io::Write, path::Path, process::Stdio};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

const DATABASE: &str = "BITROUTER_TEST_HTTP034_CUSTODY_DATABASE";
const RETAINED: &str = "http034-commit-owner-retained";

async fn exercise(path: &Path) -> Result<()> {
    let mut server = Server::start().await?;
    let owner = crate::tenancy::ownership::acquire_path(path, true)?;
    let db = crate::db::connect_tenancy(&format!("sqlite://{}", path.display()), &owner).await?;
    let fixture = Fixture::with_database(db.clone(), Default::default(), &mut |_| {}).await?;
    let (mut fixture, route, _) = super::super::provision_protocol_with_fixture(
        fixture,
        &server.url,
        "responses",
        &mut |_| {},
    )
    .await?;
    // Give the real serving worker the concrete lease. Helpers used above have
    // joined their actors; no independent test-owned Lease remains after this.
    fixture.service.shutdown().await?;
    let initial = crate::tenancy::store::load(&db, chrono::Utc::now()).await?;
    fixture.service = Service::start_prepared(
        db.clone(),
        Some(owner),
        Default::default(),
        initial,
        Arc::new(Resources::with_key(fixture.installation.clone(), [29; 32])?),
    )
    .await?;
    let revision = draft(&fixture, &route, "http-commit-fault-draft", 32).await?;
    let issued = fixture.issue("http-commit-fault-user").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let headers = headers(
        &fixture,
        issued.secret.as_deref().context("credential")?,
        &revision.revision_id,
    )?;
    for sql in [
        "PRAGMA foreign_keys=ON",
        "CREATE TABLE http034_parent (id INTEGER PRIMARY KEY)",
        "CREATE TABLE http034_child (parent INTEGER REFERENCES http034_parent(id) DEFERRABLE INITIALLY DEFERRED)",
        "CREATE TRIGGER http034_commit_fault AFTER INSERT ON provider_continuations BEGIN INSERT INTO http034_child(parent) VALUES (77); END",
    ] {
        db.execute_unprepared(sql).await?;
    }
    let ready = Ready {
        fixture,
        headers,
        route,
        revision,
    };
    let (status, _) = send_response(&ready, ready.headers.clone(), body(false, None)).await?;
    ensure!(
        !status.is_success(),
        "failed durable publication reported success"
    );
    server.wire().await?;
    ensure!(
        server.state.calls.load(Ordering::SeqCst) == 1,
        "publication failure retried provider"
    );
    tokio::time::timeout(WAIT, ready.fixture.service.cleanup.quarantined()).await?;
    ensure!(ready.fixture.service.cleanup.disposition() == Disposition::Ambiguous);
    ensure!(ready.fixture.service.cleanup.ensure_clean().is_err());
    let tickets = ready
        .fixture
        .service
        .http_continuation
        .state
        .pending_tickets()
        .map_err(|_| anyhow::anyhow!("pending custody"))?;
    ensure!(
        tickets.len() == 1,
        "ambiguous commit erased its publication generation"
    );
    // SQLite can process Drop rollback now; that is intentionally not a proof
    // of clean custody. No test-owned Service/DB handle survives the signal below.
    ensure!(row_count(&ready).await? == 0);
    let retained_proof = ready.fixture.service.cleanup.clone();
    drop((tickets, ready));
    tokio::time::timeout(WAIT, db.close()).await??;
    ensure!(retained_proof.disposition() == Disposition::Ambiguous);
    drop(retained_proof);
    server.close().await?;
    Ok(())
}

#[tokio::test]
async fn http_commit_failure_child() -> Result<()> {
    let Some(path) = std::env::var_os(DATABASE) else {
        return Ok(());
    };
    tokio::time::timeout(Duration::from_secs(20), exercise(Path::new(&path))).await??;
    // Serial libtest prints its test-name prefix without a newline.
    println!("\n{RETAINED}");
    std::io::stdout().flush()?;
    let mut input = BufReader::new(tokio::io::stdin().take(32));
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(20), input.read_line(&mut line)).await??;
    Ok(())
}

#[tokio::test]
async fn actual_http_commit_failure_keeps_owner_until_process_exit() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("http034-quarantine.sqlite");
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "tenancy::tests::execution::http_continuation::quarantine::http_commit_failure_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(DATABASE, &database)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let output = child.stdout.take().context("child stdout")?;
    let mut output = BufReader::new(output.take(8193));
    tokio::time::timeout(Duration::from_secs(25), async {
        let mut bytes = 0;
        loop {
            let mut line = String::new();
            let read = output.read_line(&mut line).await?;
            ensure!(read != 0, "child exited before HTTP custody signal");
            bytes += read;
            ensure!(bytes <= 8192, "child diagnostics exceeded bound");
            if line.trim() == RETAINED {
                return Ok::<_, anyhow::Error>(());
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
            "owner probe failed for a different reason"
        ),
        Ok(owner) => {
            drop(owner);
            anyhow::bail!("ambiguous HTTP publication released its file owner");
        }
    }
    drop(child.stdin.take());
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait()).await??;
    ensure!(status.success(), "HTTP custody child failed: {status}");
    drop(output);
    drop(crate::tenancy::ownership::acquire_path(&database, false)?);
    Ok(())
}
