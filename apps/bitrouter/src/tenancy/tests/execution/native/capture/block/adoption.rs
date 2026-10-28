//! Actual independent families, original Human head replacement, same H publication.
use super::*;
use crate::gateway::routing::blocks::allocation::{
    Arm, ArmPrior, BanditConfig, MeanPrior, draw_assignment,
};
use fixture::Options;

fn declared_prior(quality: f64, cost: f64, latency: f64) -> ArmPrior {
    let prior = |mean| MeanPrior {
        mean,
        strength: 1.0e12,
        observation_variance: 1.0e-6,
    };
    ArmPrior {
        quality: prior(quality),
        log_cost: prior(cost.ln_1p()),
        log_latency: prior(latency.ln_1p()),
    }
}
fn bandit() -> BanditConfig {
    // Explicit operator-declared test priors make the numerical plan stable.
    // They do not count as observations: both arms below must still acquire
    // one actual independent family, complete035 cost/latency and Human score.
    BanditConfig {
        prior_version: "explicit-block-publication-fixture-v1".into(),
        baseline: declared_prior(1.0, 14_000.0, 60_000.0),
        challenger: declared_prior(1.0, 14.0, 1.0),
        minimum_families_per_arm: 1,
        promotion_probability_ppm: 800_000,
        initial_exposure_ppm: 500_000,
        maximum_exposure_ppm: 500_000,
        maximum_challenger_sessions: 8,
        maximum_pending_challenger: 8,
        posterior_draws: 128,
        ..Default::default()
    }
}
fn seed(arm: Arm) -> Result<u64> {
    for seed in 0..1024 {
        if draw_assignment(500_000, seed)? == arm {
            return Ok(seed);
        }
    }
    anyhow::bail!("bounded genuine random input for arm absent")
}
async fn control(host: &Host) -> Result<Value> {
    let mut values = host.records("owned_block_control_v1").await?;
    ensure!(values.len() == 1);
    values.pop().context("actual control")
}
async fn family(host: &Host, native: &str, id: &str, model: &str) -> Result<()> {
    let current = host.current()?;
    current
        .prompt(native, "Reply with exactly native reply; no tools")
        .await?;
    tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
    let requests = host
        .upstream
        .received_requests()
        .await
        .context("real provider")?;
    let body: Value = serde_json::from_slice(&requests.last().context("real family reply")?.body)?;
    ensure!(body["model"] == model);
    current
        .checkpoint(native)
        .await?
        .context("real stopped prefix")?;
    let view = current.review_checkpoint(native, None).await?;
    current.submit_assessment(native, label(&view, id)?).await?;
    current.checkpoint(native).await?;
    Ok(())
}
async fn exercise(severe: bool) -> Result<()> {
    let mut host = Host::configured(Options {
        bandit: Some(bandit()),
        pricing: Some([
            json!({"input_micro_usd_per_token":2000.0,"output_micro_usd_per_token":3000.0}),
            json!({"input_micro_usd_per_token":2.0,"output_micro_usd_per_token":3.0}),
        ]),
        ..Default::default()
    })
    .await?;
    let checked = std::panic::AssertUnwindSafe(async {
        host.fixture
            .service
            .block_draw_seeds_for_test([seed(Arm::Baseline)?, seed(Arm::Challenger)?])?;
        let baseline = host.open().await?;
        family(&host, &baseline, "first-baseline-human", "model-a").await?;
        let current = host.current()?;
        let challenger = current
            .new_session(host.directory.path().to_owned())
            .await?;
        ensure!(baseline != challenger);
        family(&host, &challenger, "original-challenger-human", "model-b").await?;
        let adopted = control(&host).await?;
        let block = &adopted["state"]["blocks"]["same-block"];
        ensure!(
            block["status"] == "adopted",
            "actual learner did not adopt: {}",
            adopted["report"]
        );
        ensure!(
            adopted["report"]["observations"]["sessions"]
                .as_object()
                .context("actual trials")?
                .len()
                == 2
                && adopted["report"]["monitoring_observations"]["sessions"]
                    .as_object()
                    .context("no later families")?
                    .is_empty()
        );
        for arm in ["baseline", "challenger"] {
            for metric in ["quality", "log_cost", "log_latency"] {
                ensure!(
                    adopted["report"]["plan"][arm][metric]["observed_families"] == 1,
                    "operator priors substituted for an actual family"
                );
            }
        }
        ensure!(
            adopted["state"]["publications"]
                .as_array()
                .context("publications")?
                .last()
                .context("actual promote")?["action"]
                == "promote"
        );
        let before_generation = adopted["state"]["generation"]
            .as_u64()
            .context("generation")?;
        let before_intents = host.records("owned_block_intent_v1").await?;
        let view = current.review_checkpoint(&challenger, None).await?;
        if severe {
            let mut correction = label(&view, "actual-severe-correction")?;
            correction.evaluation.severe_violation = true;
            correction.evaluation.violation_evidence = view
                .scoring
                .evidence
                .items
                .iter()
                .map(|i| i.citation.clone())
                .collect();
            correction.evaluation.summary =
                "Human corrects the original recorded reply as a severe violation".into();
            current.submit_assessment(&challenger, correction).await?;
        } else {
            current
                .retract_assessment(
                    &challenger,
                    RevisionInput {
                        submission_id: "actual-original-retraction".into(),
                        checkpoint_id: view.checkpoint.checkpoint_id,
                        expected_revision: view.scoring.expected_revision,
                        source: AssessmentSource::Human,
                        evaluator_id: String::new(),
                        evaluator_version: String::new(),
                        assessment: None,
                        reason: "Human withdraws the original trial evidence".into(),
                    },
                )
                .await?;
        }
        current.checkpoint(&challenger).await?;
        let withdrawn = control(&host).await?;
        ensure!(
            withdrawn["state"]["blocks"]["same-block"]["status"] == "rolled_back"
                && withdrawn["state"]["generation"]
                    .as_u64()
                    .context("new generation")?
                    > before_generation
        );
        ensure!(
            withdrawn["state"]["publications"]
                .as_array()
                .context("publications")?
                .last()
                .context("actual withdrawal")?["action"]
                == if severe {
                    "rollback"
                } else {
                    "withdraw_unsupported_promotion"
                }
        );
        ensure!(
            host.records("owned_block_intent_v1").await? == before_intents
                && host
                    .upstream
                    .received_requests()
                    .await
                    .context("provider requests")?
                    .len()
                    == 2,
            "retraction must not redraw or replay an earlier family"
        );
        // The actual subsequent public session crosses HTTP admission and the
        // same generation/send fence, after durable withdrawal, and uses baseline.
        let fresh = current
            .new_session(host.directory.path().to_owned())
            .await?;
        current
            .prompt(&fresh, "actual new family after withdrawal")
            .await?;
        tokio::time::timeout(TIMEOUT, host.fixture.service.admission.wait_drained()).await?;
        let requests = host
            .upstream
            .received_requests()
            .await
            .context("provider requests")?;
        ensure!(requests.len() == 3);
        let body: Value = serde_json::from_slice(&requests[2].body)?;
        ensure!(body["model"] == "model-a");
        let intents = host.records("owned_block_intent_v1").await?;
        let new = intents
            .iter()
            .find(|value| value["native"] == fresh)
            .context("actual new intent")?;
        ensure!(
            intents.len() == 3
                && new["intent"]["selected_route"] == "mock:model-a"
                && new["intent"]["reason"] == "block_rolled_back"
        );
        ensure!(host.fixture.service.current().is_ok());
        Ok(())
    })
    .catch_unwind()
    .await;
    host.finish(match checked {
        Ok(value) => value,
        Err(_) => Err(anyhow::anyhow!("actual adoption regression unwound")),
    })
    .await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn actual_adoption_is_withdrawn_after_original_public_human_retraction() -> Result<()> {
    exercise(false).await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn actual_adoption_is_rolled_back_after_original_public_human_severe_correction() -> Result<()>
{
    exercise(true).await
}
