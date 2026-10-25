use super::*;
use bitrouter_ai::tenancy::authority::{
    BindingPut, GrantScope, TenantPut, TenantRole, TenantState,
};

async fn harness() -> Result<Harness> {
    Harness::with_namespace_resources(
        "chat_completions",
        StatusCode::OK,
        serde_json::to_string(&json_response(
            "chat_completions",
            full_usage("chat_completions"),
        )?)?,
        false,
        None,
        true,
    )
    .await
}

async fn second_tenant(first: &Fixture) -> Result<Fixture> {
    let tenant: bitrouter_ai::tenancy::identity::TenantId = "tenant-b".try_into()?;
    first
        .service
        .execute(
            first.operator.clone(),
            Command::Tenant(
                tenant.clone(),
                TenantPut {
                    operation_id: "receipt-second-tenant".try_into()?,
                    expected_revision: None,
                    state: TenantState::Active,
                    installation_id: first.installation.clone(),
                },
            ),
        )
        .await
        .map_err(failure)?;
    let binding_result = match first
        .service
        .execute(
            first.operator.clone(),
            Command::Binding(
                tenant.clone(),
                first.binding.clone(),
                BindingPut {
                    operation_id: "receipt-second-binding".try_into()?,
                    expected_revision: None,
                    principal_id: first.principal.clone(),
                    role: TenantRole::TenantAdmin,
                    scope: GrantScope::Tenant {
                        operator_pools: vec![],
                    },
                    expires_at: crate::tenancy::tests::expiry()?,
                    expected_management_owner: None,
                    installation_id: first.installation.clone(),
                },
            ),
        )
        .await
        .map_err(failure)?
    {
        StoredResponse::Operation(result) => result,
        _ => anyhow::bail!("binding operation"),
    };
    Ok(Fixture {
        service: first.service.clone(),
        db: first.db.clone(),
        operator: first.operator.clone(),
        installation: first.installation.clone(),
        tenant,
        principal: first.principal.clone(),
        binding: first.binding.clone(),
        binding_result,
    })
}

#[tokio::test]
async fn same_correlation_and_logical_names_remain_scoped_in_one_service_and_database() -> Result<()>
{
    let mut harness = harness().await?;
    let checked = async {
        let second = second_tenant(&harness.fixture).await?;
        let mut route = harness.route.clone();
        route.owner_id = second.tenant.as_str().try_into()?;
        let revision = draft(&second, &route, "second-receipt-config", 25).await?;
        let credential = second.issue("second-receipt-client").await?;
        let client = Harness::client(false)?;
        // Obtain both bindings after the last authority publication.
        let first_headers = headers(
            &harness.fixture,
            harness
                .credential
                .secret
                .as_deref()
                .context("first token")?,
            &harness.revision.revision_id,
        )?;
        let second_headers = headers(
            &second,
            credential.secret.as_deref().context("second token")?,
            &revision.revision_id,
        )?;
        let mut ids = Vec::new();
        for ingress in [first_headers, second_headers] {
            let response = client
                .post(format!("{}{}", harness.base, harness.path))
                .headers(ingress)
                .json(&harness.request)
                .send()
                .await?;
            ensure!(response.status() == StatusCode::OK);
            ids.push(execution_id(&response)?);
            let _ = response.bytes().await?;
        }
        ensure!(ids.len() == 2 && ids[0] != ids[1]);
        drain(&harness.fixture).await?;
        let first_caller = harness
            .fixture
            .service
            .authenticate(
                harness
                    .credential
                    .secret
                    .as_deref()
                    .context("first token")?,
            )
            .map_err(failure)?;
        let second_caller = second
            .service
            .authenticate(credential.secret.as_deref().context("second token")?)
            .map_err(failure)?;
        let mut targets = Vec::new();
        for (index, (fixture, caller)) in
            [(&harness.fixture, first_caller), (&second, second_caller)]
                .into_iter()
                .enumerate()
        {
            let receipt = read_one(fixture, caller.clone(), &ids[index]).await?;
            ensure!(receipt.execution_id == ids[index] && receipt.tenant_id == fixture.tenant);
            ensure!(
                receipt.installation_id == fixture.installation
                    && receipt.correlation_id.as_str() == "same-correlation"
            );
            ensure!(
                receipt.attempts.len() == 1 && receipt.attempts[0].outcome == Outcome::Completed
            );
            targets.push(
                receipt.attempts[0]
                    .target
                    .clone()
                    .context("actual target")?,
            );
            let page = page_as(fixture, caller.clone()).await?;
            ensure!(
                page.execution_ids == vec![ids[index].clone()]
                    && !page.multiple_executions
                    && page.next_after.is_none()
            );
            let foreign = fixture
                .service
                .read_receipt(
                    caller.clone(),
                    fixture.tenant.clone(),
                    read::Request::One(ids[1 - index].clone()),
                )
                .await;
            ensure!(matches!(foreign, Err(error) if error.0.code == ErrorCode::NotFound));
            let other_tenant = if index == 0 {
                second.tenant.clone()
            } else {
                harness.fixture.tenant.clone()
            };
            let foreign = fixture
                .service
                .read_receipt(
                    caller,
                    other_tenant,
                    read::Request::One(ids[1 - index].clone()),
                )
                .await;
            ensure!(matches!(foreign, Err(error) if error.0.code == ErrorCode::Forbidden));
        }
        ensure!(
            targets[0].credential_id != targets[1].credential_id
                && targets[0].account_id != targets[1].account_id
        );
        ensure!(harness.calls.load(Ordering::SeqCst) == 2);
        for _ in 0..2 {
            ensure!(
                harness
                    .received
                    .try_recv()
                    .context("actual authenticated POST")?["model"]
                    == "model-a"
            );
        }
        ensure!(harness.received.try_recv().is_err());
        Ok(())
    }
    .await;
    finish(&harness, checked).await
}

#[tokio::test]
async fn tenant_binding_does_not_widen_full_partial_or_disjoint_resource_credentials() -> Result<()>
{
    for scope in ["full", "partial", "disjoint"] {
        // The real authority permits two unexpired credentials per principal.
        // Keep the tenant-wide fixture key plus exactly one resource-only key.
        let harness = harness().await?;
        let checked = async {
            let client = Harness::client(false)?;
            let mut accepted_id = None;
            if scope != "full" {
                // Seed the receipt with the existing tenant-wide key, so denied
                // readers must refuse a real row rather than an absent record.
                let response = harness.send(&client).send().await?;
                ensure!(response.status() == StatusCode::OK);
                accepted_id = Some(execution_id(&response)?);
                let _ = response.bytes().await?;
                drain(&harness.fixture).await?;
            }
            let mut route = harness.route.clone();
            let mut slot = harness.slot.clone();
            if scope == "disjoint" {
                route.resource_id = "disjoint-route".try_into()?;
                slot.resource_id = "disjoint-slot".try_into()?;
            }
            let mut input = harness
                .fixture
                .issuance(&format!("receipt-{scope}-credential"))?;
            input.scope = GrantScope::Resources {
                resources: if scope == "partial" {
                    vec![route.key()]
                } else {
                    vec![route.key(), slot.key()]
                },
            };
            let issued = match harness
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
                StoredResponse::Failure(error) => {
                    anyhow::bail!(
                        "{scope} resource credential issuance failed: {:?}",
                        error.code
                    )
                }
                _ => anyhow::bail!("{scope} resource credential response was not issuance"),
            };
            let token = issued.secret.as_deref().context("resource token")?;
            let ingress = headers(&harness.fixture, token, &harness.revision.revision_id)?;
            let response = client
                .post(format!("{}{}", harness.base, harness.path))
                .headers(ingress)
                .json(&harness.request)
                .send()
                .await?;
            if scope == "full" {
                ensure!(response.status() == StatusCode::OK);
                accepted_id = Some(execution_id(&response)?);
            } else {
                ensure!(response.status() == StatusCode::FORBIDDEN);
                ensure!(
                    response.headers().get("x-bitrouter-request-id").is_none(),
                    "denied before admission must not create a receipt"
                );
            }
            let _ = response.bytes().await?;
            let id = accepted_id
                .as_ref()
                .context("full resource credential never sent")?;
            let caller = harness
                .fixture
                .service
                .authenticate(token)
                .map_err(failure)?;
            for request in [
                read::Request::One(id.clone()),
                read::Request::Page {
                    correlation: "same-correlation".try_into()?,
                    after: None,
                    limit: 100,
                },
            ] {
                let rejected = harness
                    .fixture
                    .service
                    .read_receipt(caller.clone(), harness.fixture.tenant.clone(), request)
                    .await;
                ensure!(
                    matches!(rejected, Err(error) if error.0.code == ErrorCode::Forbidden),
                    "resource-only credential acquired tenant-wide receipt access"
                );
            }
            drain(&harness.fixture).await?;
            let id = accepted_id.context("accepted execution")?;
            let receipt = harness.receipt(&client, &id).await?;
            ensure!(
                receipt.attempts.len() == 1 && receipt.attempts[0].outcome == Outcome::Completed
            );
            ensure!(harness.calls.load(Ordering::SeqCst) == 1);
            let page = harness.page().await?;
            ensure!(page.execution_ids == vec![id] && !page.multiple_executions);
            Ok(())
        }
        .await;
        finish(&harness, checked).await?;
    }
    Ok(())
}
