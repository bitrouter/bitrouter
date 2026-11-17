//! Authenticated replacement under the same authority, tenant and revision.

use sha2::{Digest, Sha256};

use super::*;
use crate::tenancy::configuration_store;

async fn replace_source(fixture: &Fixture, revision: &RevisionId) -> Result<()> {
    let original = configuration_store::configuration(&fixture.db, &fixture.tenant, revision)
        .await?
        .context("original configuration missing")?;
    let mut document = original.revision.document.clone();
    document.generation_defaults.max_output_tokens = 37;
    document.generation_defaults.temperature = 0.5;
    // Compile with the same installation, tenant and revision. The real compiler
    // seals the new document and dependency closure with the current AAD.
    let compiled = fixture
        .service
        .resources
        .compile(&fixture.db, &fixture.tenant, revision, &document)
        .await
        .map_err(|code| anyhow::anyhow!("replacement compilation rejected: {code:?}"))?;
    let mut canonical = serde_json::to_value(&document)?;
    crate::tenancy::command::sort(&mut canonical);
    let digest = hex::encode(Sha256::digest(serde_json::to_vec(&canonical)?));
    let update = Query::update()
        .table(Alias::new("tenancy_configurations"))
        .value(
            Alias::new("document_json"),
            serde_json::to_string(&document)?,
        )
        .value(
            Alias::new("compiled_json"),
            serde_json::to_string(&compiled.payload)?,
        )
        .value(Alias::new("compiler_version"), compiled.compiler_version)
        .value(Alias::new("content_digest"), digest)
        .value(
            Alias::new("dependencies_json"),
            serde_json::to_string(&compiled.dependencies)?,
        )
        .and_where(Expr::col(Alias::new("tenant_id")).eq(fixture.tenant.as_str()))
        .and_where(Expr::col(Alias::new("revision_id")).eq(revision.as_str()))
        .to_owned();
    ensure!(
        fixture
            .db
            .execute(fixture.db.get_database_backend().build(&update))
            .await?
            .rows_affected()
            == 1
    );
    let replacement = configuration_store::configuration(&fixture.db, &fixture.tenant, revision)
        .await?
        .context("replacement configuration missing")?;
    ensure!(replacement.revision.tenant_id == original.revision.tenant_id);
    ensure!(replacement.revision.revision_id == original.revision.revision_id);
    ensure!(replacement.authority_revision == original.authority_revision);
    ensure!(replacement.creator == original.creator);
    ensure!(replacement.created_at == original.created_at);
    ensure!(replacement.revision.document == document);
    ensure!(replacement.revision.content_digest != original.revision.content_digest);
    fixture
        .service
        .resources
        .validate_executable(&replacement)
        .map_err(|code| anyhow::anyhow!("authenticated replacement rejected: {code:?}"))?;
    Ok(())
}

#[tokio::test]
async fn authenticated_same_revision_source_replacement_has_equivalent_hot_and_cold_dispatch()
-> Result<()> {
    let mut provider = Provider::start().await?;
    let (mut fixture, route, _) = provision(&provider.url).await?;
    let revision = draft(&fixture, &route, "replace-source", 25).await?;
    let credential = fixture.issue("replacement-user").await?;
    let token = credential.secret.as_deref().context("secret")?;
    enable_dispatch(&fixture);
    let binding = headers(&fixture, token, &revision.revision_id)?;
    let authority = fixture.service.current().map_err(failure)?.revision;
    let instance = fixture.service.instance.clone();
    let original = provider
        .generation_request(&fixture.service, &binding, 25, SECRET)
        .await?;
    ensure!(provider.calls() == 1);

    // Leave the original dispatch cache resident, without changing authority or
    // the request binding. Only the authenticated stored source is replaced.
    replace_source(&fixture, &revision.revision_id).await?;
    ensure!(fixture.service.current().map_err(failure)?.revision == authority);
    ensure!(headers(&fixture, token, &revision.revision_id)? == binding);
    let hot = provider
        .generation_request(&fixture.service, &binding, 37, SECRET)
        .await?;
    ensure!(hot["temperature"] == 0.5);
    ensure!(hot != original, "warm dispatch reused the old source");
    let hot_again = provider
        .generation_request(&fixture.service, &binding, 37, SECRET)
        .await?;
    ensure!(hot_again == hot);
    ensure!(provider.calls() == 3);
    ensure!(fixture.service.current().map_err(failure)?.revision == authority);

    fixture.service.shutdown().await?;
    let snapshot = store::load(&fixture.db, Utc::now()).await?;
    ensure!(snapshot.revision == authority);
    fixture.service = Service::start_prepared(
        fixture.db.clone(),
        None,
        snapshot,
        fixture.service.resources.clone(),
    )
    .await?;
    ensure!(fixture.service.instance != instance);
    enable_dispatch(&fixture);
    // A fresh process requires a fresh instance/capability binding, while the
    // installation, tenant, stored source and execution revision stay the same.
    let cold_binding = headers(&fixture, token, &revision.revision_id)?;
    let cold = provider
        .generation_request(&fixture.service, &cold_binding, 37, SECRET)
        .await?;
    ensure!(cold == hot, "hot and cold dispatch used different source");
    ensure!(provider.calls() == 4);
    ensure!(fixture.service.current().map_err(failure)?.revision == authority);
    fixture.service.shutdown().await?;
    Ok(())
}
