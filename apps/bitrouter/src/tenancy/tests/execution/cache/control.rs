use super::*;
use bitrouter_ai::tenancy::ordered::{CandidateManifest, GenerationProtocol};

async fn manifest(
    fixture: &Fixture,
    route: &ResourceRef,
    revision: &RevisionId,
) -> Result<CandidateManifest> {
    fixture
        .service
        .candidate_manifest(
            &fixture.operator,
            &fixture.tenant,
            revision,
            route,
            GenerationProtocol::ChatCompletions,
        )
        .await
        .map_err(failure)
}

#[tokio::test]
async fn warm_control_manifest_still_reads_bounded_storage_and_rejects_replaced_ciphertext()
-> Result<()> {
    let (fixture, route, _) = provision("https://example.invalid/v1").await?;
    let revision = draft(&fixture, &route, "control-draft", 25).await?;
    let first = manifest(&fixture, &route, &revision.revision_id).await?;
    let second = manifest(&fixture, &route, &revision.revision_id).await?;
    ensure!(serde_json::to_value(first)? == serde_json::to_value(second)?);
    let reads = occupy_reads(&fixture.service).await?;
    ensure!(
        matches!(fixture.service.candidate_manifest(&fixture.operator,
        &fixture.tenant, &revision.revision_id, &route, GenerationProtocol::ChatCompletions).await,
        Err(error) if error.0.code == ErrorCode::CapacityExceeded)
    );
    drop(reads);
    corrupt_configuration(&fixture, &revision.revision_id).await?;
    ensure!(
        matches!(fixture.service.candidate_manifest(&fixture.operator,
        &fixture.tenant, &revision.revision_id, &route, GenerationProtocol::ChatCompletions).await,
        Err(error) if error.0.code == ErrorCode::AuthorityUnavailable)
    );
    ensure!(fixture.service.current().is_ok());
    fixture.issue("healthy-after-corrupt-manifest").await?;
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn control_cache_capacity_does_not_reject_uncached_management_or_escape_dispatch_pins()
-> Result<()> {
    let (mut fixture, route, _) = provision("https://example.invalid/v1").await?;
    fixture.service.caches = Arc::new(Caches::with_limits(
        1,
        128 * 1024 * 1024,
        256,
        8 * 1024 * 1024,
    )?);
    let revision = draft(&fixture, &route, "control-capacity-draft", 25).await?;
    let credential = fixture.issue("control-capacity-user").await?;
    enable_dispatch(&fixture);
    let binding = headers(
        &fixture,
        credential.secret.as_deref().context("secret")?,
        &revision.revision_id,
    )?;
    let execution = admitted(&fixture, &binding).await?;
    // The only execution entry is pinned by the actual admitted route. Control
    // must serve its original uncached path without evicting or rejecting it.
    let first = manifest(&fixture, &route, &revision.revision_id).await?;
    let second = manifest(&fixture, &route, &revision.revision_id).await?;
    ensure!(serde_json::to_value(first)? == serde_json::to_value(second)?);
    ensure!(fixture.service.current().is_ok());
    drop(execution);
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn warm_control_manifest_does_not_survive_closed_authority() -> Result<()> {
    let (fixture, route, _) = provision("https://example.invalid/v1").await?;
    let revision = draft(&fixture, &route, "control-closed-draft", 25).await?;
    manifest(&fixture, &route, &revision.revision_id).await?;
    fixture.service.shutdown().await?;
    ensure!(
        matches!(fixture.service.candidate_manifest(&fixture.operator,
        &fixture.tenant, &revision.revision_id, &route, GenerationProtocol::ChatCompletions).await,
        Err(error) if error.0.code == ErrorCode::AuthorityUnavailable)
    );
    Ok(())
}

#[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
#[tokio::test]
async fn control_cache_compares_stored_metadata_and_rebuilds_on_new_authority() -> Result<()> {
    let (fixture, route, _) = provision("https://example.invalid/v1").await?;
    let revision = draft(&fixture, &route, "control-metadata-draft", 25).await?;
    let entries = || -> Result<u64> {
        fixture
            .service
            .caches
            .observed()
            .into_iter()
            .next()
            .context("execution usage")?
            .map(|usage| usage.entries)
            .map_err(|_| anyhow::anyhow!("usage unavailable"))
    };
    manifest(&fixture, &route, &revision.revision_id).await?;
    ensure!(entries()? == 1);
    manifest(&fixture, &route, &revision.revision_id).await?;
    ensure!(entries()? == 1);
    let query = Query::update()
        .table(Alias::new("tenancy_configurations"))
        .value(Alias::new("creator_principal_id"), "other-creator")
        .and_where(Expr::col(Alias::new("tenant_id")).eq(fixture.tenant.as_str()))
        .and_where(Expr::col(Alias::new("revision_id")).eq(revision.revision_id.as_str()))
        .to_owned();
    fixture
        .db
        .execute(fixture.db.get_database_backend().build(&query))
        .await?;
    manifest(&fixture, &route, &revision.revision_id).await?;
    ensure!(
        entries()? == 2,
        "same-ID row replacement reused the old binding"
    );
    // An unrelated real authority publication still creates a new immutable
    // namespace, without treating the previous cache entry as permission.
    fixture.issue("control-authority-advance").await?;
    manifest(&fixture, &route, &revision.revision_id).await?;
    ensure!(entries()? == 3);
    fixture.service.shutdown().await?;
    Ok(())
}
