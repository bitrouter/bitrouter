//! Revocation and cancellation after a real provider POST, before its first response.
use super::*;
use bitrouter_ai::tenancy::{
    authority::{BindingPut, GrantScope, TenantPut, TenantRole, TenantState},
    operation::{ErrorCode, Mutation, OperationStatus},
};
use bitrouter_sdk::{error::Error as ControlError, gateway::tenancy::Error as DispatchError};
use tokio::sync::Notify;

pub(super) struct Gate {
    pub(super) entered: Notify,
    pub(super) release: Semaphore,
}

struct HeldProvider(Arc<Gate>);
impl HeldProvider {
    async fn entered(&self) -> Result<()> {
        tokio::time::timeout(TIMEOUT, self.0.entered.notified())
            .await
            .context("provider did not reach the response barrier")
    }
    fn release(&self) {
        self.0.release.add_permits(1);
    }
}
impl Drop for HeldProvider {
    fn drop(&mut self) {
        // Any failed assertion also releases the provider waiter during cleanup.
        self.0.release.close();
    }
}

impl OrderedHarness {
    fn hold_next_provider(&self) -> Result<HeldProvider> {
        let gate = Arc::new(Gate {
            entered: Notify::new(),
            release: Semaphore::new(0),
        });
        let mut held = self
            .held_provider
            .lock()
            .map_err(|_| anyhow::anyhow!("provider barrier poisoned"))?;
        ensure!(held.is_none(), "provider barrier already installed");
        *held = Some(gate.clone());
        Ok(HeldProvider(gate))
    }
}

type Submission = AbortOnDropHandle<std::result::Result<(), DispatchError>>;

fn submit(harness: &OrderedHarness, binding: &OrderedDispatchBindingV2) -> Result<Submission> {
    let client = GatewayClient::with_options(
        &harness.endpoint,
        None,
        TransportOptions {
            http_version: HttpVersion::Http1Only,
            request_timeout: Some(Duration::from_secs(30)),
            ..Default::default()
        },
    )?;
    let token = harness.token()?.to_owned();
    let binding = binding.clone();
    let request: ChatRequest = serde_json::from_value(json!({
        "model":MODEL,"messages":[{"role":"user","content":"hello"}],"stream":false
    }))?;
    Ok(AbortOnDropHandle::new(tokio::spawn(async move {
        client
            .ordered_tenant(&token, &binding)?
            .chat_reply(&request)
            .await
            .map(|_| ())
    })))
}

async fn drained_receipt(harness: &OrderedHarness, ordinals: &[u16]) -> Result<ExecutionReceiptV2> {
    tokio::time::timeout(TIMEOUT, harness.fixture.service.admission.wait_drained()).await?;
    let control = harness.control(true, HttpVersion::Http1Only)?;
    let page = control
        .executions(
            &harness.fixture.installation,
            &harness.fixture.tenant,
            &"ordered-correlation".try_into()?,
            None,
            100,
        )
        .await?;
    ensure!(page.execution_ids.len() == 1 && !page.multiple_executions);
    let receipt = control
        .execution_v2(
            &harness.fixture.installation,
            &harness.fixture.tenant,
            &page.execution_ids[0],
        )
        .await?;
    ensure!(receipt.accepted_selection.candidate_indices == [1, 0]);
    ensure!(receipt.accepted_selection.route == harness.route);
    ensure!(receipt.accepted_selection.inbound_protocol == GenerationProtocol::ChatCompletions);
    ensure!(
        receipt
            .attempts
            .iter()
            .map(|attempt| attempt.candidate_index)
            .collect::<Vec<_>>()
            == ordinals
    );
    let evidence = control.usage_evidence_v2(&receipt).await?;
    evidence.validate_against(&receipt)?;
    ensure!(evidence.attempts.len() == ordinals.len() && !evidence.truncated);
    ensure!(
        harness.observed()?
            == ordinals
                .iter()
                .map(|index| format!("model-{index}"))
                .collect::<Vec<_>>()
    );
    Ok(receipt)
}

async fn revoke_after_send(harness: &OrderedHarness, change: &str) -> Result<(u16, ErrorCode)> {
    let fixture = &harness.fixture;
    let control = harness.control(true, HttpVersion::Http1Only)?;
    let current = fixture.service.current().map_err(failure)?;
    let (operation, status, code) = match change {
        "binding" => {
            let binding = crate::tenancy::store::directory::binding_in(
                &fixture.db,
                &fixture.tenant,
                &fixture.binding,
                64 * 1024,
            )
            .await?
            .context("recipient binding")?;
            let mut retained_slot = harness.route.key();
            retained_slot.resource_kind = ResourceKind::Credential;
            retained_slot.resource_id = "static-slot-1".try_into()?;
            let result = control
                .put_binding(
                    &fixture.tenant,
                    &fixture.binding,
                    &BindingPut {
                        operation_id: "remove-fallback-grant".try_into()?,
                        expected_revision: Some(binding.revision.clone()),
                        principal_id: fixture.principal.clone(),
                        role: TenantRole::TenantAdmin,
                        scope: GrantScope::Resources {
                            resources: vec![harness.route.key(), retained_slot],
                        },
                        expires_at: binding.expires.wire.clone(),
                        expected_management_owner: Some(match &fixture.operator {
                            crate::tenancy::model::Caller::Installation(id) => id.clone(),
                            _ => anyhow::bail!("fixture operator missing"),
                        }),
                        installation_id: fixture.installation.clone(),
                    },
                )
                .await?;
            (result, 403, ErrorCode::Forbidden)
        }
        "tenant" => {
            let tenant = current.tenants.get(&fixture.tenant).context("tenant")?;
            let result = control
                .put_tenant(
                    &fixture.tenant,
                    &TenantPut {
                        operation_id: "suspend-before-fallback".try_into()?,
                        expected_revision: Some(tenant.revision.clone()),
                        state: TenantState::Suspended,
                        installation_id: fixture.installation.clone(),
                    },
                )
                .await?;
            (result, 403, ErrorCode::TenantInactive)
        }
        _ => {
            let result = control
                .revoke_credential(
                    &fixture.tenant,
                    &harness.credential.credential_id,
                    &Mutation {
                        operation_id: "revoke-before-fallback".try_into()?,
                        expected_revision: harness.credential.credential_revision.clone(),
                        installation_id: fixture.installation.clone(),
                    },
                )
                .await?;
            (result, 401, ErrorCode::Unauthorized)
        }
    };
    ensure!(operation.status == OperationStatus::Applied);
    Ok((status, code))
}

#[tokio::test]
async fn revocation_between_actual_provider_send_and_fallback_prevents_second_send() -> Result<()> {
    for change in ["unchanged", "binding", "tenant", "credential"] {
        let harness = OrderedHarness::new(2, 64, None).await?;
        let binding = harness
            .binding(vec![1, 0], GenerationProtocol::ChatCompletions)
            .await?;
        let held = harness.hold_next_provider()?;
        let caller = submit(&harness, &binding)?;
        held.entered().await?;
        ensure!(!caller.is_finished() && harness.observed()? == ["model-1"]);
        if change == "unchanged" {
            // Calibrate the same barrier: its release alone must permit the
            // ordinary definitive-503 fallback, ending at successful model 0.
            held.release();
            tokio::time::timeout(TIMEOUT, caller).await???;
            let receipt = drained_receipt(&harness, &[1, 0]).await?;
            ensure!(
                receipt.state == ReceiptState::Committed && receipt.outcome == Outcome::Completed
            );
            ensure!(
                receipt
                    .attempts
                    .iter()
                    .map(|attempt| attempt.observed_http_status)
                    .collect::<Vec<_>>()
                    == [Some(503), Some(200)]
            );
            tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await??;
            continue;
        }
        let (status, code) =
            tokio::time::timeout(TIMEOUT, revoke_after_send(&harness, change)).await??;
        held.release();
        ensure!(tokio::time::timeout(TIMEOUT, caller).await??.is_err());
        let receipt = drained_receipt(&harness, &[1]).await?;
        ensure!(
            receipt.outcome != Outcome::Completed
                && receipt.attempts[0].outcome != Outcome::Completed
        );
        // Operator inspection remains available without restoring the revoked
        // caller's separate whole-tenant receipt-read grant.
        let rejected = harness
            .control(false, HttpVersion::Http1Only)?
            .execution_v2(
                &harness.fixture.installation,
                &harness.fixture.tenant,
                &receipt.execution_id,
            )
            .await
            .err()
            .context("revoked caller retained receipt read authority")?;
        let ControlError::Tenancy(failure) = rejected else {
            anyhow::bail!("receipt refusal did not come from HTTP");
        };
        ensure!(failure.status == status && failure.body_error.is_none());
        ensure!(failure.body.is_some_and(|body| body.code == code));
        tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await??;
    }
    Ok(())
}

#[tokio::test]
async fn aborting_sdk_caller_while_provider_is_held_never_reaches_fallback() -> Result<()> {
    let harness = OrderedHarness::new(2, 64, None).await?;
    let binding = harness
        .binding(vec![1, 0], GenerationProtocol::ChatCompletions)
        .await?;
    let held = harness.hold_next_provider()?;
    let caller = submit(&harness, &binding)?;
    held.entered().await?;
    ensure!(!caller.is_finished() && harness.observed()? == ["model-1"]);
    caller.abort();
    ensure!(
        tokio::time::timeout(TIMEOUT, caller)
            .await?
            .is_err_and(|error| error.is_cancelled())
    );
    // Drain before releasing the provider: neither a response nor the 30-second
    // SDK timeout can explain cancellation within this shorter bounded wait.
    let receipt = drained_receipt(&harness, &[1]).await?;
    ensure!(held.0.release.available_permits() == 0);
    ensure!(receipt.state == ReceiptState::Unknown && receipt.outcome == Outcome::Unknown);
    ensure!(receipt.attempts[0].outcome == Outcome::Unknown);
    ensure!(receipt.attempts[0].observed_http_status.is_none());
    held.release();
    tokio::task::yield_now().await;
    let after = drained_receipt(&harness, &[1]).await?;
    ensure!(serde_json::to_value(after)? == serde_json::to_value(receipt)?);
    tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await??;
    ensure!(harness.observed()? == ["model-1"]);
    Ok(())
}
