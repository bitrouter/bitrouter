//! Real non-hit and earlier-work boundaries retain truthful baseline intents.
use super::*;

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn actual_nonmatching_rule_sends_baseline_but_never_creates_a_conclusive_trial_score()
-> Result<()> {
    let mut host = Host::non_hit().await?;
    let checked = std::panic::AssertUnwindSafe(async {
        let native = host.open().await?;
        host.current()?
            .prompt(&native, "real prompt with a different fingerprint")
            .await?;
        tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
        let rows = host.records("owned_block_intent_v1").await?;
        ensure!(
            rows.len() == 1
                && rows[0]["intent"]["selected_route"] == "mock:model-a"
                && rows[0]["intent"]["applied_block"].is_null()
                && rows[0]["intent"]["reason"] == "baseline_passthrough"
        );
        let requests = host
            .upstream
            .received_requests()
            .await
            .context("provider")?;
        ensure!(
            requests.len() == 1
                && serde_json::from_slice::<Value>(&requests[0].body)?["model"] == "model-a"
        );
        host.current()?.checkpoint(&native).await?;
        let view = host.current()?.review_checkpoint(&native, None).await?;
        host.current()?
            .submit_assessment(&native, label(&view, "real-nonhit-human")?)
            .await?;
        host.current()?.checkpoint(&native).await?;
        let control = host.records("owned_block_control_v1").await?;
        ensure!(control.len() == 1);
        let observations = control[0]["report"]["observations"]["sessions"]
            .as_object()
            .context("cohort")?;
        ensure!(
            observations.len() == 1
                && observations
                    .values()
                    .all(|value| value["quality"].is_null())
        );
        ensure!(
            control[0]["report"]["unavailable"]
                .to_string()
                .contains("block_rule_not_observed")
        );
        ensure!(
            control[0]["state"]["blocks"]["same-block"]["status"] != "adopted"
                && host.fixture.service.current().is_ok()
        );
        Ok::<_, anyhow::Error>(())
    })
    .catch_unwind()
    .await;
    host.finish(match checked {
        Ok(value) => value,
        Err(_) => Err(anyhow::anyhow!("nonhit observation unwound")),
    })
    .await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn real_prior_local_turn_cannot_be_enrolled_as_a_fresh_provider_trial() -> Result<()> {
    let mut host = Host::new().await?;
    let checked = std::panic::AssertUnwindSafe(async {
        let native = host.open_local_first().await?;
        host.current()?
            .prompt(&native, "actual local-only first turn")
            .await?;
        ensure!(
            host.upstream
                .received_requests()
                .await
                .context("provider")?
                .is_empty()
        );
        ensure!(
            host.records("owned_block_enrollment_v1").await?.is_empty()
                && host.records("owned_block_intent_v1").await?.is_empty()
        );
        host.current()?
            .prompt(&native, "actual first provider call after earlier work")
            .await?;
        tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
        let intents = host.records("owned_block_intent_v1").await?;
        let enrollment = host.records("owned_block_enrollment_v1").await?;
        ensure!(
            intents.len() == 1
                && enrollment.len() == 1
                && enrollment[0]["value"]["admission_reason"] == "session_already_started"
                && enrollment[0]["value"]["assignments"]
                    .as_object()
                    .context("assignments")?
                    .is_empty()
                && intents[0]["intent"]["assignments"]
                    .as_object()
                    .context("intent assignments")?
                    .is_empty()
                && intents[0]["intent"]["selected_route"] == "mock:model-a"
        );
        let requests = host
            .upstream
            .received_requests()
            .await
            .context("actual provider")?;
        ensure!(
            requests.len() == 1
                && serde_json::from_slice::<Value>(&requests[0].body)?["model"] == "model-a"
                && host.fixture.service.current().is_ok()
        );
        Ok::<_, anyhow::Error>(())
    })
    .catch_unwind()
    .await;
    host.finish(match checked {
        Ok(value) => value,
        Err(_) => Err(anyhow::anyhow!("preexisting observation unwound")),
    })
    .await
}
