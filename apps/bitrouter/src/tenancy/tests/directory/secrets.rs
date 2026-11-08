use super::*;
use bitrouter_ai::tenancy::resources::{Definition, ResourcePut};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

#[tokio::test]
async fn directory_does_not_project_resource_ciphertext_or_operation_payloads() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    fixture.service.shutdown().await?;
    let state = crate::tenancy::store::load(&fixture.db, Utc::now()).await?;
    fixture.service = Service::start_resources(
        fixture.db.clone(),
        None,
        state,
        std::sync::Arc::new(crate::tenancy::resources::Resources::with_key(
            fixture.installation.clone(),
            rand::random(),
        )?),
        None,
        None,
    )
    .await?;
    let server = Server::start(&fixture.service).await?;
    server
        .client
        .put_resource(
            &fixture.tenant,
            &ResourcePut {
                format: 1,
                operation_id: "directory-secret-operation".try_into()?,
                installation_id: fixture.installation.clone(),
                key: ResourceKey {
                    owner_kind: OwnerKind::Tenant,
                    owner_id: fixture.tenant.as_str().try_into()?,
                    resource_kind: ResourceKind::Credential,
                    resource_id: "secret-slot".try_into()?,
                },
                expected_revision: None,
                definition: Definition::Credential {
                    secret: "SYNTHETIC_UPSTREAM_SECRET".into(),
                },
            },
        )
        .await?;
    let ciphertext = fixture
        .db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT payload_json FROM tenancy_resource_revisions",
        ))
        .await?
        .context("encrypted resource")?
        .try_get::<String>("", "payload_json")?;
    ensure!(!ciphertext.contains("SYNTHETIC_UPSTREAM_SECRET"));
    let response = reqwest::Client::new()
        .get(format!("{}/control/v1/tenancy/directory", server.endpoint))
        .bearer_auth(OPERATOR)
        .header("Bitrouter-Installation-Id", fixture.installation.as_str())
        .send()
        .await?;
    ensure!(response.status() == 200 && response.headers()["cache-control"] == "no-store");
    let body = response.text().await?;
    for private in [
        ciphertext.as_str(),
        "SYNTHETIC_UPSTREAM_SECRET",
        "directory-secret-operation",
        "secret-slot",
        "payload_json",
        "ciphertext",
    ] {
        ensure!(!body.contains(private));
    }
    let page: DirectoryPageV1 = serde_json::from_str(&body)?;
    ensure!(page.entries.len() == 3);
    fixture.service.shutdown().await?;
    Ok(())
}
