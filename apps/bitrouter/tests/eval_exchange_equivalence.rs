use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use axum_test::TestServer;
use bitrouter::eval::EvalService;
use bitrouter::eval::api;
use bitrouter::eval::store::EvalStore;
use bitrouter::eval::types::{
    EVAL_SCHEMA_VERSION, EvalScope, EvalSubject, EvalVerdict, EvaluationResult, EvaluatorIdentity,
    EvaluatorKind, MetricUnit, MetricValue, evidence_digest,
};
use bitrouter_sdk::config::EvalConfig;

#[tokio::test]
async fn cli_and_rest_are_idempotent_semantic_equivalents() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let cli_db_path = directory.path().join("cli.db");
    let cli_config_path = directory.path().join("bitrouter.yaml");
    let subject_path = directory.path().join("subject.json");
    let result_path = directory.path().join("result.json");
    let subject = subject()?;
    let result = result(&subject);
    std::fs::write(&subject_path, serde_json::to_vec_pretty(&subject)?)?;
    std::fs::write(&result_path, serde_json::to_vec_pretty(&result)?)?;
    std::fs::write(
        &cli_config_path,
        format!("database:\n  url: sqlite://{}\n", cli_db_path.display()),
    )?;

    run_cli(&[
        "eval",
        "subject",
        "put",
        path_text(&subject_path)?,
        "--config",
        path_text(&cli_config_path)?,
    ])?;
    run_cli(&[
        "eval",
        "subject",
        "put",
        path_text(&subject_path)?,
        "--config",
        path_text(&cli_config_path)?,
    ])?;
    run_cli(&[
        "eval",
        "result",
        "submit",
        path_text(&result_path)?,
        "--config",
        path_text(&cli_config_path)?,
    ])?;
    run_cli(&[
        "eval",
        "result",
        "submit",
        path_text(&result_path)?,
        "--config",
        path_text(&cli_config_path)?,
    ])?;
    run_cli(&[
        "eval",
        "snapshot",
        "freeze",
        "--at",
        "2026-07-30T00:02:00Z",
        "--config",
        path_text(&cli_config_path)?,
    ])?;
    let cli_db = bitrouter::db::connect(&format!("sqlite://{}", cli_db_path.display())).await?;
    bitrouter::db::run_migrations(&cli_db).await?;
    let cli_store = EvalStore::new(cli_db);
    let cli_snapshot = cli_store
        .freeze_snapshot_for_owner("2026-07-30T00:02:00Z", "local")
        .await?;

    let rest_db = bitrouter::db::connect("sqlite::memory:").await?;
    bitrouter::db::run_migrations(&rest_db).await?;
    let rest_store = EvalStore::new(rest_db.clone());
    let service = EvalService::new(rest_store.clone(), EvalConfig::default());
    let server = TestServer::new(api::router(service, rest_db, true));
    server
        .post("/v1/evals/subjects")
        .json(&subject)
        .await
        .assert_status_ok();
    server
        .post("/v1/evals/subjects")
        .json(&subject)
        .await
        .assert_status_ok();
    server
        .post("/v1/evals/results")
        .json(&result)
        .await
        .assert_status_ok();
    let duplicate = server
        .post("/v1/evals/results")
        .json(&result)
        .await
        .json::<serde_json::Value>();
    assert_eq!(duplicate["duplicate"], true);
    let rest_snapshot = server
        .post("/v1/evals/snapshots")
        .json(&serde_json::json!({ "frozen_at": "2026-07-30T00:02:00Z" }))
        .await
        .json::<bitrouter::eval::store::EvalSnapshot>();

    assert_eq!(
        cli_store.list_subjects().await?,
        rest_store.list_subjects().await?
    );
    assert_eq!(
        cli_store.results_for_subject(&subject.eval_id).await?,
        rest_store.results_for_subject(&subject.eval_id).await?
    );
    assert_eq!(cli_snapshot, rest_snapshot);
    Ok(())
}

fn run_cli(arguments: &[&str]) -> anyhow::Result<()> {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_bro"))
        .args(arguments)
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "bitrouter {} failed: {}{}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn path_text(path: &Path) -> anyhow::Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8: {}", path.display()))
}

fn subject() -> anyhow::Result<EvalSubject> {
    let evidence = Vec::new();
    Ok(EvalSubject {
        schema_version: EVAL_SCHEMA_VERSION,
        eval_id: "eval-equivalence".into(),
        scope: EvalScope::Task,
        subject_id: "task-equivalence".into(),
        policy_digest: "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            .into(),
        preset: Some("auto:cost".into()),
        cohort: None,
        holdout: false,
        decisions: Vec::new(),
        requested_dimensions: BTreeSet::from(["quality.pass".into()]),
        evidence_digest: evidence_digest(&evidence)?,
        evidence,
        observed_at: "2026-07-30T00:00:00Z".into(),
    })
}

fn result(subject: &EvalSubject) -> EvaluationResult {
    EvaluationResult {
        schema_version: EVAL_SCHEMA_VERSION,
        eval_id: subject.eval_id.clone(),
        evidence_digest: subject.evidence_digest.clone(),
        evaluator: EvaluatorIdentity {
            authority_id: "local".into(),
            evaluator_id: "task-native-fixture".into(),
            kind: EvaluatorKind::TaskNative,
            version: "1".into(),
            config_digest: subject.policy_digest.clone(),
        },
        verdict: EvalVerdict::Pass,
        metrics: BTreeMap::from([(
            "quality.pass".into(),
            MetricValue::new(1, MetricUnit::Boolean),
        )]),
        hard_violations: Vec::new(),
        confidence_ppm: Some(1_000_000),
        evidence_refs: Vec::new(),
        decision_credit: BTreeMap::new(),
        idempotency_key: "result-equivalence".into(),
        submitted_at: "2026-07-30T00:01:00Z".into(),
    }
}
