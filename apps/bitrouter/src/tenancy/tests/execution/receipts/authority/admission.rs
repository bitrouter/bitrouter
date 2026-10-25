use super::*;
use bitrouter_ai::tenancy::authority::{
    BindingPut, GrantScope, TenantPut, TenantRole, TenantState,
};

#[tokio::test]
async fn queued_http_rechecks_revoke_suspend_and_scope_without_a_second_receipt_or_attempt()
-> Result<()> {
    for change in ["revoke", "suspend", "scope"] {
        let harness = Harness::new(
            "chat_completions",
            StatusCode::OK,
            json_response("chat_completions", full_usage("chat_completions"))?,
        )
        .await?;
        let checked = async {
            let mut input = harness
                .fixture
                .configuration_input("authority-queue-config")?;
            input.document.routes = vec![harness.route.clone()];
            input.document.limits.active_executions = 1;
            input.document.limits.queued_work = 1;
            let revision = match harness
                .fixture
                .service
                .execute(
                    harness.fixture.operator.clone(),
                    Command::Configuration(harness.fixture.tenant.clone(), input),
                )
                .await
                .map_err(failure)?
            {
                StoredResponse::Configuration(revision) => revision,
                _ => anyhow::bail!("queue configuration"),
            };
            let ingress = headers(
                &harness.fixture,
                harness.credential.secret.as_deref().context("token")?,
                &revision.revision_id,
            )?;
            let held = super::super::super::admitted(&harness.fixture, &ingress).await?;
            let id = held.id().clone();
            let client = Harness::client(false)?;
            let request = client
                .post(format!("{}{}", harness.base, harness.path))
                .headers(ingress)
                .json(&harness.request);
            let queued = AbortOnDropHandle::new(tokio::spawn(async move { request.send().await }));
            tokio::time::timeout(Duration::from_secs(2), async {
                while harness.fixture.service.admission.queued_for_test() != 1 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .context("HTTP request did not reach admission queue")?;
            ensure!(!queued.is_finished() && harness.calls.load(Ordering::SeqCst) == 0);
            let fixture = &harness.fixture;
            let command = match change {
                "revoke" => revoke(fixture, &harness.credential)?,
                "suspend" => Command::Tenant(
                    fixture.tenant.clone(),
                    TenantPut {
                        operation_id: "receipt-queue-suspend".try_into()?,
                        expected_revision: Some(
                            fixture
                                .service
                                .current()
                                .map_err(failure)?
                                .tenants
                                .get(&fixture.tenant)
                                .context("tenant")?
                                .revision
                                .clone(),
                        ),
                        state: TenantState::Suspended,
                        installation_id: fixture.installation.clone(),
                    },
                ),
                _ => Command::Binding(
                    fixture.tenant.clone(),
                    fixture.binding.clone(),
                    BindingPut {
                        operation_id: "receipt-queue-scope".try_into()?,
                        expected_revision: fixture.binding_result.resource_revision.clone(),
                        principal_id: fixture.principal.clone(),
                        role: TenantRole::TenantAdmin,
                        scope: GrantScope::Resources {
                            resources: vec![harness.route.key()],
                        },
                        expires_at: crate::tenancy::tests::expiry()?,
                        expected_management_owner: Some(
                            crate::tenancy::operator_principal("test-operator").map_err(failure)?,
                        ),
                        installation_id: fixture.installation.clone(),
                    },
                ),
            };
            fixture
                .service
                .execute(fixture.operator.clone(), command)
                .await
                .map_err(failure)?;
            drop(held);
            let response = tokio::time::timeout(TIMEOUT, queued).await???;
            ensure!(
                response.status()
                    == if change == "revoke" {
                        StatusCode::UNAUTHORIZED
                    } else {
                        StatusCode::FORBIDDEN
                    }
            );
            ensure!(response.headers().get("x-bitrouter-request-id").is_none());
            let body: Value = response.json().await?;
            ensure!(
                body["code"]
                    == match change {
                        "revoke" => "unauthorized",
                        "suspend" => "tenant_inactive",
                        _ => "forbidden",
                    }
            );
            drain(fixture).await?;
            let receipt = read_one(fixture, fixture.operator.clone(), &id).await?;
            no_attempt(&receipt, &id)?;
            let page = page_as(fixture, fixture.operator.clone()).await?;
            ensure!(page.execution_ids == vec![id] && !page.multiple_executions);
            ensure!(harness.calls.load(Ordering::SeqCst) == 0);
            Ok(())
        }
        .await;
        finish(&harness, checked).await?;
    }
    Ok(())
}
