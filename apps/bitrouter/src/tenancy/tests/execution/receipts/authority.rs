//! Current grants across real waits; receipts never manufacture send authority.
use super::*;
use crate::tenancy::{command::Command, model::Caller, store::StoredResponse};
use bitrouter_ai::tenancy::{
    execution::{Outcome, Provenance, ReceiptState},
    operation::{ErrorCode, Mutation},
};

mod admission;
mod namespaces;
mod queued;

fn revoke(fixture: &Fixture, credential: &CredentialIssued) -> Result<Command> {
    Ok(Command::Revoke(
        fixture.tenant.clone(),
        credential.credential_id.clone(),
        Mutation {
            operation_id: "authority-revoke".try_into()?,
            expected_revision: credential.credential_revision.clone(),
            installation_id: fixture.installation.clone(),
        },
    ))
}

async fn read_one(fixture: &Fixture, caller: Caller, id: &ExecutionId) -> Result<ExecutionReceipt> {
    let bytes = fixture
        .service
        .read_receipt(
            caller,
            fixture.tenant.clone(),
            read::Request::One(id.clone()),
        )
        .await
        .map_err(failure)?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn page_as(fixture: &Fixture, caller: Caller) -> Result<ExecutionPage> {
    let bytes = fixture
        .service
        .read_receipt(
            caller,
            fixture.tenant.clone(),
            read::Request::Page {
                correlation: "same-correlation".try_into()?,
                after: None,
                limit: 100,
            },
        )
        .await
        .map_err(failure)?;
    Ok(serde_json::from_slice(&bytes)?)
}

async fn drain(fixture: &Fixture) -> Result<()> {
    tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
    Ok(())
}

async fn finish(harness: &Harness, checked: Result<()>) -> Result<()> {
    let stopped = tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await;
    checked?;
    stopped.context("receipt authority cleanup timeout")??;
    Ok(())
}

fn no_attempt(receipt: &ExecutionReceipt, id: &ExecutionId) -> Result<()> {
    ensure!(receipt.execution_id == *id && receipt.state == ReceiptState::Incomplete);
    ensure!(receipt.attempts.is_empty(), "denied work armed an attempt");
    ensure!(receipt.usage.provenance == Provenance::Unknown && !receipt.usage.complete);
    ensure!(
        receipt.usage.input_tokens.is_none()
            && receipt.usage.output_tokens.is_none()
            && receipt.usage.cache_read_tokens.is_none()
            && receipt.usage.cache_write_tokens.is_none()
            && receipt.usage.reasoning_tokens.is_none()
    );
    Ok(())
}

// Install immutable fixture resources before any gateway or admitted work exists.
// The real authority commands, credentials and requests are created afterwards.
pub(super) async fn provision_resources(
    fixture: &mut Fixture,
    route: &bitrouter_ai::tenancy::configuration::ResourceRef,
    slot: &bitrouter_ai::tenancy::configuration::ResourceRef,
    url: &str,
) -> Result<()> {
    fixture.service.shutdown().await?;
    let mut resources = Vec::new();
    for second_tenant in [false, true] {
        let mut route = route.clone();
        let mut slot = slot.clone();
        if second_tenant {
            route.owner_id = "tenant-b".try_into()?;
            slot.owner_id = "tenant-b".try_into()?;
        } else {
            route.resource_id = "disjoint-route".try_into()?;
            slot.resource_id = "disjoint-slot".try_into()?;
        }
        resources.push(
            json!({"reference":slot,"definition":{"kind":"credential","credential":{
            "type":"api_key","data":{"value":"owned-upstream-secret"}}}}),
        );
        resources.push(json!({"reference":route,"definition":{"kind":"route","selector":"mock:model-a","policy":null,
            "routing":{"providers":{"mock":{"api_base":url,"api_protocol":[{"*":"chat_completions"}],"models":[{"id":"model-a"}]}}},
            "credentials":[{"provider":"mock","account":"default","credential":slot}]}}));
    }
    fixture
        .service
        .resources
        .provision_bytes(
            &fixture.db,
            &serde_json::to_vec(&json!({
                "format":1,"installation_id":fixture.installation,"resources":resources,
            }))?,
        )
        .await?;
    fixture.service = crate::tenancy::Service::start_prepared(
        fixture.db.clone(),
        None,
        crate::tenancy::actor::OperationLimits::default(),
        crate::tenancy::store::load(&fixture.db, chrono::Utc::now()).await?,
        fixture.service.resources.clone(),
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn credential_read_wait_then_revoke_cannot_arm_and_no_send_receipt_drains() -> Result<()> {
    let harness = Harness::new(
        "chat_completions",
        StatusCode::OK,
        json_response("chat_completions", full_usage("chat_completions"))?,
    )
    .await?;
    let gate = harness
        .fixture
        .service
        .hold_receipt_credential_read_for_test()?;
    let release = gate.release.clone().drop_guard();
    let checked = async {
        let client = Harness::client(false)?;
        let request = harness.send(&client);
        let task = AbortOnDropHandle::new(tokio::spawn(async move { request.send().await }));
        tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        let id = gate
            .execution
            .get()
            .context("actual credential read did not report admission")?
            .clone();
        ensure!(harness.calls.load(Ordering::SeqCst) == 0 && !task.is_finished());
        harness
            .fixture
            .service
            .execute(
                harness.fixture.operator.clone(),
                revoke(&harness.fixture, &harness.credential)?,
            )
            .await
            .map_err(failure)?;
        gate.release.cancel();
        let response = tokio::time::timeout(TIMEOUT, task).await???;
        ensure!(response.status() == StatusCode::UNAUTHORIZED);
        ensure!(execution_id(&response)? == id);
        let _ = response.bytes().await?;
        drain(&harness.fixture).await?;
        let receipt = read_one(&harness.fixture, harness.fixture.operator.clone(), &id).await?;
        no_attempt(&receipt, &id)?;
        let page = page_as(&harness.fixture, harness.fixture.operator.clone()).await?;
        ensure!(page.execution_ids == vec![id] && !page.multiple_executions);
        ensure!(harness.calls.load(Ordering::SeqCst) == 0);
        Ok(())
    }
    .await;
    drop(release);
    finish(&harness, checked).await
}

#[tokio::test]
async fn revoke_after_actual_send_preserves_terminal_usage_without_the_old_grant() -> Result<()> {
    let gate = Arc::new(Semaphore::new(0));
    let mut harness = Harness::with_reply(
        "chat_completions",
        StatusCode::OK,
        serde_json::to_string(&json_response(
            "chat_completions",
            full_usage("chat_completions"),
        )?)?,
        false,
        Some(gate.clone()),
    )
    .await?;
    let checked = async {
        let caller = harness
            .fixture
            .service
            .authenticate(harness.credential.secret.as_deref().context("secret")?)
            .map_err(failure)?;
        let client = Harness::client(false)?;
        let request = harness.send(&client);
        let task = AbortOnDropHandle::new(tokio::spawn(async move { request.send().await }));
        let observed = tokio::time::timeout(TIMEOUT, harness.received.recv())
            .await?
            .context("authenticated provider POST")?;
        ensure!(observed["model"] == "model-a" && harness.calls.load(Ordering::SeqCst) == 1);
        ensure!(!task.is_finished());
        harness
            .fixture
            .service
            .execute(
                harness.fixture.operator.clone(),
                revoke(&harness.fixture, &harness.credential)?,
            )
            .await
            .map_err(failure)?;
        gate.add_permits(1);
        let response = tokio::time::timeout(TIMEOUT, task).await???;
        // Caller delivery may be denied after reauthorization; billing evidence must survive.
        let id = execution_id(&response)?;
        let _ = response.bytes().await?;
        drain(&harness.fixture).await?;
        let rejected = harness
            .fixture
            .service
            .read_receipt(
                caller,
                harness.fixture.tenant.clone(),
                read::Request::One(id.clone()),
            )
            .await;
        ensure!(matches!(rejected, Err(error) if error.0.code == ErrorCode::Unauthorized));
        let receipt = read_one(&harness.fixture, harness.fixture.operator.clone(), &id).await?;
        ensure!(receipt.state == ReceiptState::Committed && receipt.execution_id == id);
        ensure!(receipt.attempts.len() == 1 && receipt.attempts[0].outcome == Outcome::Completed);
        let attempt = &receipt.attempts[0];
        ensure!(
            attempt
                .target
                .as_ref()
                .is_some_and(|target| target.model_id == "model-a")
        );
        ensure!(attempt.usage.provenance == Provenance::Provider && attempt.usage.complete);
        ensure!(attempt.usage.input_tokens == Some(11) && attempt.usage.output_tokens == Some(7));
        ensure!(receipt.usage == attempt.usage);
        ensure!(harness.calls.load(Ordering::SeqCst) == 1 && harness.received.try_recv().is_err());
        let page = page_as(&harness.fixture, harness.fixture.operator.clone()).await?;
        ensure!(page.execution_ids == vec![id] && !page.multiple_executions);
        Ok(())
    }
    .await;
    gate.add_permits(1);
    finish(&harness, checked).await
}
