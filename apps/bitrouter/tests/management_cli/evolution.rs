//! Real CLI adapters over the same checkpoint and publication owners as the UI.

use super::{Fixture, Result, Value, field, idle, json, text_path};
use anyhow::{Context, ensure};
use bitrouter::acp_trajectory::checkpoint::types::{AssessmentSource, CriterionScore};
use bitrouter::core::evolution::evidence::EvidenceKind;
use bitrouter::core::evolution::operator::checkpoint::ReviewInput;
use bitrouter::core::evolution::rubric::{self, Applicability, RubricEvaluation, RubricItem};
use bitrouter::core::evolution::scoring::RubricSubmission;
use wiremock::MockServer;

async fn operate(fixture: &Fixture, action: &[&str]) -> Result<Value> {
    let mut args = vec!["acp", "evolution"];
    args.extend_from_slice(action);
    fixture.run(&args).await
}

async fn checkpoint(fixture: &Fixture, session: &str, action: &[&str]) -> Result<Value> {
    let mut args = vec!["checkpoint", "--source", "stub", "--session", session];
    args.extend_from_slice(action);
    operate(fixture, &args).await
}

#[tokio::test]
async fn candidate_preview_registration_and_baseline_restore_use_exact_revisions() -> Result<()> {
    let upstream = MockServer::start().await;
    let fixture = Fixture::new(Some("minimal"), Some(&upstream.uri())).await?;
    let catalog = operate(&fixture, &["candidate", "catalog"]).await?;
    ensure!(
        catalog["report"]["report"]["routes"]
            .as_array()
            .context("catalog routes")?
            .iter()
            .any(|route| route["selector"] == "coding")
    );
    let spec = fixture.prompt("candidate.json", &serde_json::to_string(&json!({
        "block_id":"cli-trial","source":"stub","rules":[{"selector":"coding","fingerprint":null,"baseline_route":"coding","challenger_route":"candidate"}],
        "rationale":"Controlled CLI withdrawal fixture","independence_rationale":"One isolated fixture block","feedback":{"kind":"manual"}
    }))?)?;
    let preview = operate(
        &fixture,
        &["candidate", "preview", "--from", text_path(&spec)?],
    )
    .await?;
    let original = fixture.prompt(
        "reviewed-preview.json",
        &serde_json::to_string(&preview["report"]["report"])?,
    )?;
    // Review generation is a fence, not a suggestion to silently rebase.
    operate(&fixture, &["mode", "manual"]).await?;
    fixture
        .rejected(&[
            "acp",
            "evolution",
            "candidate",
            "register",
            "--from",
            text_path(&original)?,
        ])
        .await?;
    assert!(
        operate(&fixture, &["status"]).await?["report"]["control"]["blocks"]
            .as_object()
            .context("blocks")?
            .is_empty()
    );
    let preview = operate(
        &fixture,
        &["candidate", "preview", "--from", text_path(&spec)?],
    )
    .await?;
    let reviewed = fixture.prompt(
        "current-preview.json",
        &serde_json::to_string(&preview["report"]["report"])?,
    )?;
    let mut tampered = preview["report"]["report"].clone();
    tampered["definition"]["rationale"] = json!("not reviewed");
    let tampered = fixture.prompt("tampered-preview.json", &serde_json::to_string(&tampered)?)?;
    fixture
        .rejected(&[
            "acp",
            "evolution",
            "candidate",
            "register",
            "--from",
            text_path(&tampered)?,
        ])
        .await?;
    operate(
        &fixture,
        &["candidate", "register", "--from", text_path(&reviewed)?],
    )
    .await?;
    let state = operate(&fixture, &["status"]).await?;
    let block = &state["report"]["control"]["blocks"]["cli-trial"];
    let experiment = field(block, "/experiment_id")?.to_owned();
    let revision = field(block, "/revision")?.to_owned();
    assert_eq!(block["status"], "exploring");
    let revision_spec = operate(&fixture, &["candidate", "revision", "cli-trial"]).await?;
    assert_eq!(revision_spec["report"]["report"]["predecessor"], experiment);
    let learning = operate(
        &fixture,
        &["learning", "cli-trial", "--experiment", &experiment],
    )
    .await?;
    ensure!(
        learning["report"]["publications"]
            .as_array()
            .context("publications")?
            .is_empty()
    );
    fixture
        .rejected(&[
            "acp",
            "evolution",
            "restore",
            "cli-trial",
            "--expected-experiment",
            &experiment,
            "--expected-revision",
            "stale",
            "--reason",
            "reviewed withdrawal",
        ])
        .await?;
    assert_eq!(
        operate(&fixture, &["status"]).await?["report"]["control"]["blocks"]["cli-trial"]["status"],
        "exploring"
    );
    operate(
        &fixture,
        &[
            "restore",
            "cli-trial",
            "--expected-experiment",
            &experiment,
            "--expected-revision",
            &revision,
            "--reason",
            "Return to the baseline after my CLI review",
        ],
    )
    .await?;
    let restored = operate(&fixture, &["status"]).await?;
    assert_eq!(
        restored["report"]["control"]["blocks"]["cli-trial"]["status"],
        "rolled_back"
    );
    assert_eq!(restored["report"]["control"]["mode"], "manual");
    let publication = restored["report"]["control"]["publications"]
        .as_array()
        .context("publications")?
        .last()
        .context("restore publication")?;
    assert_eq!(publication["action"], "operator_restore");
    assert_eq!(
        publication["operator_reason"],
        "Return to the baseline after my CLI review"
    );
    assert!(publication["evidence_digest"].is_null());
    // Setting a judge must not silently enable automatic publication.
    operate(&fixture, &["judge-model", "mock:model"]).await?;
    assert_eq!(
        operate(&fixture, &["status"]).await?["report"]["control"]["mode"],
        "manual"
    );
    operate(&fixture, &["mode", "off"]).await?;
    Ok(())
}

fn submission(input: &ReviewInput, name: &str, score: u32) -> Result<RubricSubmission> {
    let citation = input
        .evidence
        .items
        .iter()
        .find(|item| item.kind == EvidenceKind::AgentMessage)
        .context("captured agent message")?
        .citation
        .clone();
    Ok(RubricSubmission {
        submission_id: name.into(),
        checkpoint_id: input.checkpoint.checkpoint_id.clone(),
        expected_revision: input.expected_revision.clone(),
        source: AssessmentSource::Human,
        evaluator_id: "cli-fixture-reviewer".into(),
        evaluator_version: "fixture-v1".into(),
        evaluation: RubricEvaluation {
            rubric_version: rubric::RUBRIC_VERSION.into(),
            items: rubric::library()
                .iter()
                .map(|criterion| RubricItem {
                    criterion_id: criterion.id.into(),
                    applicability: Applicability::Applicable,
                    selection_reason: "Controlled history annotation".into(),
                    score: if criterion.id == "delivery" {
                        CriterionScore::Scored { value_ppm: score }
                    } else {
                        CriterionScore::Unknown
                    },
                    evidence: if criterion.id == "delivery" {
                        vec![citation.clone()]
                    } else {
                        Vec::new()
                    },
                    explanation: "Fixed fixture label, not actual task quality".into(),
                })
                .collect(),
            diagnostics: Vec::new(),
            severe_violation: false,
            violation_evidence: Vec::new(),
            summary: name.into(),
        },
    })
}

#[tokio::test]
async fn recorded_checkpoint_cli_review_preserves_history_idempotency_and_current_reward()
-> Result<()> {
    let fixture = Fixture::new(Some("minimal"), None).await?;
    let run = fixture
        .background("Record the first controlled CLI turn", &["--direct"])
        .await?;
    let ready = idle(&fixture, &run).await?;
    let session = field(&ready, "/payload/native_session_id")?.to_owned();
    let list = checkpoint(&fixture, &session, &["list"]).await?;
    let watermark = list["report"]["data"]["effective"]["current_watermark"]
        .as_i64()
        .context("watermark")?
        .to_string();
    let frozen = checkpoint(
        &fixture,
        &session,
        &["freeze", "--expected-watermark", &watermark],
    )
    .await?;
    let input: ReviewInput = serde_json::from_value(frozen["report"]["data"].clone())?;
    ensure!(!input.evidence.items.is_empty() && input.history.is_empty());
    let first = submission(&input, "CLI initial annotation", 1_000_000)?;
    let bounds = first.evaluation.aggregate(&input.evidence)?;
    ensure!(
        bounds.lower_ppm < bounds.upper_ppm && bounds.complete_score().is_none(),
        "unknown criteria became a complete quality score"
    );
    let mut fabricated = first.clone();
    fabricated.submission_id = "invalid verification evidence".into();
    let citation = input
        .evidence
        .items
        .iter()
        .find(|item| item.kind == EvidenceKind::AgentMessage)
        .context("agent claim evidence")?
        .citation
        .clone();
    fabricated.evaluation.items[2].score = CriterionScore::Scored {
        value_ppm: 1_000_000,
    };
    fabricated.evaluation.items[2].evidence = vec![citation];
    let invalid = fixture.prompt(
        "invalid-verification.json",
        &serde_json::to_string(&fabricated)?,
    )?;
    fixture
        .rejected(&[
            "acp",
            "evolution",
            "checkpoint",
            "--source",
            "stub",
            "--session",
            &session,
            "submit",
            "--from",
            text_path(&invalid)?,
        ])
        .await?;
    let unchanged = checkpoint(
        &fixture,
        &session,
        &["review", &input.checkpoint.checkpoint_id],
    )
    .await?;
    ensure!(
        unchanged["report"]["data"]["history"]
            .as_array()
            .context("unchanged history")?
            .is_empty(),
        "invalid evidence created a revision"
    );
    let first_file = fixture.prompt("first-score.json", &serde_json::to_string(&first)?)?;
    let accepted = checkpoint(
        &fixture,
        &session,
        &["submit", "--from", text_path(&first_file)?],
    )
    .await?;
    let replay = checkpoint(
        &fixture,
        &session,
        &["submit", "--from", text_path(&first_file)?],
    )
    .await?;
    assert_eq!(
        accepted["report"]["data"]["receipt"]["revision"],
        replay["report"]["data"]["receipt"]["revision"]
    );
    let checkpoint_id = input.checkpoint.checkpoint_id.clone();
    let review = checkpoint(&fixture, &session, &["review", &checkpoint_id]).await?;
    let reviewed: ReviewInput = serde_json::from_value(review["report"]["data"].clone())?;
    assert_eq!(reviewed.history.len(), 1);
    let correction = submission(&reviewed, "CLI correction", 500_000)?;
    let correction_file =
        fixture.prompt("correction.json", &serde_json::to_string(&correction)?)?;
    checkpoint(
        &fixture,
        &session,
        &["submit", "--from", text_path(&correction_file)?],
    )
    .await?;
    let mut stale = first;
    stale.submission_id = "fresh id with stale revision".into();
    let stale_file = fixture.prompt("stale-score.json", &serde_json::to_string(&stale)?)?;
    fixture
        .rejected(&[
            "acp",
            "evolution",
            "checkpoint",
            "--source",
            "stub",
            "--session",
            &session,
            "submit",
            "--from",
            text_path(&stale_file)?,
        ])
        .await?;
    let before = checkpoint(&fixture, &session, &["list"]).await?;
    let reviewed = checkpoint(&fixture, &session, &["review", &checkpoint_id]).await?;
    let reviewed: ReviewInput = serde_json::from_value(reviewed["report"]["data"].clone())?;
    assert_eq!(reviewed.history.len(), 2);
    assert_eq!(
        reviewed.previous.context("review prefill")?.summary,
        "CLI correction"
    );
    let after = checkpoint(&fixture, &session, &["list"]).await?;
    assert_eq!(
        before["report"]["data"]["effective"], after["report"]["data"]["effective"],
        "reading history changed the current reward"
    );
    let prompt = fixture.prompt("next-turn.txt", "Make the old reviewed prefix stale")?;
    fixture
        .run(&[
            "agents",
            "prompt",
            &run,
            "--prompt-file",
            text_path(&prompt)?,
        ])
        .await?;
    idle(&fixture, &run).await?;
    let current = checkpoint(&fixture, &session, &["list"]).await?;
    assert_eq!(current["report"]["data"]["effective"]["stale"], true);
    let old = checkpoint(&fixture, &session, &["review", &checkpoint_id]).await?;
    assert_eq!(
        old["report"]["data"]["history"]
            .as_array()
            .context("history")?
            .len(),
        2
    );
    Ok(())
}

#[tokio::test]
async fn candidate_cli_preserves_joint_rules_judge_fences_balanced_accounts_and_recovery()
-> Result<()> {
    let upstream = MockServer::start().await;
    let mut fixture = Fixture::new(Some("minimal"), Some(&upstream.uri())).await?;
    fixture.stop(false).await?;
    let mut config: Value = serde_json::from_slice(&std::fs::read(&fixture.config)?)?;
    config["models"]["review"] = config["models"]["coding"].clone();
    config["providers"]["mock"]["account_strategy"] = json!("balance");
    config["providers"]["mock"]["accounts"] = json!([
        {"label":"first","api_key":"first-account-secret"},
        {"label":"second","api_key":"second-account-secret"}
    ]);
    std::fs::write(&fixture.config, serde_json::to_vec_pretty(&config)?)?;
    fixture.start().await?;
    let mut spec = json!({"block_id":"joint-cli-trial","source":"stub","rules":[
        {"selector":"coding","fingerprint":null,"baseline_route":"coding","challenger_route":"candidate"},
        {"selector":"review","fingerprint":null,"baseline_route":"review","challenger_route":"mock:alt"}
    ],"rationale":"Review a joint change through CLI","independence_rationale":"One isolated block","feedback":{"kind":"manual"}});
    let path = fixture.prompt("joint.json", &serde_json::to_string(&spec)?)?;
    let first = operate(
        &fixture,
        &["candidate", "preview", "--from", text_path(&path)?],
    )
    .await?;
    let again = operate(
        &fixture,
        &["candidate", "preview", "--from", text_path(&path)?],
    )
    .await?;
    assert_eq!(
        first["report"]["report"]["preview_digest"],
        again["report"]["report"]["preview_digest"]
    );
    let descriptions = first["report"]["report"]["route_descriptions"].to_string();
    for label in ["account first", "account second", "order may rotate"] {
        ensure!(
            descriptions.contains(label),
            "account description lost {label}: {descriptions}"
        );
    }
    for secret in [
        "first-account-secret",
        "second-account-secret",
        "fixture",
        "127.0.0.1",
    ] {
        ensure!(
            !descriptions.contains(secret),
            "route description leaked {secret}: {descriptions}"
        );
    }
    assert_eq!(
        first["report"]["report"]["definition"]["rules"]
            .as_array()
            .context("joint rules")?
            .len(),
        2
    );
    assert_eq!(
        first["report"]["report"]["definition"]["batch_sessions"],
        16
    );
    let mut foreign: bitrouter::core::evolution::runtime::candidates::CandidatePreview =
        serde_json::from_value(first["report"]["report"].clone())?;
    foreign.owner = "another-owner".into();
    // The digest is public integrity, not authorization. A caller can recompute
    // it; the independent owner fence must still reject the authentic shape.
    foreign.preview_digest = bitrouter::storage::evolution::digest(&(
        "candidate-preview-v1",
        &foreign.owner,
        &foreign.spec,
        &foreign.definition,
        &foreign.routing_digest,
        foreign.control_generation,
        foreign.mode_epoch,
        foreign.mode,
        foreign.reset_to_configured,
        &foreign.route_descriptions,
    ))?;
    let foreign = fixture.prompt("foreign.json", &serde_json::to_string(&foreign)?)?;
    let rejected = fixture
        .rejected(&[
            "acp",
            "evolution",
            "candidate",
            "register",
            "--from",
            text_path(&foreign)?,
        ])
        .await?;
    ensure!(
        rejected.to_string().contains("owner"),
        "owner fence missing: {rejected}"
    );
    operate(&fixture, &["judge-model", "mock:model"]).await?;
    spec["feedback"] = json!({"kind":"judge","model":"mock:model"});
    std::fs::write(&path, serde_json::to_vec(&spec)?)?;
    let judge = operate(
        &fixture,
        &["candidate", "preview", "--from", text_path(&path)?],
    )
    .await?;
    assert_eq!(
        judge["report"]["report"]["definition"]["measurement_contract"],
        json!(bitrouter::core::evolution::scoring::measurement_contract(
            AssessmentSource::Agentic,
            "checkpoint-judge:mock:model",
            bitrouter::core::evolution::judge::JUDGE_VERSION
        )?)
    );
    operate(&fixture, &["judge-model", "mock:alt"]).await?;
    fixture
        .rejected(&[
            "acp",
            "evolution",
            "candidate",
            "preview",
            "--from",
            text_path(&path)?,
        ])
        .await?;
    spec["feedback"] = json!({"kind":"manual"});
    std::fs::write(&path, serde_json::to_vec(&spec)?)?;
    let preview = operate(
        &fixture,
        &["candidate", "preview", "--from", text_path(&path)?],
    )
    .await?;
    let reviewed = fixture.prompt(
        "joint-reviewed.json",
        &serde_json::to_string(&preview["report"]["report"])?,
    )?;
    operate(
        &fixture,
        &["candidate", "register", "--from", text_path(&reviewed)?],
    )
    .await?;
    operate(
        &fixture,
        &["candidate", "register", "--from", text_path(&reviewed)?],
    )
    .await?;
    let revision = operate(&fixture, &["candidate", "revision", "joint-cli-trial"]).await?;
    let predecessor = revision["report"]["report"]["predecessor"]
        .as_str()
        .context("predecessor")?
        .to_owned();
    let mut updated = revision["report"]["report"].clone();
    assert_eq!(
        updated["rules"]
            .as_array()
            .context("inherited rules")?
            .len(),
        2
    );
    updated["rationale"] = json!("Revise the exact retained experiment");
    let revised = fixture.prompt("revision.json", &serde_json::to_string(&updated)?)?;
    let preview = operate(
        &fixture,
        &["candidate", "preview", "--from", text_path(&revised)?],
    )
    .await?;
    let stale = fixture.prompt(
        "stale-route.json",
        &serde_json::to_string(&preview["report"]["report"])?,
    )?;
    fixture.stop(false).await?;
    config["providers"]["mock"]["api_base"] = json!("http://127.0.0.1:2");
    std::fs::write(&fixture.config, serde_json::to_vec_pretty(&config)?)?;
    fixture.start().await?;
    fixture
        .rejected(&[
            "acp",
            "evolution",
            "candidate",
            "register",
            "--from",
            text_path(&stale)?,
        ])
        .await?;
    let status = operate(&fixture, &["status"]).await?;
    assert_eq!(status["report"]["control"]["mode"], "off");
    assert_eq!(
        status["report"]["control"]["blocks"]["joint-cli-trial"]["experiment_id"],
        predecessor
    );
    let fresh = operate(
        &fixture,
        &["candidate", "preview", "--from", text_path(&revised)?],
    )
    .await?;
    let fresh = fixture.prompt(
        "fresh-revision.json",
        &serde_json::to_string(&fresh["report"]["report"])?,
    )?;
    operate(
        &fixture,
        &["candidate", "register", "--from", text_path(&fresh)?],
    )
    .await?;
    let mut overlap = spec;
    overlap["block_id"] = json!("overlapping-cli-trial");
    let overlap = fixture.prompt("overlap.json", &serde_json::to_string(&overlap)?)?;
    fixture
        .rejected(&[
            "acp",
            "evolution",
            "candidate",
            "preview",
            "--from",
            text_path(&overlap)?,
        ])
        .await?;
    let catalog = operate(&fixture, &["candidate", "catalog"]).await?;
    assert_eq!(
        catalog["report"]["report"]["reserved_matchers"]
            .as_array()
            .context("reserved matchers")?
            .len(),
        2
    );
    ensure!(
        upstream
            .received_requests()
            .await
            .context("upstream requests")?
            .is_empty(),
        "candidate management executed a model request"
    );
    Ok(())
}
