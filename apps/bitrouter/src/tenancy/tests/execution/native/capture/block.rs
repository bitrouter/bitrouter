//! Real ACP -> mounted HTTP -> retained Block -> strict receipt -> Human head.
use super::*;
use crate::{
    acp_trajectory::checkpoint::types::{AssessmentSource, CriterionScore, RevisionInput},
    core::evolution::{
        rubric::{self, Applicability, RubricEvaluation, RubricItem},
        scoring::RubricSubmission,
    },
    harness::acp::controlled::CheckpointReview,
};
use bitrouter_ai::tenancy::{
    authority::PrincipalPut,
    identity::{BindingId, PrincipalId},
};
mod adoption;
mod coexistence;
mod eligibility;
mod faults;
mod fixture;
mod pricing;
mod restart;
mod tenants;
use fixture::Host;

fn label(view: &CheckpointReview, id: &str) -> Result<RubricSubmission> {
    let citations = view
        .scoring
        .evidence
        .items
        .iter()
        .map(|item| item.citation.clone())
        .collect::<Vec<_>>();
    ensure!(
        !citations.is_empty(),
        "the actual captured prompt/reply must exist"
    );
    Ok(RubricSubmission {
        submission_id:id.into(),checkpoint_id:view.checkpoint.checkpoint_id.clone(),expected_revision:view.scoring.expected_revision.clone(),
        source:AssessmentSource::Human,evaluator_id:String::new(),evaluator_version:String::new(),
        evaluation:RubricEvaluation {rubric_version:rubric::RUBRIC_VERSION.into(),
            items:rubric::library().into_iter().map(|criterion|RubricItem {
                criterion_id:criterion.id.into(),applicability:if criterion.mandatory {Applicability::Applicable}else{Applicability::NotApplicable},
                selection_reason:"Explicit Human checks only the observed reply; no code or tools were requested".into(),
                score:if criterion.mandatory{CriterionScore::Scored{value_ppm:rubric::PPM}}else{CriterionScore::NotApplicable},
                evidence:citations.clone(),explanation:"Human compared the recorded request and actual response".into(),
            }).collect(),diagnostics:Vec::new(),severe_violation:false,violation_evidence:Vec::new(),summary:"Manual judgement of the real captured reply".into()},
    })
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn actual_block_socket_selection_keeps_one_owner_and_rebuilds_effective_human_head()
-> Result<()> {
    let mut host = Host::new().await?;
    let checked = std::panic::AssertUnwindSafe(async {
        let native = host.open().await?;
        ensure!(host.fixture.service.host_slots_for_test() == 0);
        let current = host.current()?;
        current
            .prompt(
                &native,
                "Reply with exactly native reply; do not run tools or modify files",
            )
            .await?;
        tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
        let requests = host
            .upstream
            .received_requests()
            .await
            .context("actual provider requests")?;
        ensure!(
            requests.len() == 1
                && !requests[0]
                    .headers
                    .contains_key(crate::tenancy::execution::BLOCK_LINK_HEADER)
        );
        ensure!(
            requests[0]
                .headers
                .get("authorization")
                .is_some_and(|value| value == "Bearer owned-block-secret")
        );
        let body: Value = serde_json::from_slice(&requests[0].body)?;
        let intents = host.records("owned_block_intent_v1").await?;
        let enrollments = host.records("owned_block_enrollment_v1").await?;
        ensure!(intents.len() == 1 && enrollments.len() == 1);
        let intent = &intents[0];
        let selected = intent["intent"]["selected_route"]
            .as_str()
            .context("committed selector")?;
        ensure!(matches!(selected, "mock:model-a" | "mock:model-b"));
        ensure!(body["model"] == selected.strip_prefix("mock:").context("known selector")?);
        ensure!(
            intent["native"] == native
                && intent["descriptor"]["principal"] == host.fixture.principal.as_str()
                && enrollments[0]["value"]["identity"]["native_session_id"] == native
        );
        let execution = intent["intent"]["request_id"]
            .as_str()
            .context("execution identity")?
            .try_into()?;
        let receipt = crate::tenancy::receipts::store::lookup(
            &host.fixture.db,
            &host.fixture.installation,
            &host.fixture.tenant,
            &execution,
        )
        .await?
        .context("actual strict receipt")?;
        ensure!(
            receipt.state == bitrouter_ai::tenancy::execution::ReceiptState::Committed
                && receipt.usage.complete
                && receipt.usage.input_tokens == Some(4)
                && receipt.usage.output_tokens == Some(2)
                && receipt.attempts.len() == 1
                && receipt.attempts[0]
                    .target
                    .as_ref()
                    .context("actual target")?
                    .model_id
                    == body["model"].as_str().context("body model")?
        );
        current
            .checkpoint(&native)
            .await?
            .context("actual stopped prefix")?;
        let view = current.review_checkpoint(&native, None).await?;
        let human = current
            .submit_assessment(&native, label(&view, "block-human")?)
            .await?;
        ensure!(human.quality.complete_score() == Some(1.0));
        current.checkpoint(&native).await?;
        let control = host.records("owned_block_control_v1").await?;
        ensure!(control.len() == 1);
        let observations = control[0]["report"]["observations"]["sessions"]
            .as_object()
            .context("actual Block observations")?;
        ensure!(observations.len() == 1);
        let observation = observations.values().next().context("actual family")?;
        ensure!(
            observation["quality"] == 1.0
                && observation["total_cost_micro_usd"] == 14
                && observation["latency_ms"].as_u64().is_some()
                && control[0]["state"]["blocks"]["same-block"]["status"] != "adopted"
        );
        let before_revision = observation["revision"].clone();
        current
            .retract_assessment(
                &native,
                RevisionInput {
                    submission_id: "block-retract".into(),
                    checkpoint_id: view.checkpoint.checkpoint_id,
                    expected_revision: Some(human.revision.revision_id),
                    source: AssessmentSource::Human,
                    evaluator_id: String::new(),
                    evaluator_version: String::new(),
                    assessment: None,
                    reason: "Human withdraws the original judgement".into(),
                },
            )
            .await?;
        current.checkpoint(&native).await?;
        let changed = host.records("owned_block_control_v1").await?;
        let observations = changed[0]["report"]["observations"]["sessions"]
            .as_object()
            .context("reconstructed observations")?;
        ensure!(observations.len() == 1);
        let after = observations.values().next().context("same family")?;
        ensure!(after["quality"].is_null() && after["revision"] != before_revision);
        ensure!(
            host.records("owned_block_enrollment_v1").await?.len() == 1
                && host.records("owned_block_intent_v1").await?.len() == 1
                && host
                    .upstream
                    .received_requests()
                    .await
                    .context("provider requests")?
                    .len()
                    == 1
                && host.fixture.service.current().is_ok()
                && host.fixture.service.host_slots_for_test() == 0
        );
        Ok::<_, anyhow::Error>(())
    })
    .catch_unwind()
    .await;
    let checked = match checked {
        Ok(value) => value,
        Err(_) => Err(anyhow::anyhow!("Block consumer observation unwound")),
    };
    host.finish(checked).await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn copied_headers_and_same_name_reopened_controller_cannot_reanimate_an_old_block_link()
-> Result<()> {
    let mut host = Host::new().await?;
    let checked = std::panic::AssertUnwindSafe(async {
        let native = host.open().await?;
        let original = host.endpoint_headers().await?;
        // A live link without an actual pending ACP prompt is ordinary refusal,
        // not database corruption and never an installation-wide fence.
        host.rejected(original.clone()).await?;
        ensure!(host.fixture.service.current().is_ok());
        let mut no_link = original.clone();
        no_link.remove(crate::tenancy::execution::BLOCK_LINK_HEADER);
        host.rejected(no_link).await?;
        let mut wrong = original.clone();
        wrong.insert(
            crate::tenancy::execution::BLOCK_LINK_HEADER,
            "0".repeat(64).parse()?,
        );
        host.rejected(wrong).await?;
        ensure!(
            host.upstream
                .received_requests()
                .await
                .context("provider requests")?
                .is_empty()
        );
        host.current()?
            .prompt(&native, "actual first controller turn")
            .await?;
        tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
        host.current()?.checkpoint(&native).await?;
        let old = host.records("owned_block_intent_v1").await?;
        ensure!(old.len() == 1);
        host.close().await?;
        // These are the exact originally issued credentials and link, after
        // known local teardown. The stale row remains in the same physical DB.
        host.rejected(original.clone()).await?;
        let reopened = host.open().await?;
        ensure!(
            reopened == native,
            "fixture must reuse the same visible native name"
        );
        let fresh = host.endpoint_headers().await?;
        ensure!(
            fresh.get(crate::tenancy::execution::BLOCK_LINK_HEADER)
                != original.get(crate::tenancy::execution::BLOCK_LINK_HEADER)
        );
        host.rejected(original).await?;
        host.current()?
            .prompt(&reopened, "actual second controller turn")
            .await?;
        tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
        let intents = host.records("owned_block_intent_v1").await?;
        ensure!(
            intents.len() == 2
                && intents[0]["session"] != intents[1]["session"]
                && intents[0]["descriptor"]["controller"] != intents[1]["descriptor"]["controller"]
        );
        ensure!(
            host.upstream
                .received_requests()
                .await
                .context("provider requests")?
                .len()
                == 2
                && host.fixture.service.current().is_ok()
        );
        Ok::<_, anyhow::Error>(())
    })
    .catch_unwind()
    .await;
    let checked = match checked {
        Ok(value) => value,
        Err(_) => Err(anyhow::anyhow!("Block link observation unwound")),
    };
    host.finish(checked).await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn known_block_intent_cannot_send_after_original_capture_close_or_key_revocation()
-> Result<()> {
    for revoke in [false, true] {
        let mut host = Host::new().await?;
        let mut held = None;
        let checked = std::panic::AssertUnwindSafe(async {
            let native = host.open().await?;
            let gate = host.fixture.service.hold_host(crate::tenancy::hosts::HostBoundary::BlockIntentAck)?;
            held = Some(gate.clone());
            let mut prompt = Box::pin(host.current()?.prompt(&native, "held committed Block selection"));
            tokio::time::timeout(TIMEOUT, async {
                tokio::select! {
                    result = &mut prompt => { result?; anyhow::bail!("Block intent never reached its acknowledged barrier") }
                    () = gate.entered.notified() => Ok::<_,anyhow::Error>(()),
                }
            }).await??;
            let intents = host.records("owned_block_intent_v1").await?;
            ensure!(intents.len() == 1 && host.upstream.received_requests().await.context("provider")?.is_empty());
            ensure!(host.fixture.service.host_slots_for_test() == 0);
            if revoke {
                result(host.fixture.service.execute(host.fixture.operator.clone(), Command::Revoke(
                    host.fixture.tenant.clone(), host.key.credential_id.clone(), Mutation {
                        operation_id: "block-revoke-before-send".try_into()?,
                        expected_revision: host.key.credential_revision.clone(),
                        installation_id: host.fixture.installation.clone(),
                    })).await.map_err(failure)?)?;
            } else { host.stop.cancel(); }
            gate.release();
            let returned = tokio::time::timeout(TIMEOUT, &mut prompt).await?;
            ensure!(returned.is_err(), "changed original source authorized a send");
            tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
            let id = intents[0]["intent"]["request_id"].as_str().context("actual execution")?.try_into()?;
            let receipt = crate::tenancy::receipts::store::lookup(&host.fixture.db, &host.fixture.installation,
                &host.fixture.tenant, &id).await?.context("actual pre-send receipt")?;
            ensure!(receipt.attempts.is_empty() && host.upstream.received_requests().await.context("provider")?.is_empty());
            ensure!(host.records("owned_block_intent_v1").await?.len() == 1 && host.fixture.service.current().is_ok());
            Ok::<_, anyhow::Error>(())
        }).catch_unwind().await;
        if let Some(gate) = held {
            gate.release();
        }
        let checked = match checked {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!("Block send-fence observation unwound")),
        };
        host.finish(checked).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn copied_block_nonce_does_not_authorize_a_second_key_or_another_principal() -> Result<()> {
    let mut host = Host::new().await?;
    let checked = std::panic::AssertUnwindSafe(async {
        let native = host.open().await?;
        let original = host.endpoint_headers().await?;
        let key = host.fixture.issue("different-key-same-principal").await?;
        let mut changed = original.clone();
        changed.insert(
            http::header::AUTHORIZATION,
            format!("Bearer {}", key.secret.as_deref().context("key")?).parse()?,
        );
        host.rejected(changed).await?;
        let principal: PrincipalId = "principal-b".try_into()?;
        let binding: BindingId = "binding-b".try_into()?;
        result(
            host.fixture
                .service
                .execute(
                    host.fixture.operator.clone(),
                    Command::Principal(
                        principal.clone(),
                        PrincipalPut {
                            operation_id: "block-other-principal".try_into()?,
                            expected_revision: None,
                            issuer: "test".into(),
                            subject: "other-subject".into(),
                            installation_id: host.fixture.installation.clone(),
                        },
                    ),
                )
                .await
                .map_err(failure)?,
        )?;
        result(
            host.fixture
                .service
                .execute(
                    host.fixture.operator.clone(),
                    Command::Binding(
                        host.fixture.tenant.clone(),
                        binding.clone(),
                        BindingPut {
                            operation_id: "block-other-binding".try_into()?,
                            expected_revision: None,
                            principal_id: principal.clone(),
                            role: TenantRole::TenantAdmin,
                            scope: crate::tenancy::mutation::tenant_scope(),
                            expires_at: expiry()?,
                            expected_management_owner: None,
                            installation_id: host.fixture.installation.clone(),
                        },
                    ),
                )
                .await
                .map_err(failure)?,
        )?;
        let mut input = host.fixture.issuance("block-other-key")?;
        input.principal_id = principal;
        input.binding_id = binding;
        let key = crate::tenancy::tests::issued(
            host.fixture
                .service
                .execute(
                    host.fixture.operator.clone(),
                    Command::Issue(host.fixture.tenant.clone(), input),
                )
                .await
                .map_err(failure)?,
        )?;
        let mut changed = original;
        changed.insert(
            http::header::AUTHORIZATION,
            format!("Bearer {}", key.secret.as_deref().context("key")?).parse()?,
        );
        host.rejected(changed).await?;
        ensure!(
            host.records("owned_block_intent_v1").await?.is_empty()
                && host
                    .upstream
                    .received_requests()
                    .await
                    .context("provider")?
                    .is_empty()
        );
        // Actual original credential/capture still works after both rejected transplants.
        host.current()?
            .prompt(&native, "unchanged original source")
            .await?;
        tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
        ensure!(
            host.records("owned_block_intent_v1").await?.len() == 1
                && host
                    .upstream
                    .received_requests()
                    .await
                    .context("provider")?
                    .len()
                    == 1
        );
        Ok::<_, anyhow::Error>(())
    })
    .catch_unwind()
    .await;
    let checked = match checked {
        Ok(value) => value,
        Err(_) => Err(anyhow::anyhow!("Block transplant observation unwound")),
    };
    host.finish(checked).await
}
