//! Publication races exercise the real authority owner and loopback provider.
use super::*;
use bitrouter_ai::tenancy::{
    admission::AdmissionAttachmentV1,
    authority::{OwnerKind, ResourceKey, ResourceKind},
    configuration::ResourceRef,
    ordered::GenerationProtocol,
    resources::{Definition, ResourcePut},
};

async fn publish(harness: &Harness, input: ResourcePut) -> Result<ResourceRef> {
    let key = input.key.clone();
    let response = harness
        .fixture
        .service
        .execute(
            harness.fixture.operator.clone(),
            Command::Resource(harness.fixture.tenant.clone(), input),
        )
        .await
        .map_err(failure)?;
    let StoredResponse::Operation(result) = response else {
        anyhow::bail!("resource publication did not apply");
    };
    Ok(ResourceRef {
        owner_kind: key.owner_kind,
        owner_id: key.owner_id,
        resource_kind: key.resource_kind,
        resource_id: key.resource_id,
        revision_id: result.resource_revision.context("published revision")?,
    })
}

async fn protect(harness: &Harness) -> Result<()> {
    let profile = publish(
        harness,
        ResourcePut {
            format: 1,
            operation_id: "protect-profile".try_into()?,
            installation_id: harness.fixture.installation.clone(),
            key: ResourceKey {
                owner_kind: OwnerKind::OperatorPool,
                owner_id: "pool".try_into()?,
                resource_kind: ResourceKind::Policy,
                resource_id: "profile".try_into()?,
            },
            expected_revision: None,
            definition: Definition::AdmissionProfile {
                profile: serde_json::from_value(
                    json!({"schema_version":1,"rate":null,"health":null,"fallback":"stop"}),
                )?,
            },
        },
    )
    .await?;
    // The old immutable imported route has no managed head. A successful new
    // revision protects the whole key, including that already-cached revision.
    publish(harness, ResourcePut {
        format: 1,
        operation_id: "protect-route".try_into()?,
        installation_id: harness.fixture.installation.clone(),
        key: harness.route.key(),
        expected_revision: None,
        definition: Definition::ProfiledRoute {
            routing: json!({"providers":{"mock":{"api_base":"http://127.0.0.1:1/v1",
                "api_protocol":[{"*":["chat_completions","messages","responses"]}],"models":[{"id":"model-a"}]}}}),
            selector: "mock:model-a".into(),
            policy: None,
            credentials: vec![bitrouter_ai::tenancy::resources::CredentialBinding {
                provider: "mock".into(), account: "default".into(), credential: harness.slot.clone(),
            }],
            attachments: [GenerationProtocol::ChatCompletions,GenerationProtocol::Messages,GenerationProtocol::Responses]
                .into_iter().map(|inbound_protocol| AdmissionAttachmentV1 {
                    inbound_protocol,candidate_index:0,credential:harness.slot.clone(),profile:profile.clone(),
                }).collect(),
        },
    }).await?;
    ensure!(
        harness
            .fixture
            .service
            .current()
            .map_err(failure)?
            .resource_admission
            .protected(&harness.route)
    );
    Ok(())
}

#[tokio::test]
async fn cached_legacy_configuration_is_denied_after_profile_publication_but_receipt_stays_readable()
-> Result<()> {
    for protocol in ["chat_completions", "messages", "responses"] {
        let harness = Harness::new(
            protocol,
            StatusCode::OK,
            json_response(protocol, full_usage(protocol))?,
        )
        .await?;
        let checked = async {
            let client = Harness::client(false)?;
            let response = harness.send(&client).send().await?;
            ensure!(response.status() == StatusCode::OK);
            let id = execution_id(&response)?;
            let _ = response.bytes().await?;
            drain(&harness.fixture).await?;
            let before = serde_json::to_vec(&harness.receipt(&client, &id).await?)?;
            ensure!(harness.calls.load(Ordering::SeqCst) == 1);
            protect(&harness).await?;
            let response = harness.send(&client).send().await?;
            ensure!(response.status() == StatusCode::CONFLICT);
            ensure!(response.headers().get("x-bitrouter-request-id").is_none());
            let body: Value = response.json().await?;
            ensure!(body["code"] == "capability_mismatch");
            ensure!(harness.calls.load(Ordering::SeqCst) == 1);
            ensure!(serde_json::to_vec(&harness.receipt(&client, &id).await?)? == before);
            let page = page_as(&harness.fixture, harness.fixture.operator.clone()).await?;
            ensure!(page.execution_ids == vec![id]);
            Ok(())
        }
        .await;
        finish(&harness, checked).await?;
    }
    Ok(())
}

#[tokio::test]
async fn queued_legacy_request_cannot_publish_ownership_after_protection_acknowledgement()
-> Result<()> {
    let harness = Harness::new(
        "chat_completions",
        StatusCode::OK,
        json_response("chat_completions", full_usage("chat_completions"))?,
    )
    .await?;
    let checked = async {
        let mut input = harness.fixture.configuration_input("protection-queue")?;
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
        tokio::time::timeout(TIMEOUT, async {
            while harness.fixture.service.admission.queued_for_test() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .context("request never reached capacity queue")?;
        ensure!(!queued.is_finished() && harness.calls.load(Ordering::SeqCst) == 0);
        protect(&harness).await?;
        drop(held);
        let response = tokio::time::timeout(TIMEOUT, queued).await???;
        ensure!(response.status() == StatusCode::CONFLICT);
        ensure!(response.headers().get("x-bitrouter-request-id").is_none());
        let body: Value = response.json().await?;
        ensure!(body["code"] == "capability_mismatch");
        drain(&harness.fixture).await?;
        no_attempt(
            &read_one(&harness.fixture, harness.fixture.operator.clone(), &id).await?,
            &id,
        )?;
        ensure!(
            page_as(&harness.fixture, harness.fixture.operator.clone())
                .await?
                .execution_ids
                == vec![id]
        );
        ensure!(harness.calls.load(Ordering::SeqCst) == 0);
        Ok(())
    }
    .await;
    finish(&harness, checked).await
}

#[tokio::test]
async fn profile_publication_during_credential_read_prevents_actual_arm_in_h1_and_h2() -> Result<()>
{
    let modes: &[bool] = if cfg!(feature = "http2") {
        &[false, true]
    } else {
        &[false]
    };
    for &h2 in modes {
        for protocol in ["chat_completions", "messages", "responses"] {
            let harness = Harness::new(
                protocol,
                StatusCode::OK,
                json_response(protocol, full_usage(protocol))?,
            )
            .await?;
            let gate = harness
                .fixture
                .service
                .hold_receipt_credential_read_for_test()?;
            let release = gate.release.clone().drop_guard();
            let checked = async {
                let client = Harness::client(h2)?;
                let request = harness.send(&client);
                let caller =
                    AbortOnDropHandle::new(tokio::spawn(async move { request.send().await }));
                tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
                let id = gate
                    .execution
                    .get()
                    .context("credential boundary execution")?
                    .clone();
                ensure!(harness.calls.load(Ordering::SeqCst) == 0 && !caller.is_finished());
                protect(&harness).await?;
                gate.release.cancel();
                let response = tokio::time::timeout(TIMEOUT, caller).await???;
                ensure!(response.status() == StatusCode::CONFLICT);
                ensure!(execution_id(&response)? == id);
                let _ = response.bytes().await?;
                drain(&harness.fixture).await?;
                no_attempt(
                    &read_one(&harness.fixture, harness.fixture.operator.clone(), &id).await?,
                    &id,
                )?;
                ensure!(harness.calls.load(Ordering::SeqCst) == 0);
                Ok(())
            }
            .await;
            drop(release);
            finish(&harness, checked).await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn protection_after_real_send_preserves_json_and_sse_terminal_obligation() -> Result<()> {
    for stream in [false, true] {
        let protocol = "chat_completions";
        let body = if stream {
            super::super::wire::chat_stream(full_usage(protocol), true)?
        } else {
            serde_json::to_string(&json_response(protocol, full_usage(protocol))?)?
        };
        let gate = Arc::new(Semaphore::new(0));
        let mut harness =
            Harness::with_reply(protocol, StatusCode::OK, body, stream, Some(gate.clone())).await?;
        let checked = async {
            let client = Harness::client(false)?;
            let request = harness.send(&client);
            let caller = AbortOnDropHandle::new(tokio::spawn(async move { request.send().await }));
            tokio::time::timeout(TIMEOUT, harness.received.recv())
                .await?
                .context("actual provider request")?;
            ensure!(harness.calls.load(Ordering::SeqCst) == 1);
            protect(&harness).await?;
            gate.add_permits(1);
            let response = tokio::time::timeout(TIMEOUT, caller).await???;
            ensure!(response.status() == StatusCode::OK);
            let id = execution_id(&response)?;
            let _ = response.bytes().await?;
            drain(&harness.fixture).await?;
            let receipt = harness.receipt(&client, &id).await?;
            ensure!(receipt.outcome == Outcome::Completed && receipt.attempts.len() == 1);
            ensure!(receipt.attempts[0].outcome == Outcome::Completed);
            ensure!(harness.calls.load(Ordering::SeqCst) == 1);
            Ok(())
        }
        .await;
        gate.add_permits(1);
        finish(&harness, checked).await?;
    }
    Ok(())
}
