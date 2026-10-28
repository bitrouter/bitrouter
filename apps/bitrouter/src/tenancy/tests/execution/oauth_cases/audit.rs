//! O1's actual optional auxiliary row is read through the unchanged archive decoder.
use super::*;
#[tokio::test]
async fn actual_refreshed_receipt_audits_without_refreshing_or_rewriting_037_bytes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("authority.sqlite");
    let harness = Harness::with_path(true, false, Some(&source)).await?;
    crate::tenancy::audit::tests::assert_file_database(&harness.fixture.db, &source).await?;
    ensure!(crate::tenancy::ownership::acquire_path(&source, false).is_err());
    let persisted =
        crate::tenancy::store::load_existing(&harness.fixture.db, chrono::Utc::now()).await?;
    ensure!(persisted.installation == harness.fixture.installation);
    harness.provider.refresh_gate.add_permits(1);
    harness.provider.model_gate.add_permits(1);
    let response = harness.send(&Harness::client(false)?, false).send().await?;
    ensure!(response.status() == StatusCode::OK);
    let id = execution_id(&response)?;
    response.bytes().await?;
    let receipt = harness.receipt(&id).await?;
    ensure!(receipt.attempts.len() == 1 && receipt.usage.input_tokens == Some(16));
    ensure!(
        harness.provider.refreshes.load(Ordering::SeqCst) == 1
            && harness.provider.models.load(Ordering::SeqCst) == 1
    );
    ensure!(harness.row().await?.metadata.generation == 1);
    harness.fixture.service.shutdown().await?;
    super::super::receipts::audit::roundtrip(
        &harness.fixture.db,
        &source,
        &receipt,
        harness.credential.secret.as_deref().context("old token")?,
    )
    .await?;
    ensure!(
        harness.provider.refreshes.load(Ordering::SeqCst) == 1
            && harness.provider.models.load(Ordering::SeqCst) == 1
    );
    ensure!(harness.row().await?.metadata.generation == 1);
    Ok(())
}
