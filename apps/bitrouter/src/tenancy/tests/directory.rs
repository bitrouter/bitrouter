use super::*;
use bitrouter_ai::{
    control::ControlScope,
    tenancy::{
        authority::{AuditReaderCreate, CredentialState, OwnerKind, ResourceKey, ResourceKind},
        directory::{DirectoryEntryV1, DirectoryPageV1, MAX_DIRECTORY_PAGE_BYTES},
        validation::Validate,
    },
};
use bitrouter_sdk::tenancy::TenancyClient;

mod bounds;
mod secrets;

const OPERATOR: &str = "directory-operator-synthetic-token-32-bytes";
const READER: &str = "directory-host-reader-synthetic-token-32-bytes";

struct Server {
    endpoint: String,
    client: TenancyClient,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn router(service: &Service, operator_scope: ControlScope) -> Result<axum::Router> {
    let auth = crate::remote_control::auth::ControlAuth::from_lookup(
        &crate::config::ControlConfig {
            credentials: vec![
                crate::config::ControlCredentialConfig {
                    id: "test-operator".into(),
                    token_env: "DIRECTORY_OPERATOR".into(),
                    scopes: vec![operator_scope],
                },
                crate::config::ControlCredentialConfig {
                    id: "reader".into(),
                    token_env: "DIRECTORY_READER".into(),
                    scopes: vec![ControlScope::Read],
                },
            ],
            ..Default::default()
        },
        |name| {
            Some(
                if name == "DIRECTORY_OPERATOR" {
                    OPERATOR
                } else {
                    READER
                }
                .into(),
            )
        },
    )?;
    Ok(crate::tenancy::http::router(service.clone(), auth))
}
impl Server {
    async fn start(service: &Service) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let router = router(service, ControlScope::Tenancy)?;
        let task = tokio::spawn(async move { axum::serve(listener, router).await });
        let client = TenancyClient::new(&endpoint, OPERATOR)?;
        Ok(Self {
            endpoint,
            client,
            task,
        })
    }
    async fn page(&self, installation: &InstallationId) -> Result<DirectoryPageV1> {
        Ok(self.client.directory(installation, None, None, 100).await?)
    }
}

#[tokio::test]
async fn directory_http_requires_fresh_operator_authority_and_valid_pagination() -> Result<()> {
    let fixture = Fixture::new().await?;
    let issued = fixture.issue("directory-auth-credential").await?;
    let token = issued.secret.as_deref().context("issuance secret")?;
    let server = Server::start(&fixture.service).await?;
    let client = reqwest::Client::new();
    for (credential, query, installation, expected) in [
        ("wrong-key", "", fixture.installation.as_str(), 401),
        (token, "", fixture.installation.as_str(), 403),
        (READER, "", fixture.installation.as_str(), 403),
        (OPERATOR, "", "wrong-installation", 409),
        (OPERATOR, "limit=0", fixture.installation.as_str(), 400),
        (OPERATOR, "limit=101", fixture.installation.as_str(), 400),
        (
            OPERATOR,
            "limit=1&limit=2",
            fixture.installation.as_str(),
            400,
        ),
        (
            OPERATOR,
            "authority_revision=9007199254740992",
            fixture.installation.as_str(),
            400,
        ),
        (
            OPERATOR,
            "authority_revision=1&after=secret-invalid-cursor",
            fixture.installation.as_str(),
            400,
        ),
        (
            OPERATOR,
            "after=v1%7Ctenant%7C%7Ctenant-a",
            fixture.installation.as_str(),
            400,
        ),
        (
            OPERATOR,
            "unexpected=secret-query",
            fixture.installation.as_str(),
            400,
        ),
    ] {
        let response = client
            .get(format!(
                "{}/control/v1/tenancy/directory?{query}",
                server.endpoint
            ))
            .bearer_auth(credential)
            .header("Bitrouter-Installation-Id", installation)
            .send()
            .await?;
        ensure!(
            response.status().as_u16() == expected,
            "directory status mismatch for {query}"
        );
        ensure!(response.headers()["cache-control"] == "no-store");
        let body = response.text().await?;
        ensure!(!body.contains("secret-") && !body.contains(credential));
    }
    let response = client
        .get(format!("{}/control/v1/tenancy/directory", server.endpoint))
        .bearer_auth(OPERATOR)
        .send()
        .await?;
    ensure!(response.status() == 400 && response.headers()["cache-control"] == "no-store");

    let first = server
        .client
        .directory(&fixture.installation, None, None, 1)
        .await?;
    let cursor = first.next_after.context("continuation")?;
    // The very next HTTP request must authenticate against the current control
    // configuration; a previously accepted cursor does not preserve authority.
    use tower::ServiceExt;
    let url = reqwest::Url::parse_with_params(
        "http://localhost/control/v1/tenancy/directory",
        &[
            (
                "authority_revision",
                first.authority_revision.get().to_string(),
            ),
            ("after", cursor.as_str().to_owned()),
        ],
    )?;
    let response = router(&fixture.service, ControlScope::Read)?
        .oneshot(
            axum::http::Request::builder()
                .uri(format!("{}?{}", url.path(), url.query().context("query")?))
                .header("authorization", format!("Bearer {OPERATOR}"))
                .header("Bitrouter-Installation-Id", fixture.installation.as_str())
                .body(axum::body::Body::empty())?,
        )
        .await?;
    ensure!(response.status() == 403);
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn directory_retains_revoked_expired_audit_and_detached_owner_facts() -> Result<()> {
    let fixture = Fixture::new().await?;
    let server = Server::start(&fixture.service).await?;
    let revoked = fixture.issue("directory-revoked").await?;
    let revoke = server
        .client
        .revoke_credential(
            &fixture.tenant,
            &revoked.credential_id,
            &Mutation {
                operation_id: "directory-revoke".try_into()?,
                installation_id: fixture.installation.clone(),
                expected_revision: revoked.credential_revision.clone(),
            },
        )
        .await?;
    let mut input = fixture.issuance("directory-expiring")?;
    let deadline = Utc::now() + chrono::Duration::milliseconds(500);
    input.expires_at = deadline
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        .try_into()?;
    let expired = server
        .client
        .issue_credential(&fixture.tenant, &input)
        .await?;
    let audit = server
        .client
        .issue_audit_reader(
            &fixture.tenant,
            &AuditReaderCreate {
                operation_id: "directory-audit".try_into()?,
                expected_authority_revision: expired.authority_revision,
                principal_id: fixture.principal.clone(),
                expires_at: expiry()?,
                installation_id: fixture.installation.clone(),
            },
        )
        .await?;
    tokio::time::sleep(std::time::Duration::from_millis(550)).await;
    let detached = server
        .client
        .detach_binding_management(
            &fixture.tenant,
            &fixture.binding,
            &Mutation {
                operation_id: "directory-detach".try_into()?,
                expected_revision: fixture
                    .binding_result
                    .resource_revision
                    .clone()
                    .context("binding revision")?,
                installation_id: fixture.installation.clone(),
            },
        )
        .await?;
    let page = server.page(&fixture.installation).await?;
    ensure!(Some(page.authority_revision) == detached.authority_revision);
    ensure!(page.entries.len() == 6 && page.next_after.is_none());
    ensure!(page.entries.iter().any(|entry| matches!(entry, DirectoryEntryV1::Binding { management_owner: None, revision_id, .. } if Some(revision_id) == detached.resource_revision.as_ref())));
    for (issued, state, is_audit) in [
        (&revoked, CredentialState::Revoked, false),
        (&expired, CredentialState::Active, false),
        (&audit, CredentialState::Active, true),
    ] {
        let entry = page.entries.iter().find(|entry| matches!(entry, DirectoryEntryV1::Credential { credential_id, .. } if credential_id == &issued.credential_id)).context("durable credential missing")?;
        let DirectoryEntryV1::Credential {
            recorded_state,
            binding_id,
            permissions,
            audit_reader,
            revision_id,
            expires_at,
            ..
        } = entry
        else {
            anyhow::bail!("credential variant");
        };
        ensure!(*recorded_state == state && *audit_reader == is_audit);
        ensure!(binding_id.is_none() == is_audit && permissions.is_empty() == is_audit);
        if state == CredentialState::Revoked {
            ensure!(Some(revision_id) == revoke.resource_revision.as_ref());
        }
        if issued.credential_id == expired.credential_id {
            ensure!(expires_at == &input.expires_at);
        }
        let body = serde_json::to_string(&page)?;
        let secret = issued.secret.as_ref().context("secret")?;
        ensure!(!body.contains(secret) && !body.contains(&crate::auth::keys::hash_key(secret)));
    }
    let current = fixture.service.current().map_err(failure)?;
    ensure!(!current.credentials.contains_key(&revoked.credential_id));
    ensure!(!current.credentials.contains_key(&expired.credential_id));
    for field in ["key_hash", "secret", "ciphertext", "created_at"] {
        ensure!(!serde_json::to_string(&page)?.contains(field));
    }
    ensure!(format!("{page:?}") == "DirectoryPageV1 { .. }");
    fixture.service.shutdown().await?;
    // Durable history must remain visible after the live auth map is rebuilt.
    let reopened = Service::open(fixture.db.clone(), None).await?;
    let restarted = Server::start(&reopened).await?;
    ensure!(restarted.page(&fixture.installation).await? == page);
    reopened.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn directory_pages_are_complete_sorted_and_fenced_by_every_authority_change() -> Result<()> {
    let fixture = Fixture::new().await?;
    let server = Server::start(&fixture.service).await?;
    for id in ["z", "Z", "a", "A", "a0"] {
        server
            .client
            .put_tenant(
                &id.try_into()?,
                &TenantPut {
                    operation_id: format!("directory-tenant-{id}").try_into()?,
                    expected_revision: None,
                    state: TenantState::Active,
                    installation_id: fixture.installation.clone(),
                },
            )
            .await?;
        server
            .client
            .put_binding(
                &id.try_into()?,
                &fixture.binding,
                &BindingPut {
                    operation_id: format!("directory-binding-{id}").try_into()?,
                    expected_revision: None,
                    principal_id: fixture.principal.clone(),
                    role: TenantRole::TenantAdmin,
                    scope: super::super::mutation::tenant_scope(),
                    expires_at: expiry()?,
                    expected_management_owner: None,
                    installation_id: fixture.installation.clone(),
                },
            )
            .await?;
        server
            .client
            .issue_credential(
                &id.try_into()?,
                &fixture.issuance(&format!("directory-credential-{id}"))?,
            )
            .await?;
    }
    fixture.issue("directory-live").await?;
    fixture.issue("directory-live-second").await?;
    let full = server.page(&fixture.installation).await?;
    let mut collected = Vec::new();
    let mut cursor = None;
    loop {
        let page = server
            .client
            .directory(
                &fixture.installation,
                Some(full.authority_revision),
                cursor.as_ref(),
                2,
            )
            .await?;
        ensure!(page.entries.len() <= 2);
        collected.extend(page.entries);
        cursor = page.next_after;
        if cursor.is_none() {
            break;
        }
    }
    ensure!(collected == full.entries);
    let tenants: Vec<_> = collected
        .iter()
        .filter_map(|entry| match entry {
            DirectoryEntryV1::Tenant { tenant_id, .. } => Some(tenant_id.as_str()),
            _ => None,
        })
        .collect();
    ensure!(tenants == ["A", "Z", "a", "a0", "tenant-a", "z"]);
    let mut credential_tenants: Vec<_> = collected
        .iter()
        .filter_map(|entry| match entry {
            DirectoryEntryV1::Credential { tenant_id, .. } => Some(tenant_id.as_str()),
            _ => None,
        })
        .collect();
    ensure!(credential_tenants.len() == tenants.len() + 1);
    credential_tenants.dedup();
    ensure!(credential_tenants == tenants);
    let first = server
        .client
        .directory(&fixture.installation, None, None, 1)
        .await?;
    server
        .client
        .put_tenant(
            &fixture.tenant,
            &TenantPut {
                operation_id: "directory-suspend".try_into()?,
                expected_revision: fixture
                    .service
                    .current()
                    .map_err(failure)?
                    .tenants
                    .get(&fixture.tenant)
                    .map(|tenant| tenant.revision.clone()),
                state: TenantState::Suspended,
                installation_id: fixture.installation.clone(),
            },
        )
        .await?;
    let error = server
        .client
        .directory(
            &fixture.installation,
            Some(first.authority_revision),
            first.next_after.as_ref(),
            1,
        )
        .await
        .err()
        .context("mutation did not conflict")?;
    ensure!(
        matches!(error, bitrouter_sdk::error::Error::Tenancy(ref failure) if failure.status == 409 && failure.body.as_ref().is_some_and(|error| error.code == ErrorCode::RevisionConflict))
    );
    let fresh = server.page(&fixture.installation).await?;
    ensure!(fresh.authority_revision > first.authority_revision);
    ensure!(fresh.entries.iter().any(|entry| matches!(entry, DirectoryEntryV1::Tenant { tenant_id, state: TenantState::Suspended, .. } if tenant_id == &fixture.tenant)));
    fixture.service.shutdown().await?;
    Ok(())
}
