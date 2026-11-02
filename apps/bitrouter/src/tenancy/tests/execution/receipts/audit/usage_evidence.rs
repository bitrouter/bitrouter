//! A frozen archive preserves actual provider observations and refuses their loss.
use super::*;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn original_usage_sidecar_survives_archive_http_and_deleted_sidecar_blocks_open() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let live = directory.path().join("authority.sqlite");
    let harness = file_harness(&live).await?;
    let client = Harness::client(false)?;
    let response = harness.send(&client).send().await?;
    ensure!(response.status() == StatusCode::OK);
    let id = execution_id(&response)?;
    response.bytes().await?;
    let receipt = harness.receipt(&client, &id).await?;
    ensure!(receipt.attempts.len() == 1 && harness.calls.load(Ordering::SeqCst) == 1);
    harness.fixture.service.shutdown().await?;
    // This checks exact sidecar bytes through the real protected HTTP route,
    // SDK validation, both supported HTTP versions, and unchanged bundle bytes.
    roundtrip(
        &harness.fixture.db,
        &live,
        &receipt,
        harness.credential.secret.as_deref().context("old token")?,
    )
    .await?;

    let retired = directory.path().join("missing-sidecar.sqlite");
    std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&retired)?;
    std::fs::set_permissions(&retired, std::fs::Permissions::from_mode(0o600))?;
    harness
        .fixture
        .db
        .execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "VACUUM INTO ?",
            [retired.to_str().context("retired path")?.into()],
        ))
        .await?;
    let owner = crate::tenancy::ownership::acquire_path(&retired, false)?;
    let db =
        crate::db::connect_tenancy(&format!("sqlite://{}", owner.path.display()), &owner).await?;
    let deleted = db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "DELETE FROM tenancy_usage_evidence WHERE installation_id=? AND tenant_id=? AND execution_id=?",
        [receipt.installation_id.as_str().into(), receipt.tenant_id.as_str().into(), receipt.execution_id.as_str().into()],
    )).await?;
    ensure!(
        deleted.rows_affected() == 1,
        "actual provider sidecar must exist before fault injection"
    );
    db.close().await?;
    drop(owner);
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if let Ok(owner) = crate::tenancy::ownership::acquire_path(&retired, false) {
                drop(owner);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;

    let original = std::fs::read(&retired)?;
    let bundle = directory.path().join("missing-bundle");
    let report = crate::offline::snapshot(&retired, &bundle).await?;
    let report = serde_json::to_value(&report as &dyn crate::output::CliReport)?;
    let digest = report["evidence_digest"]
        .as_str()
        .context("snapshot anchor")?;
    crate::offline::verify(&bundle, digest).await?;
    let before = files(&bundle)?;
    let stop = CancellationToken::new();
    let _stop_on_drop = stop.clone().drop_guard();
    let opened = crate::offline::audit::open(
        &bundle,
        &directory.path().join("rejected-projection"),
        digest,
        stop,
    )
    .await;
    match opened {
        Err(_) => {}
        Ok(reader) => {
            reader.shutdown().await?;
            anyhow::bail!("archive adopted a receipt whose bound usage evidence was deleted");
        }
    }
    ensure!(
        files(&bundle)? == before && std::fs::read(&retired)? == original,
        "archive verification must never reconstruct or repair evidence"
    );
    ensure!(
        harness.calls.load(Ordering::SeqCst) == 1,
        "archive inspection must never repeat provider work"
    );
    Ok(())
}
