//! Exercise bootstrap unwind through Service startup, including file-owner custody.

use super::*;
use crate::tenancy::BootstrapGate;
use std::{io::Write, path::Path, process::Stdio};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

const DATABASE: &str = "BITROUTER_TEST_BOOTSTRAP_UNWIND_DATABASE";
const READY: &str = "bootstrap-unwind-owner-retained";

async fn unwind_bootstrap(path: &Path) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let owner = crate::tenancy::ownership::acquire_path(path, true)?;
        let db =
            crate::db::connect_tenancy(&format!("sqlite://{}", path.display()), &owner).await?;
        crate::db::run_migrations(&db).await?;
        let initial = store::load(&db, chrono::Utc::now()).await?;
        let resources = Arc::new(Resources::with_key(initial.installation.clone(), [64; 32])?);
        let import = serde_json::to_vec(&serde_json::json!({
            "format": 1,
            "installation_id": initial.installation,
            "resources": [{
                "reference": {
                    "owner_kind": "tenant", "owner_id": "bootstrap-tenant",
                    "resource_kind": "credential", "resource_id": "bootstrap-slot",
                    "revision_id": "r1"
                },
                "definition": {
                    "kind": "credential",
                    "credential": {"type": "api_key", "data": {"value": "fixture-only"}}
                }
            }]
        }))?;
        let (attached, gate_ready) = tokio::sync::oneshot::channel();
        let startup = tokio::spawn(Service::start_resources(
            db.clone(),
            Some(owner),
            OperationLimits::default(),
            initial,
            resources,
            Some(import),
            Some(BootstrapGate {
                phase: Phase::Committing,
                attached,
            }),
        ));
        let (proof, gate) = gate_ready.await?;
        gate.entered.notified().await;
        ensure!(proof.disposition() == Disposition::Pending);
        ensure!(
            !startup.is_finished(),
            "startup published before bootstrap commit"
        );

        // This is Control::commit inside initialize, after real epoch and import
        // DML. H's existing gate unwinds the actual caught bootstrap work future.
        proof.unwind_gate()?;
        let outcome = startup.await?;
        ensure!(
            outcome.is_err(),
            "unwound bootstrap returned an available Service"
        );
        proof.quarantined().await;
        ensure!(proof.disposition() == Disposition::Ambiguous);
        ensure!(proof.ensure_clean().is_err());

        // These queries let the SQLite driver finish its queued Drop rollback.
        // Rolled-back rows and a closed pool do not acknowledge H cleanup.
        let row = db
            .query_one(sea_orm::Statement::from_string(
                db.get_database_backend(),
                "SELECT (SELECT COUNT(*) FROM tenancy_continuation_epoch) AS epochs, \
                 (SELECT COUNT(*) FROM tenancy_resource_revisions) AS resources"
                    .to_owned(),
            ))
            .await?
            .context("bootstrap rollback counts missing")?;
        ensure!(row.try_get::<i64>("", "epochs")? == 0);
        ensure!(row.try_get::<i64>("", "resources")? == 0);
        db.close().await?;
        ensure!(proof.disposition() == Disposition::Ambiguous);
        ensure!(proof.ensure_clean().is_err());
        drop((proof, gate));
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("bootstrap unwind did not settle within fifteen seconds")?
}

#[tokio::test]
async fn bootstrap_unwind_child() -> Result<()> {
    let Some(path) = std::env::var_os(DATABASE) else {
        return Ok(());
    };
    unwind_bootstrap(Path::new(&path)).await?;
    // The helper has dropped every normal Service/startup/DB/proof handle. Only
    // the actual quarantined worker remains; no test-owned Lease guards the file.
    println!("{READY}");
    std::io::stdout().flush()?;
    let mut input = BufReader::new(tokio::io::stdin().take(32));
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(20), input.read_line(&mut line)).await??;
    Ok(())
}

#[tokio::test]
async fn bootstrap_unwind_keeps_file_owner_after_startup_fails() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("bootstrap-unwind.sqlite");
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "tenancy::continuation::tests::unwind::bootstrap_unwind_child",
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
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut bytes = 0;
        loop {
            let mut line = String::new();
            let read = output.read_line(&mut line).await?;
            ensure!(read != 0, "child exited before bootstrap quarantine signal");
            bytes += read;
            ensure!(bytes <= 8192, "child diagnostics exceeded bound");
            if line.trim() == READY {
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
            "file-owner probe failed for a reason other than retained ownership"
        ),
        Ok(owner) => {
            drop(owner);
            anyhow::bail!("unwound bootstrap released its quarantined file owner");
        }
    }
    // EOF exits the whole child; no API acknowledges or releases quarantine.
    drop(child.stdin.take());
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait()).await??;
    ensure!(status.success(), "bootstrap unwind child failed: {status}");
    drop(output);
    drop(crate::tenancy::ownership::acquire_path(&database, false)?);
    Ok(())
}
