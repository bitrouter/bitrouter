use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use bitrouter::eval::compiler::EvalEvidenceSnapshot;
use bitrouter::eval::store::EvalStore;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const POLICY_DIGEST: &str =
    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const ROUTE: &str = "agent_route/v1|unknown|mechanical|normal";

#[tokio::test]
async fn unclassified_request_errors_never_reach_quality_compiler() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let attempts = (0..5)
        .map(|index| Attempt {
            canonical_task: format!("mystery-{index}"),
            attempt: format!("attempt-{index}"),
            marker: format!("mystery-{index}"),
            request_error: Some("mystery_upstream_fault"),
        })
        .collect::<Vec<_>>();
    let output = run_adapter_fixture(directory.path(), &attempts)?;
    let routes = ingest_and_compile(directory.path(), &output).await?;
    assert!(
        routes.is_empty(),
        "unclassified errors produced {routes:#?}"
    );
    let matrix = load_matrix(&output)?;

    assert_eq!(matrix.len(), 1);
    assert_eq!(matrix[0]["independent_tasks"], 0);
    assert_eq!(matrix[0]["unclassified_request_errors"], 5);
    assert_eq!(matrix[0]["active_recommendation"], "retain");
    assert_eq!(matrix[0]["controlled_validation_candidate"], false);

    Ok(())
}

#[tokio::test]
async fn repeated_attempts_compile_as_one_independent_task() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let attempts = vec![
        Attempt {
            canonical_task: "canonical-task".into(),
            attempt: "attempt-one".into(),
            marker: "canonical-one".into(),
            request_error: None,
        },
        Attempt {
            canonical_task: "canonical-task".into(),
            attempt: "attempt-two".into(),
            marker: "canonical-two".into(),
            request_error: None,
        },
    ];
    let output = run_adapter_fixture(directory.path(), &attempts)?;
    let routes = ingest_and_compile(directory.path(), &output).await?;
    let tier = &routes
        .get(&("auto".into(), ROUTE.into()))
        .ok_or_else(|| anyhow::anyhow!("adapter route missing from compiler"))?
        .tiers["economy"];
    assert_eq!(tier.eligible_episodes, 2);
    assert_eq!(tier.independent_tasks.len(), 1);
    let packets = load_jsonl(&output.join("packets.jsonl"))?;
    let eval_ids = packets
        .iter()
        .map(|packet| packet["subject"]["eval_id"].as_str())
        .collect::<BTreeSet<_>>();
    let subject_ids = packets
        .iter()
        .map(|packet| packet["subject"]["subject_id"].as_str())
        .collect::<BTreeSet<_>>();
    let matrix = load_matrix(&output)?;

    assert_eq!(eval_ids.len(), 2);
    assert_eq!(subject_ids.len(), 1);
    assert_eq!(matrix[0]["independent_tasks"], 1);
    assert_eq!(matrix[0]["independent_episodes"], 2);
    assert_eq!(matrix[0]["active_recommendation"], "retain");

    Ok(())
}

#[derive(Debug)]
struct Attempt {
    canonical_task: String,
    attempt: String,
    marker: String,
    request_error: Option<&'static str>,
}

fn run_adapter_fixture(root: &Path, attempts: &[Attempt]) -> anyhow::Result<PathBuf> {
    let run_dir = root.join("run");
    let mut traces = Vec::new();
    let mut decisions = Vec::new();
    let mut outcomes = Vec::new();
    for (index, attempt) in attempts.iter().enumerate() {
        let trial_name = format!("{}__{}", attempt.canonical_task, attempt.attempt);
        let result_dir = run_dir.join("jobs/job").join(&trial_name);
        std::fs::create_dir_all(result_dir.join("agent"))?;
        std::fs::write(
            result_dir.join("result.json"),
            serde_json::to_vec(&json!({
                "id": format!("result-{}", attempt.attempt),
                "exact_task_id": attempt.canonical_task,
                "task_name": format!("terminal-bench/{}", attempt.canonical_task),
                "task_checksum": attempt.canonical_task,
                "trial_name": trial_name,
                "verifier_result": {"rewards": {"reward": 1}},
                "critical_violations": 0,
                "finished_at": "2026-08-17T00:01:00Z"
            }))?,
        )?;
        let message = format!(
            "Task Description:\nBuild {}\n\nCurrent terminal state:\n$",
            attempt.marker
        );
        std::fs::write(
            result_dir.join("agent/trajectory.json"),
            serde_json::to_vec(&json!({
                "steps": [{"source": "user", "message": message}]
            }))?,
        )?;
        let request_id = format!("physical-{}", attempt.attempt);
        traces.push(json!({
            "id": request_id,
            "raw_body": {"messages": [{"role": "user", "content": message}]}
        }));
        decisions.push(json!({
            "decision_id": format!("decision-{}", attempt.attempt),
            "ingress_request_id_sha256": ingress_commitment(&request_id),
            "policy": "auto",
            "policy_digest": POLICY_DIGEST,
            "route_projection": ROUTE,
            "request_key": ROUTE,
            "selected_tier": "economy",
            "baseline_tier": "strong",
            "static_tier": "economy",
            "captured_at": format!("2026-08-17T00:00:{index:02}Z"),
            "progress_clause_ids": []
        }));
        outcomes.push(json!({
            "request_id": request_id,
            "error": attempt.request_error,
            "cost_micro_usd": 20,
            "latency_ms": 40
        }));
    }

    let traces_path = root.join("traces.jsonl");
    let decisions_path = root.join("decisions.jsonl");
    let outcomes_path = root.join("request-outcomes.jsonl");
    write_jsonl(&traces_path, &traces)?;
    write_jsonl(&decisions_path, &decisions)?;
    write_jsonl(&outcomes_path, &outcomes)?;
    let output = root.join("output");
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../skills/evaluating-bitrouter-routes/scripts/terminal_bench_route_evidence.py");
    run_command(
        std::process::Command::new("python3")
            .arg(script)
            .arg("--run-dir")
            .arg(run_dir)
            .arg("--traces")
            .arg(traces_path)
            .arg("--decisions")
            .arg(decisions_path)
            .arg("--request-outcomes")
            .arg(outcomes_path)
            .arg("--output-dir")
            .arg(&output),
    )?;
    Ok(output)
}

async fn ingest_and_compile(
    root: &Path,
    output: &Path,
) -> anyhow::Result<
    std::collections::BTreeMap<(String, String), bitrouter::eval::compiler::RouteEvalEvidence>,
> {
    let validation = root.join("validation");
    std::fs::create_dir_all(&validation)?;
    let database = validation.join("eval.db");
    let config = validation.join("bitrouter.yaml");
    std::fs::write(
        &config,
        format!(
            "inherit_defaults: false\ndatabase:\n  url: sqlite://{}\n",
            database.display()
        ),
    )?;
    for (index, packet) in load_jsonl(&output.join("packets.jsonl"))?
        .iter()
        .enumerate()
    {
        let subject = validation.join(format!("subject-{index}.json"));
        let sealed = validation.join(format!("sealed-{index}.json"));
        let result = validation.join(format!("result-{index}.json"));
        std::fs::write(&subject, serde_json::to_vec(&packet["subject"])?)?;
        std::fs::write(&result, serde_json::to_vec(&packet["result"])?)?;
        run_bitrouter(
            &validation,
            [
                "eval",
                "subject",
                "seal",
                path_text(&subject)?,
                "--output",
                path_text(&sealed)?,
            ],
        )?;
        run_bitrouter(
            &validation,
            [
                "eval",
                "subject",
                "put",
                path_text(&sealed)?,
                "--config",
                path_text(&config)?,
            ],
        )?;
        run_bitrouter(
            &validation,
            [
                "eval",
                "result",
                "submit",
                path_text(&result)?,
                "--config",
                path_text(&config)?,
            ],
        )?;
    }
    let db = bitrouter::db::connect(&format!("sqlite://{}", database.display())).await?;
    bitrouter::db::run_migrations(&db).await?;
    let store = EvalStore::new(db);
    let frozen = store
        .freeze_snapshot_for_owner("2026-08-17T00:02:00Z", "local")
        .await?;
    EvalEvidenceSnapshot::load(&store, &frozen.evidence_root)
        .await?
        .route_evidence()
}

fn run_bitrouter<const N: usize>(cwd: &Path, args: [&str; N]) -> anyhow::Result<()> {
    run_command(
        std::process::Command::new(env!("CARGO_BIN_EXE_bitrouter"))
            .current_dir(cwd)
            .args(args),
    )
}

fn run_command(command: &mut std::process::Command) -> anyhow::Result<()> {
    let output = command.output()?;
    if !output.status.success() {
        anyhow::bail!(
            "command failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn ingress_commitment(request_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"bitrouter.ingress-request-id.v1\0");
    digest.update(request_id.as_bytes());
    format!("sha256:{}", hex::encode(digest.finalize()))
}

fn write_jsonl(path: &Path, rows: &[Value]) -> anyhow::Result<()> {
    let mut bytes = Vec::new();
    for row in rows {
        serde_json::to_writer(&mut bytes, row)?;
        bytes.push(b'\n');
    }
    std::fs::write(path, bytes)?;
    Ok(())
}

fn load_jsonl(path: &Path) -> anyhow::Result<Vec<Value>> {
    std::fs::read_to_string(path)?
        .lines()
        .map(|line| serde_json::from_str(line).map_err(anyhow::Error::from))
        .collect()
}

fn load_matrix(output: &Path) -> anyhow::Result<Vec<Value>> {
    Ok(serde_json::from_slice(&std::fs::read(
        output.join("matrix.json"),
    )?)?)
}

fn path_text(path: &Path) -> anyhow::Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8: {}", path.display()))
}
