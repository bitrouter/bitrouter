use super::*;
use crate::tenancy::receipts::read;
use bitrouter_ai::tenancy::{
    execution::{ExecutionReceipt, Provenance, ReceiptState},
    identity::ExecutionId,
};

#[tokio::test]
async fn plan_wait_authority_changes_leave_one_exact_no_attempt_receipt() -> Result<()> {
    for change in ["revoke", "suspend", "scope"] {
        let provider = Provider::start(false).await?;
        let (fixture, route, _) = provision_protocol(&provider.url, "chat_completions").await?;
        let gate = Arc::new(Gate::default());
        let release = gate.release.clone().drop_guard();
        let checked = async {
            let (ingress, credential) = ready(&fixture, &route).await?;
            let app = host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                .with_tenancy(fixture.service.clone());
            let control = Arc::new(Control {
                hold: Some((Hold::Plan, gate.clone())),
                ..Default::default()
            });
            let observed = control.clone();
            let call = AbortOnDropHandle::new(tokio::spawn(async move {
                app.execute_native_controlled_with_headers(
                    prompt(),
                    CallerContext::local(),
                    ingress,
                    control,
                )
                .await
            }));
            tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
            let id: ExecutionId = {
                let plans = observed.plans.lock().await;
                ensure!(plans.len() == 1 && !plans[0].routes.is_empty());
                plans[0].request_id.as_str().try_into()?
            };
            ensure!(provider.calls() == 0 && !call.is_finished());
            change_authority(&fixture, &credential, &route, change).await?;
            gate.release.cancel();
            let error = tokio::time::timeout(TIMEOUT, call)
                .await??
                .err()
                .context("authority change allowed dispatch")?;
            let expected = if change == "revoke" {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::FORBIDDEN
            };
            ensure!(error.status() == expected.as_u16(), "{change}: {error}");
            tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
            let bytes = fixture
                .service
                .read_receipt(
                    fixture.operator.clone(),
                    fixture.tenant.clone(),
                    read::Request::One(id.clone()),
                )
                .await
                .map_err(failure)?;
            let receipt: ExecutionReceipt = serde_json::from_slice(&bytes)?;
            ensure!(receipt.execution_id == id && receipt.tenant_id == fixture.tenant);
            ensure!(
                receipt.principal_id == fixture.principal
                    && receipt.state == ReceiptState::Incomplete
            );
            ensure!(
                receipt.attempts.is_empty()
                    && receipt.usage.provenance == Provenance::Unknown
                    && !receipt.usage.complete
            );
            let bytes = fixture
                .service
                .read_receipt(
                    fixture.operator.clone(),
                    fixture.tenant.clone(),
                    read::Request::Page {
                        correlation: "same-correlation".try_into()?,
                        after: None,
                        limit: 100,
                    },
                )
                .await
                .map_err(failure)?;
            let page: bitrouter_ai::tenancy::execution::ExecutionPage =
                serde_json::from_slice(&bytes)?;
            ensure!(page.execution_ids == vec![id] && !page.multiple_executions);
            ensure!(provider.calls() == 0);
            Ok(())
        }
        .await;
        drop(release);
        finish(&fixture, provider, checked).await?;
    }
    Ok(())
}
