use super::*;
use crate::tenancy::{command::Command, store::StoredResponse};
use bitrouter_ai::tenancy::{
    authority::{AuditReaderCreate, Permission, TenantPut, TenantState},
    operation::{ErrorCode, Mutation},
};

#[tokio::test]
async fn protected_reads_bind_installation_and_namespace_and_require_receipt_permission()
-> Result<()> {
    let harness = Harness::new(
        "chat_completions",
        StatusCode::OK,
        json_response("chat_completions", full_usage("chat_completions"))?,
    )
    .await?;
    let client = Harness::client(false)?;
    let response = harness.send(&client).send().await?;
    ensure!(response.status() == StatusCode::OK);
    let id = execution_id(&response)?;
    let _ = response.bytes().await?;
    let endpoint = format!(
        "{}/control/v1/tenants/{}/executions/{id}",
        harness.base, harness.fixture.tenant
    );
    let token = harness.credential.secret.as_deref().context("token")?;
    for installation in [None, Some("other-installation")] {
        let mut request = client.get(&endpoint).bearer_auth(token);
        if let Some(installation) = installation {
            request = request.header("bitrouter-installation-id", installation);
        }
        let response = request.send().await?;
        ensure!(!response.status().is_success());
        let text = response.text().await?;
        ensure!(!text.contains("owned-upstream-secret") && !text.contains("input_tokens"));
    }
    let duplicate = client
        .get(&endpoint)
        .bearer_auth(token)
        .header(
            "bitrouter-installation-id",
            harness.fixture.installation.as_str(),
        )
        .header(
            "bitrouter-installation-id",
            harness.fixture.installation.as_str(),
        )
        .send()
        .await?;
    ensure!(!duplicate.status().is_success());
    let foreign = client
        .get(format!(
            "{}/control/v1/tenants/another-tenant/executions/{id}",
            harness.base
        ))
        .bearer_auth(token)
        .header(
            "bitrouter-installation-id",
            harness.fixture.installation.as_str(),
        )
        .send()
        .await?;
    ensure!(foreign.status() == StatusCode::FORBIDDEN);
    let mut input = harness.fixture.issuance("submit-only")?;
    input.permissions = vec![Permission::ExecutionSubmit, Permission::CredentialUse];
    let submit = match harness
        .fixture
        .service
        .execute(
            harness.fixture.operator.clone(),
            Command::Issue(harness.fixture.tenant.clone(), input),
        )
        .await
        .map_err(failure)?
    {
        StoredResponse::Issued(issued) => issued,
        _ => anyhow::bail!("expected credential"),
    };
    let denied = client
        .get(&endpoint)
        .bearer_auth(submit.secret.as_deref().context("submit token")?)
        .header(
            "bitrouter-installation-id",
            harness.fixture.installation.as_str(),
        )
        .send()
        .await?;
    ensure!(denied.status() == StatusCode::FORBIDDEN);
    let receipt = harness.receipt(&client, &id).await?;
    ensure!(receipt.execution_id == id);
    for query in [
        "correlation_id=same-correlation&limit=101",
        "correlation_id=same-correlation&limit=0",
        "correlation_id=same-correlation&limit=1&unknown=1",
    ] {
        let response = client
            .get(format!(
                "{}/control/v1/tenants/{}/executions?{query}",
                harness.base, harness.fixture.tenant
            ))
            .bearer_auth(token)
            .header(
                "bitrouter-installation-id",
                harness.fixture.installation.as_str(),
            )
            .send()
            .await?;
        ensure!(response.status() == StatusCode::BAD_REQUEST);
    }
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn suspend_denies_ordinary_receipts_but_audit_reader_remains_tenant_bound_and_revocable()
-> Result<()> {
    let harness = Harness::new(
        "chat_completions",
        StatusCode::OK,
        json_response("chat_completions", full_usage("chat_completions"))?,
    )
    .await?;
    let client = Harness::client(false)?;
    let response = harness.send(&client).send().await?;
    let id = execution_id(&response)?;
    let _ = response.bytes().await?;
    let audit = match harness
        .fixture
        .service
        .execute(
            harness.fixture.operator.clone(),
            Command::AuditReader(
                harness.fixture.tenant.clone(),
                AuditReaderCreate {
                    operation_id: "receipt-auditor".try_into()?,
                    principal_id: harness.fixture.principal.clone(),
                    expected_authority_revision: harness
                        .fixture
                        .service
                        .current()
                        .map_err(failure)?
                        .revision,
                    expires_at: super::super::super::expiry()?,
                    installation_id: harness.fixture.installation.clone(),
                },
            ),
        )
        .await
        .map_err(failure)?
    {
        StoredResponse::Issued(value) => value,
        _ => anyhow::bail!("audit credential"),
    };
    let ordinary = harness
        .fixture
        .service
        .authenticate(harness.credential.secret.as_deref().context("ordinary")?)
        .map_err(failure)?;
    let auditor = harness
        .fixture
        .service
        .authenticate(audit.secret.as_deref().context("audit")?)
        .map_err(failure)?;
    let revision = harness
        .fixture
        .service
        .current()
        .map_err(failure)?
        .tenants
        .get(&harness.fixture.tenant)
        .context("tenant")?
        .revision
        .clone();
    harness
        .fixture
        .service
        .execute(
            harness.fixture.operator.clone(),
            Command::Tenant(
                harness.fixture.tenant.clone(),
                TenantPut {
                    operation_id: "suspend-receipt-tenant".try_into()?,
                    expected_revision: Some(revision),
                    state: TenantState::Suspended,
                    installation_id: harness.fixture.installation.clone(),
                },
            ),
        )
        .await
        .map_err(failure)?;
    let rejected = harness
        .fixture
        .service
        .read_receipt(
            ordinary,
            harness.fixture.tenant.clone(),
            read::Request::One(id.clone()),
        )
        .await;
    ensure!(matches!(rejected,Err(error) if error.0.code==ErrorCode::TenantInactive));
    let bytes = harness
        .fixture
        .service
        .read_receipt(
            auditor.clone(),
            harness.fixture.tenant.clone(),
            read::Request::One(id.clone()),
        )
        .await
        .map_err(failure)?;
    let receipt: ExecutionReceipt = serde_json::from_slice(&bytes)?;
    ensure!(receipt.execution_id == id && receipt.principal_id == harness.fixture.principal);
    let foreign = harness
        .fixture
        .service
        .read_receipt(
            auditor.clone(),
            "another-tenant".try_into()?,
            read::Request::One(id.clone()),
        )
        .await;
    ensure!(matches!(foreign,Err(error) if error.0.code==ErrorCode::Forbidden));
    harness
        .fixture
        .service
        .execute(
            harness.fixture.operator.clone(),
            Command::Revoke(
                harness.fixture.tenant.clone(),
                audit.credential_id,
                Mutation {
                    operation_id: "revoke-receipt-auditor".try_into()?,
                    expected_revision: audit.credential_revision,
                    installation_id: harness.fixture.installation.clone(),
                },
            ),
        )
        .await
        .map_err(failure)?;
    let revoked = harness
        .fixture
        .service
        .read_receipt(
            auditor,
            harness.fixture.tenant.clone(),
            read::Request::One(id),
        )
        .await;
    ensure!(matches!(revoked,Err(error) if error.0.code==ErrorCode::Unauthorized));
    ensure!(harness.calls.load(Ordering::SeqCst) == 1);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn unauthenticated_attempt_creates_no_execution_and_lookup_cannot_be_used_as_submit()
-> Result<()> {
    let harness = Harness::new(
        "chat_completions",
        StatusCode::OK,
        json_response("chat_completions", full_usage("chat_completions"))?,
    )
    .await?;
    let client = Harness::client(false)?;
    let response = client
        .post(format!("{}{}", harness.base, harness.path))
        .headers({
            let mut headers = harness.headers.clone();
            headers.remove("authorization");
            headers
        })
        .json(&harness.request)
        .send()
        .await?;
    ensure!(response.status() == StatusCode::UNAUTHORIZED);
    ensure!(response.headers().get("x-bitrouter-request-id").is_none());
    let page = harness.page().await?;
    ensure!(page.execution_ids.is_empty() && !page.multiple_executions);
    ensure!(harness.calls.load(Ordering::SeqCst) == 0);
    harness.fixture.service.shutdown().await?;
    Ok(())
}
