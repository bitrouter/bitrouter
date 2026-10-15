use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use axum_test::TestServer;
use bitrouter::config::EvalConfig;
use bitrouter::eval::EvalService;
use bitrouter::eval::api;
use bitrouter::eval::store::EvalStore;
use {
    bitrouter::storage::evaluation::evidence_digest, bitrouter_ai::eval::EVAL_SCHEMA_VERSION,
    bitrouter_ai::eval::EvalScope, bitrouter_ai::eval::EvalSubject,
    bitrouter_ai::eval::EvalVerdict, bitrouter_ai::eval::EvaluationResult,
    bitrouter_ai::eval::EvaluatorIdentity, bitrouter_ai::eval::EvaluatorKind,
    bitrouter_ai::eval::MetricUnit, bitrouter_ai::eval::MetricValue,
};

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
        .json::<bitrouter_ai::eval::EvalSnapshot>();

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

struct HttpTask(tokio::task::JoinHandle<std::io::Result<()>>);

impl Drop for HttpTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn serve_http(
    db: sea_orm::DatabaseConnection,
    skip_auth: bool,
) -> anyhow::Result<(String, HttpTask)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let service = EvalService::new(EvalStore::new(db.clone()), EvalConfig::default());
    let router = api::router(service, db, skip_auth);
    let task = HttpTask(tokio::spawn(
        async move { axum::serve(listener, router).await },
    ));
    Ok((endpoint, task))
}

#[tokio::test]
async fn sdk_exchange_preserves_server_receipts_and_snapshot_identity() -> anyhow::Result<()> {
    use bitrouter_ai::eval::{AdmissionStatus, FreezeRequest, SubjectInsertOutcome};
    use bitrouter_sdk::gateway::GatewayClient;
    let db = bitrouter::db::connect("sqlite::memory:").await?;
    bitrouter::db::run_migrations(&db).await?;
    let (endpoint, _task) = serve_http(db, true).await?;
    let client = GatewayClient::new(&endpoint, None)?;
    let mut subject = subject()?;
    subject.eval_id = "eval/with ?#%雪".into();
    let inserted = client.put_eval_subject(&subject).await?;
    assert_eq!(inserted.eval_id, subject.eval_id);
    assert_eq!(inserted.outcome, SubjectInsertOutcome::Inserted);
    assert_eq!(
        client.put_eval_subject(&subject).await?.outcome,
        SubjectInsertOutcome::Duplicate
    );
    assert_eq!(
        client.eval_subjects().await?.as_slice(),
        std::slice::from_ref(&subject)
    );
    assert_eq!(client.eval_subject(&subject.eval_id).await?, subject);
    let result = result(&subject);
    let admitted = client.submit_eval_result(&result).await?;
    assert_eq!(admitted.status, AdmissionStatus::Admitted);
    assert!(!admitted.duplicate);
    let duplicate = client.submit_eval_result(&result).await?;
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.result_id, admitted.result_id);
    let status = client.eval_status().await?;
    assert_eq!(status.subjects, 1);
    assert_eq!(status.results, 1);
    assert_eq!(status.admission.get("admitted"), Some(&1));
    let snapshot = client
        .freeze_eval_snapshot(&FreezeRequest {
            frozen_at: Some("2026-07-30T00:02:00Z".into()),
        })
        .await?;
    assert_eq!(snapshot.entries.len(), 1);
    assert_eq!(snapshot.entries[0].result_id, admitted.result_id);
    assert_eq!(
        client.eval_snapshot(&snapshot.evidence_root).await?,
        snapshot
    );
    Ok(())
}

#[tokio::test]
async fn sdk_evaluation_calls_preserve_virtual_key_and_tenant_boundaries() -> anyhow::Result<()> {
    use bitrouter::auth::{db as auth_db, keys};
    use bitrouter_sdk::{error::Error, gateway::GatewayClient};
    let db = bitrouter::db::connect("sqlite::memory:").await?;
    bitrouter::db::run_migrations(&db).await?;
    let mut credentials = Vec::new();
    for user in ["sdk-user-a", "sdk-user-b"] {
        auth_db::upsert_user(&db, user).await?;
        let key = keys::generate();
        auth_db::insert_api_key(
            &db,
            &auth_db::NewApiKey {
                id: user.into(),
                key_hash: key.hash,
                user_id: user.into(),
                spend_limit_micro_usd: None,
                rpm_limit: None,
                policy_id: None,
            },
        )
        .await?;
        credentials.push(key.secret);
    }
    let (endpoint, _task) = serve_http(db, false).await?;
    let caller_a = GatewayClient::new(&endpoint, Some(&credentials[0]))?;
    let caller_b = GatewayClient::new(&endpoint, Some(&credentials[1]))?;
    let subject = subject()?;
    caller_a.put_eval_subject(&subject).await?;
    assert_eq!(
        caller_a.eval_subjects().await?.as_slice(),
        std::slice::from_ref(&subject)
    );
    assert!(caller_b.eval_subjects().await?.is_empty());
    assert!(
        matches!(caller_b.eval_subject(&subject.eval_id).await, Err(Error::Evaluation(error)) if error.status == 404)
    );
    for credential in [None, Some("control-bearer-is-not-an-inference-virtual-key")] {
        let client = GatewayClient::new(&endpoint, credential)?;
        assert!(
            matches!(client.eval_status().await, Err(Error::Evaluation(error)) if error.status == 401)
        );
    }
    Ok(())
}
