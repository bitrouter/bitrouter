use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::routing::post;
use axum_test::TestServer;
use bitrouter::adequacy::reliability::{ReliabilityEvent, ReliabilityKey, ReliabilityObservation};
use bitrouter::adequacy::store::AdequacyStore;
use bitrouter::eval::store::EvalStore;
use bitrouter::eval::types::AdmissionStatus;
use bitrouter::metering::{
    ChargeEvidence, ChargeStatus, EffectivePricingRates, PricingSource, ReconciliationStatus,
};
use bitrouter::policy_lock;
use bitrouter::workflow_state::archive::{
    CloudUsageRecord, RequestTransportOutcome, SemanticSettlementOutcome, TraceArchive,
    WorkflowRunArtifact,
};
use bitrouter::workflow_state::decision::{
    PolicyDecisionRecord, PolicyDecisionSummary, ingress_request_id_sha256,
};
use bitrouter::workflow_state::fixture::WorkflowTraceFixture;
use bitrouter::workflow_state::ir::{HarnessId, ProtocolKind};
use bitrouter::workflow_state::online::OnlineWorkflowState;
use bitrouter::workflow_state::predictive::{NextActionClass, NextStepRole};
use bitrouter::workflow_state::real_trace::{
    CapturedIngressTrace, RealTraceCapture, RealTraceOutcome, TraceCaptureOptions, TraceSanitizer,
};
use bitrouter::workflow_state::replay::ReplayEvaluator;
use bitrouter::workflow_state::reward::BenchmarkOutcomeRecord;
use bitrouter::workflow_state::shadow_policy::{ShadowPolicyEvaluator, TierName};
use bitrouter_sdk::HeaderMap;
use bitrouter_sdk::config;
use bitrouter_sdk::language_model::{
    ApiProtocol, NormalizedUsage, UsageOrigin, inbound_adapter_for,
};
use serde::Deserialize;
use serde_json::json;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/workflow_state/hermes")
        .join(name)
}

#[test]
fn loads_workflow_trace_fixture() {
    let fixture = WorkflowTraceFixture::load_file(fixture_path("opening.json")).unwrap();
    assert_eq!(fixture.id, "hermes-opening-001");
    assert_eq!(fixture.expected.state_kind.to_string(), "opening");
    assert_eq!(fixture.prompt.model, "bitrouter-mvp-alias");
}

#[test]
fn fixture_exposes_policy_table_baseline_fingerprint() {
    let fixture = WorkflowTraceFixture::load_file(fixture_path("tool_followup.json")).unwrap();
    assert_eq!(fixture.baseline_fingerprint(), "after_bash");
    assert_eq!(fixture.expected.baseline_fingerprint, "after_bash");
}

#[test]
fn loads_runtime_fixture_with_canonical_prompt_fallback() {
    let fixture = WorkflowTraceFixture::load_file(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/workflow_state/openclaw/runtime_stub.json"),
    )
    .unwrap();
    assert_eq!(fixture.id, "openclaw-runtime-stub-001");
    assert_eq!(fixture.prompt.model, "openclaw-runtime-model");
}

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workflow_state")
}

fn temp_path(name: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "bitrouter-workflow-state-{name}-{}-{unique}",
        std::process::id()
    ))
}

#[derive(Deserialize)]
struct ProgressFixtureRequest {
    stage: String,
    body: serde_json::Value,
}

#[test]
fn general_progress_fixture_replays_without_routing_headers() -> anyhow::Result<()> {
    let adapter = inbound_adapter_for(&ApiProtocol::ChatCompletions)
        .ok_or_else(|| anyhow::anyhow!("chat completions adapter is unavailable"))?;
    let requests = include_str!("fixtures/trajectory/recovery_then_repeat.jsonl")
        .lines()
        .map(serde_json::from_str::<ProgressFixtureRequest>)
        .collect::<Result<Vec<_>, _>>()?;

    let stages = requests
        .iter()
        .map(|request| request.stage.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        stages,
        [
            "opening", "recovery", "review_1", "review_2", "review_3", "review_4", "review_5",
        ]
    );

    let projections = requests
        .into_iter()
        .map(|request| {
            let prompt = adapter.parse_request(request.body)?;
            Ok(OnlineWorkflowState::from_prompt(
                &HeaderMap::new(),
                &prompt,
                None,
                ProtocolKind::ChatCompletions,
            )
            .routing_key()
            .to_owned())
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    assert_eq!(
        projections,
        [
            "agent_route/v1|unknown|normal",
            "agent_route/v1|implement|guarded",
            "agent_route/v1|implement|guarded",
            "agent_route/v1|implement|guarded",
            "agent_route/v1|implement|normal",
            "agent_route/v1|implement|normal",
            "agent_route/v1|implement|normal",
        ]
    );
    Ok(())
}

#[tokio::test]
async fn reliability_report_cli_replays_persisted_events_without_mutating_database() {
    let root = temp_path("reliability-report");
    std::fs::create_dir_all(&root).unwrap();
    let database_path = root.join("bitrouter.db");
    let database_url = format!("sqlite://{}", database_path.display());
    let config_path = root.join("bitrouter.yaml");
    let first_output = root.join("reliability-first.json");
    let second_output = root.join("reliability-second.json");
    std::fs::write(
        &config_path,
        r#"policy_table:
  adequacy:
    enabled: true
    reliability_window_size: 23
    reliability_consecutive_failures: 2
    reliability_error_rate_percent: 35
    reliability_cooldown_secs: 300
"#,
    )
    .unwrap();
    let db = bitrouter::db::connect(&database_url).await.unwrap();
    bitrouter::db::run_migrations(&db).await.unwrap();
    let store = AdequacyStore::new(db.clone());
    store
        .append_reliability_event(&ReliabilityEvent {
            request_id: "request-1".to_string(),
            route_key: "bitrouter:economy-model".to_string(),
            endpoint_key: ReliabilityKey {
                provider: "bitrouter".to_string(),
                model: "economy-model".to_string(),
                credential_class: "default:oauth".to_string(),
                endpoint_scope: "api.example.test:443".to_string(),
                protocol: "responses".to_string(),
            },
            observation: ReliabilityObservation::TransientFailure,
            half_open_probe: false,
            observed_at_unix: 100,
        })
        .await
        .unwrap();

    for output_path in [&first_output, &second_output] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_bitrouter"))
            .args([
                "workflow-state",
                "reliability-report",
                "--database-url",
                &database_url,
                "--config",
                config_path.to_str().unwrap(),
                "--output",
                output_path.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "reliability report failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let first = std::fs::read(&first_output).unwrap();
    let second = std::fs::read(&second_output).unwrap();
    assert_eq!(first, second);
    let report: serde_json::Value = serde_json::from_slice(&first).unwrap();
    assert_eq!(report["event_count"], 1);
    assert_eq!(report["events"][0]["request_id"], "request-1");
    assert_eq!(store.load_reliability_events().await.unwrap().len(), 1);

    // Windows keeps the SQLite file locked while the pool is alive. Release all
    // references and explicitly close the pool before deleting the temp directory.
    drop(store);
    db.close().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

fn computed_usage(
    request_id: &str,
    provider_id: &str,
    model_id: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
    charge_micro_usd: u64,
) -> CloudUsageRecord {
    let input_rate = if prompt_tokens > 0 {
        charge_micro_usd as f64 / prompt_tokens as f64
    } else {
        0.0
    };
    let normalized = NormalizedUsage {
        uncached_input_tokens: prompt_tokens,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        output_tokens: completion_tokens,
        reasoning_tokens: 0,
    };
    CloudUsageRecord {
        id: Some(format!("usage-{request_id}")),
        request_id: Some(request_id.to_string()),
        provider_id: provider_id.to_string(),
        model_id: model_id.to_string(),
        prompt_tokens,
        completion_tokens,
        reasoning_tokens: 0,
        uncached_input_tokens: prompt_tokens,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        output_tokens: completion_tokens,
        usage_origin: UsageOrigin::ProviderReported,
        raw_usage: Some(json!({
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens
        })),
        final_charge_micro_usd: Some(charge_micro_usd),
        charge_status: ChargeStatus::Computed,
        charge_evidence: Some(ChargeEvidence {
            status: ChargeStatus::Computed,
            charge_micro_usd: Some(charge_micro_usd as i64),
            normalized_usage: normalized,
            effective_rates: EffectivePricingRates {
                uncached_input_micro_usd_per_token: Some(input_rate),
                cache_read_micro_usd_per_token: Some(0.1),
                cache_write_micro_usd_per_token: Some(1.25),
                output_micro_usd_per_token: Some(0.0),
            },
            pricing_source: PricingSource::Configured,
            pricing_version: format!("sha256:{}", "0".repeat(64)),
            unknown_reason: None,
        }),
        reconciliation_status: ReconciliationStatus::NotApplicable,
        reconciliation_attempts: 0,
        authoritative_receipt: None,
        status: Some("succeeded".to_string()),
    }
}

#[test]
fn benchmark_integrity_recomputes_charge_from_effective_rates() {
    let traces = vec![benchmark_trace("req-1")];
    let mut usage = computed_usage("req-1", "openai", "gpt-test", 10, 2, 30);
    usage
        .charge_evidence
        .as_mut()
        .expect("charge evidence")
        .effective_rates
        .uncached_input_micro_usd_per_token = Some(1.0);

    let error = WorkflowRunArtifact::validate_benchmark_integrity(&traces, &[usage])
        .expect_err("charge inconsistent with effective rates must fail");

    assert!(
        error
            .to_string()
            .contains("charge does not match effective rates")
    );
}

#[test]
fn benchmark_integrity_rounds_shared_output_rate_after_combining_tokens() {
    let traces = vec![benchmark_trace("req-shared-output-rate")];
    let mut usage = computed_usage(
        "req-shared-output-rate",
        "ambient",
        "moonshotai/kimi-k2.7-code",
        1_137,
        258,
        1_985,
    );
    usage.output_tokens = 213;
    usage.reasoning_tokens = 45;
    usage.raw_usage = Some(json!({
        "prompt_tokens": 1_137,
        "completion_tokens": 258,
        "output_tokens": 213,
        "reasoning_tokens": 45,
    }));
    let evidence = usage.charge_evidence.as_mut().expect("charge evidence");
    evidence.normalized_usage.output_tokens = 213;
    evidence.normalized_usage.reasoning_tokens = 45;
    evidence.effective_rates = EffectivePricingRates {
        uncached_input_micro_usd_per_token: Some(0.84),
        cache_read_micro_usd_per_token: Some(0.84),
        cache_write_micro_usd_per_token: Some(0.84),
        output_micro_usd_per_token: Some(3.99),
    };

    WorkflowRunArtifact::validate_benchmark_integrity(&traces, &[usage])
        .expect("shared-rate completion tokens must round after one multiplication");
}

#[test]
fn benchmark_integrity_rounds_all_equal_rate_buckets_after_combining_tokens() {
    let traces = vec![benchmark_trace("req-all-equal-rates")];
    let mut usage = computed_usage(
        "req-all-equal-rates",
        "ambient",
        "moonshotai/kimi-k2.7-code",
        265_355,
        1_445,
        232_450,
    );
    usage.uncached_input_tokens = 96_565;
    usage.cache_read_tokens = 84_573;
    usage.cache_write_tokens = 84_217;
    usage.output_tokens = 1_445;
    usage.raw_usage = Some(json!({
        "prompt_tokens": 265_355,
        "completion_tokens": 1_445,
        "cache_read_tokens": 84_573,
        "cache_write_tokens": 84_217,
    }));
    let evidence = usage.charge_evidence.as_mut().expect("charge evidence");
    evidence.normalized_usage = NormalizedUsage {
        uncached_input_tokens: 96_565,
        cache_read_tokens: 84_573,
        cache_write_tokens: 84_217,
        output_tokens: 1_445,
        reasoning_tokens: 0,
    };
    evidence.effective_rates = EffectivePricingRates {
        uncached_input_micro_usd_per_token: Some(0.87125),
        cache_read_micro_usd_per_token: Some(0.87125),
        cache_write_micro_usd_per_token: Some(0.87125),
        output_micro_usd_per_token: Some(0.87125),
    };

    WorkflowRunArtifact::validate_benchmark_integrity(&traces, &[usage])
        .expect("all buckets sharing one frozen rate must round after one multiplication");
}

fn benchmark_trace(request_id: &str) -> CapturedIngressTrace {
    CapturedIngressTrace {
        id: request_id.to_string(),
        captured_at: None,
        harness: HarnessId::Hermes,
        protocol: ProtocolKind::ChatCompletions,
        method: "POST".to_string(),
        path: "/v1/chat/completions".to_string(),
        headers: [("x-request-id".to_string(), request_id.to_string())]
            .into_iter()
            .collect(),
        raw_body: json!({"model": "test", "messages": []}),
        outcome: RealTraceOutcome {
            http_status: 200,
            status: "completed".to_string(),
        },
    }
}

fn benchmark_decision(request_id: &str) -> PolicyDecisionRecord {
    PolicyDecisionRecord {
        captured_at: None,
        request_id: Some(request_id.to_string()),
        ingress_request_id_sha256: None,
        input_model: "inbound".to_string(),
        key_strategy: "workflow_state".to_string(),
        request_key: "agent_trace/v1|opening|normal".to_string(),
        ledger_key: None,
        policy: None,
        policy_digest: None,
        preset_variant: None,
        baseline_tier: Some("strong".to_string()),
        legacy_fingerprint: "opening".to_string(),
        workflow_state: "opening".to_string(),
        workflow_identity: Default::default(),
        static_tier: Some("strong".to_string()),
        static_model: Some("vendor/strong".to_string()),
        selected_tier: Some("strong".to_string()),
        selected_model: Some("vendor/strong".to_string()),
        continuation_proposed_tier: None,
        continuation_proposed_model: None,
        continuation_adjustment: None,
        predicted_role: None,
        predicted_action: None,
        prediction_confidence_ppm: None,
        predictor_contract_digest: None,
        prediction_confidence_kind: None,
        prediction_reason_codes: Vec::new(),
        observed_route_projection: None,
        trajectory_episode_id: None,
        trajectory_sequence: None,
        trajectory_completeness: None,
        trajectory_health_digest: None,
        candidate_tier: None,
        progress_clause_ids: Vec::new(),
        reason: "static_table".to_string(),
        pinned: false,
        request_qualified: false,
        semantic_successes: 0,
        semantic_success_threshold: 0,
        locked: false,
        trialed: false,
    }
}

#[test]
fn benchmark_integrity_rejects_unknown_charge_evidence() {
    let traces = vec![benchmark_trace("req-1")];
    let mut usage = computed_usage("req-1", "openai", "gpt-test", 10, 2, 30);
    usage.charge_status = ChargeStatus::Unknown;
    usage.final_charge_micro_usd = None;

    let error = WorkflowRunArtifact::validate_benchmark_integrity(&traces, &[usage])
        .expect_err("unknown charge must fail benchmark integrity");

    assert!(error.to_string().contains("charge is not computed"));
}

#[test]
fn reward_feedback_integrity_accepts_terminus_without_private_identity_headers() {
    let mut terminus = benchmark_trace("req-reward-terminus");
    terminus.harness = HarnessId::Terminus2;
    let generic = benchmark_trace("req-reward-generic");
    let terminus_usage = computed_usage("req-reward-terminus", "openai", "gpt-test", 10, 2, 30);
    let generic_usage = computed_usage("req-reward-generic", "openai", "gpt-test", 10, 2, 30);
    let terminus_decision = benchmark_decision("req-reward-terminus");
    let generic_decision = benchmark_decision("req-reward-generic");

    WorkflowRunArtifact::validate_reward_feedback_integrity(
        &[terminus, generic],
        &[terminus_usage, generic_usage],
        &[],
        &[terminus_decision, generic_decision],
    )
    .expect("reward admission must not require Terminus diagnostic identity headers");
}

#[tokio::test]
async fn equivalent_generic_and_terminus_rewards_enter_generic_eval_without_private_headers() {
    let canonical_key = "agent_trace/v1|tool_followup|normal";
    let ledger_key = format!("coding\0{canonical_key}");
    let mut generic = benchmark_trace("req-reward-generic-merge");
    generic.headers.insert(
        "x-request-id".to_string(),
        "generic-public-header-b".to_string(),
    );
    generic.headers.insert(
        "x-bitrouter-request-id".to_string(),
        "generic-private-header-c".to_string(),
    );
    let mut terminus = benchmark_trace("req-reward-terminus-merge");
    terminus.harness = HarnessId::Terminus2;
    terminus.headers.insert(
        "x-request-id".to_string(),
        "terminus-public-header-b".to_string(),
    );
    terminus.headers.insert(
        "x-bitrouter-request-id".to_string(),
        "terminus-private-header-c".to_string(),
    );

    let mut generic_decision = benchmark_decision("req-reward-generic-merge");
    generic_decision.key_strategy = "agent_trace".to_string();
    generic_decision.request_key = canonical_key.to_string();
    generic_decision.ledger_key = Some(ledger_key.clone());
    generic_decision.selected_tier = Some("cheap".to_string());
    generic_decision.selected_model = Some("vendor/cheap".to_string());
    let mut terminus_decision = generic_decision.clone();
    terminus_decision.request_id = Some("req-reward-terminus-merge".to_string());

    let outcomes = vec![
        BenchmarkOutcomeRecord::new("episode-generic", "terminal-bench/regex-log", 1.0)
            .with_request_id("req-reward-generic-merge")
            .with_trial_name("episode-generic"),
        BenchmarkOutcomeRecord::new("episode-terminus", "terminal-bench/regex-log", 1.0)
            .with_request_id("req-reward-terminus-merge")
            .with_trial_name("episode-terminus"),
    ];
    let mut generic_usage =
        computed_usage("req-reward-generic-merge", "openai", "gpt-test", 10, 2, 30);
    generic_usage.status = Some("completed".to_string());
    let mut terminus_usage =
        computed_usage("req-reward-terminus-merge", "openai", "gpt-test", 10, 2, 30);
    terminus_usage.status = Some("completed".to_string());
    WorkflowRunArtifact::validate_reward_feedback_integrity(
        &[generic.clone(), terminus.clone()],
        &[generic_usage.clone(), terminus_usage.clone()],
        &outcomes,
        &[generic_decision.clone(), terminus_decision.clone()],
    )
    .expect("source-neutral reward admission without private workflow headers");
    let artifact = WorkflowRunArtifact::build_with_decisions(
        "equivalent-adapters",
        &[generic.clone(), terminus.clone()],
        &[generic_usage.clone(), terminus_usage.clone()],
        &outcomes,
        &[generic_decision.clone(), terminus_decision.clone()],
    )
    .expect("source-neutral reward candidate construction");

    assert_eq!(artifact.semantic_policy_transition_candidates.len(), 2);
    assert!(
        artifact
            .semantic_policy_transition_candidates
            .iter()
            .all(|candidate| candidate.ledger_key.as_deref() == Some(ledger_key.as_str()))
    );

    assert!(
        generic
            .headers
            .keys()
            .chain(terminus.headers.keys())
            .all(|header| !header.starts_with("x-bitrouter-workflow-")
                && !header.starts_with("x-bitrouter-parent-session")
                && !header.starts_with("x-bitrouter-agent-")),
        "feedback fixtures must not smuggle private workflow/session/identity headers"
    );

    let root = temp_path("source-neutral-reward-feedback-command");
    std::fs::create_dir_all(&root).unwrap();
    let traces_path = root.join("traces.jsonl");
    let usage_path = root.join("usage.jsonl");
    let outcomes_path = root.join("outcomes.jsonl");
    let decisions_path = root.join("decisions.jsonl");
    let database_url = format!("sqlite://{}", root.join("adequacy.db").display());
    let setup_db = bitrouter::db::connect(&database_url).await.unwrap();
    bitrouter::db::run_migrations(&setup_db).await.unwrap();
    setup_db.close().await.unwrap();
    std::fs::write(
        &traces_path,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&generic).unwrap(),
            serde_json::to_string(&terminus).unwrap()
        ),
    )
    .unwrap();
    std::fs::write(
        &usage_path,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&generic_usage).unwrap(),
            serde_json::to_string(&terminus_usage).unwrap()
        ),
    )
    .unwrap();
    BenchmarkOutcomeRecord::write_jsonl(&outcomes_path, &outcomes).unwrap();
    PolicyDecisionRecord::write_jsonl(&decisions_path, &[generic_decision, terminus_decision])
        .unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_bitrouter"))
        .args([
            "workflow-state",
            "apply-reward-feedback",
            "--database-url",
            &database_url,
            "--traces",
            traces_path.to_str().unwrap(),
            "--cloud-usage",
            usage_path.to_str().unwrap(),
            "--outcomes",
            outcomes_path.to_str().unwrap(),
            "--policy-decisions",
            decisions_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "reward feedback command failed (status={}): stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("2 candidates"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );

    let db = bitrouter::db::connect(&database_url).await.unwrap();
    let eval_store = EvalStore::new(db.clone());
    let subjects = eval_store.list_subjects().await.unwrap();
    assert_eq!(subjects.len(), 2);
    assert!(subjects.iter().all(|subject| {
        subject.decisions.len() == 1
            && subject.decisions[0].policy == "coding"
            && subject.decisions[0].request_key == canonical_key
    }));
    let admissions = eval_store.latest_admissions().await.unwrap();
    assert_eq!(admissions.len(), 2);
    assert!(
        admissions
            .values()
            .all(|event| event.status == AdmissionStatus::Admitted)
    );
    let legacy_counts = AdequacyStore::new(db.clone())
        .load_semantic_success_counts()
        .await
        .unwrap();
    assert!(legacy_counts.is_empty());
    db.close().await.unwrap();

    let mut conflicting_trace = benchmark_trace("header-public-b");
    conflicting_trace.id = "artifact-trace-a".to_string();
    conflicting_trace.headers.insert(
        "x-bitrouter-request-id".to_string(),
        "private-header-c".to_string(),
    );
    let mut conflicting_usage = computed_usage("private-header-c", "openai", "gpt-test", 10, 2, 30);
    conflicting_usage.status = Some("completed".to_string());
    let mut conflicting_decision = benchmark_decision("private-header-c");
    conflicting_decision.key_strategy = "agent_trace".to_string();
    conflicting_decision.request_key = canonical_key.to_string();
    conflicting_decision.ledger_key = Some(ledger_key.clone());
    conflicting_decision.selected_tier = Some("cheap".to_string());
    conflicting_decision.selected_model = Some("vendor/cheap".to_string());
    let conflicting_outcome =
        BenchmarkOutcomeRecord::new("episode-conflict", "terminal-bench/header-conflict", 1.0)
            .with_request_id("header-public-b");
    let conflicting_traces_path = root.join("conflicting-traces.jsonl");
    let conflicting_usage_path = root.join("conflicting-usage.jsonl");
    let conflicting_outcomes_path = root.join("conflicting-outcomes.jsonl");
    let conflicting_decisions_path = root.join("conflicting-decisions.jsonl");
    std::fs::write(
        &conflicting_traces_path,
        format!("{}\n", serde_json::to_string(&conflicting_trace).unwrap()),
    )
    .unwrap();
    std::fs::write(
        &conflicting_usage_path,
        format!("{}\n", serde_json::to_string(&conflicting_usage).unwrap()),
    )
    .unwrap();
    BenchmarkOutcomeRecord::write_jsonl(&conflicting_outcomes_path, &[conflicting_outcome])
        .unwrap();
    PolicyDecisionRecord::write_jsonl(&conflicting_decisions_path, &[conflicting_decision])
        .unwrap();
    let conflicting_output = std::process::Command::new(env!("CARGO_BIN_EXE_bitrouter"))
        .args([
            "workflow-state",
            "apply-reward-feedback",
            "--database-url",
            &database_url,
            "--traces",
            conflicting_traces_path.to_str().unwrap(),
            "--cloud-usage",
            conflicting_usage_path.to_str().unwrap(),
            "--outcomes",
            conflicting_outcomes_path.to_str().unwrap(),
            "--policy-decisions",
            conflicting_decisions_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        !conflicting_output.status.success(),
        "conflicting headers must fail before learning: stdout={} stderr={}",
        String::from_utf8_lossy(&conflicting_output.stdout),
        String::from_utf8_lossy(&conflicting_output.stderr)
    );
    let db = bitrouter::db::connect(&database_url).await.unwrap();
    let eval_store = EvalStore::new(db.clone());
    assert_eq!(
        eval_store.list_subjects().await.unwrap().len(),
        2,
        "failed feedback must not mutate the eval ledger"
    );
    assert_eq!(eval_store.latest_admissions().await.unwrap().len(), 2);
    db.close().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn reward_feedback_identity_join_fails_closed_for_duplicate_ambiguous_missing_and_mismatched_ids() {
    let trace = benchmark_trace("req-reward-identity");
    let usage = computed_usage("req-reward-identity", "openai", "gpt-test", 10, 2, 30);
    let decision = benchmark_decision("req-reward-identity");
    let outcome = |request_id: Option<&str>| {
        let outcome = BenchmarkOutcomeRecord::new(
            "legacy-session-is-not-an-admission-key",
            "terminal-bench/regex-log",
            1.0,
        );
        match request_id {
            Some(request_id) => outcome.with_request_id(request_id),
            None => outcome,
        }
    };

    let cases = [
        (
            "ambiguous duplicate outcome",
            vec![
                outcome(Some("req-reward-identity")),
                outcome(Some("req-reward-identity")),
            ],
        ),
        ("missing outcome identity", vec![outcome(None)]),
        (
            "mismatched outcome identity",
            vec![outcome(Some("another-request"))],
        ),
    ];
    for (case, outcomes) in cases {
        let error = WorkflowRunArtifact::validate_reward_feedback_integrity(
            std::slice::from_ref(&trace),
            std::slice::from_ref(&usage),
            &outcomes,
            std::slice::from_ref(&decision),
        )
        .expect_err(case);
        assert!(
            error.to_string().contains("outcome join incomplete"),
            "{case}: {error}"
        );
    }

    let error = WorkflowRunArtifact::validate_reward_feedback_integrity(
        &[trace.clone(), trace],
        &[usage.clone(), usage],
        &[outcome(Some("req-reward-identity"))],
        &[decision.clone(), decision],
    )
    .expect_err("duplicate trace/request identity must fail closed");
    assert!(error.to_string().contains("duplicate"), "{error}");
}

#[test]
fn strict_reward_identity_rejects_conflicting_trace_headers_before_learning() {
    let mut trace = benchmark_trace("header-public-b");
    trace.id = "artifact-trace-a".to_string();
    trace.headers.insert(
        "x-bitrouter-request-id".to_string(),
        "private-header-c".to_string(),
    );
    let usage = computed_usage("private-header-c", "openai", "gpt-test", 10, 2, 30);
    let decision = benchmark_decision("private-header-c");
    let outcome = BenchmarkOutcomeRecord::new("episode-a", "terminal-bench/regex-log", 1.0)
        .with_request_id("header-public-b");

    WorkflowRunArtifact::validate_reward_feedback_integrity(
        &[trace],
        &[usage],
        &[outcome],
        &[decision],
    )
    .expect_err("strict reward identity must not reconcile conflicting trace headers");
}

#[test]
fn benchmark_integrity_accepts_exact_authoritative_computed_receipt() {
    let traces = vec![benchmark_trace("req-1")];
    let mut usage = computed_usage("req-1", "openai", "gpt-test", 10, 2, 30);
    let receipt = json!({
        "request_id": "req-1",
        "state": "computed",
        "model_id": "gpt-test",
        "provider_id": "openai",
        "usage": {
            "uncached_input_tokens": 10,
            "cache_read_tokens": 0,
            "cache_write_tokens": 0,
            "output_tokens": 2,
            "reasoning_tokens": 0
        },
        "final_charge_micro_usd": 30
    });
    usage.usage_origin = UsageOrigin::AuthoritativeReceipt;
    usage.raw_usage = Some(receipt.clone());
    usage.reconciliation_status = ReconciliationStatus::Computed;
    usage.reconciliation_attempts = 1;
    usage.authoritative_receipt = Some(receipt);

    WorkflowRunArtifact::validate_benchmark_integrity(&traces, &[usage])
        .expect("exact authoritative receipt should pass");
}

#[test]
fn benchmark_integrity_rejects_pending_authoritative_reconciliation() {
    let traces = vec![benchmark_trace("req-1")];
    let mut usage = computed_usage("req-1", "openai", "gpt-test", 10, 2, 30);
    usage.reconciliation_status = ReconciliationStatus::Pending;

    let error = WorkflowRunArtifact::validate_benchmark_integrity(&traces, &[usage])
        .expect_err("pending receipt must fail artifact assembly");

    assert!(error.to_string().contains("reconciliation is pending"));
}

#[test]
fn benchmark_integrity_accepts_authoritative_not_charged_without_zero_imputation() {
    let traces = vec![benchmark_trace("req-1")];
    let receipt = json!({
        "request_id": "req-1",
        "state": "not_charged",
        "model_id": null,
        "provider_id": null,
        "usage": {
            "uncached_input_tokens": 0,
            "cache_read_tokens": 0,
            "cache_write_tokens": 0,
            "output_tokens": 0,
            "reasoning_tokens": 0
        },
        "final_charge_micro_usd": null
    });
    let usage = CloudUsageRecord {
        request_id: Some("req-1".to_string()),
        provider_id: "bitrouter".to_string(),
        model_id: "unresolved".to_string(),
        usage_origin: UsageOrigin::AuthoritativeReceipt,
        raw_usage: Some(receipt.clone()),
        charge_status: ChargeStatus::NotCharged,
        reconciliation_status: ReconciliationStatus::NotCharged,
        reconciliation_attempts: 1,
        authoritative_receipt: Some(receipt),
        status: Some("failed".to_string()),
        ..Default::default()
    };

    WorkflowRunArtifact::validate_benchmark_integrity(&traces, &[usage])
        .expect("authoritative not-charged receipt should pass");
}

#[test]
fn benchmark_integrity_rejects_duplicate_or_unmatched_request_ids() {
    let traces = vec![benchmark_trace("req-1")];
    let duplicate = computed_usage("req-1", "openai", "gpt-test", 10, 2, 30);
    let error =
        WorkflowRunArtifact::validate_benchmark_integrity(&traces, &[duplicate.clone(), duplicate])
            .expect_err("duplicate usage ids must fail");
    assert!(error.to_string().contains("duplicate usage request id"));

    let unmatched = computed_usage("req-2", "openai", "gpt-test", 10, 2, 30);
    let error = WorkflowRunArtifact::validate_benchmark_integrity(&traces, &[unmatched])
        .expect_err("unmatched usage ids must fail");
    assert!(error.to_string().contains("request ids differ"));
}

#[test]
fn benchmark_integrity_rejects_nonempty_traces_without_usage() {
    let traces = vec![benchmark_trace("req-without-usage")];

    let error = WorkflowRunArtifact::validate_benchmark_integrity(&traces, &[])
        .expect_err("a benchmark trace without settlement evidence must fail closed");

    assert!(error.to_string().contains("usage snapshot is empty"));
}

#[test]
fn benchmark_bundle_accepts_equivalent_native_and_generic_request_id_joins() {
    let generic_trace = benchmark_trace("generic-request");
    let mut terminus_trace = benchmark_trace("terminus-request");
    terminus_trace.harness = HarnessId::Terminus2;

    let generic_usage = computed_usage("generic-request", "openai", "gpt-test", 10, 2, 30);
    let terminus_usage = computed_usage("terminus-request", "openai", "gpt-test", 10, 2, 30);
    let generic_decision = benchmark_decision("generic-request");
    let terminus_decision = benchmark_decision("terminus-request");
    let generic_outcome =
        BenchmarkOutcomeRecord::new("generic-episode", "terminal-bench/task", 1.0)
            .with_request_id("generic-request");
    let terminus_outcome =
        BenchmarkOutcomeRecord::new("terminus-episode", "terminal-bench/task", 1.0)
            .with_request_id("terminus-request");

    WorkflowRunArtifact::validate_complete_benchmark_integrity(
        &[generic_trace],
        &[generic_usage],
        &[generic_outcome],
        &[generic_decision],
    )
    .expect("a generic bundle with stable request-id joins should be accepted");
    WorkflowRunArtifact::validate_complete_benchmark_integrity(
        &[terminus_trace],
        &[terminus_usage],
        &[terminus_outcome],
        &[terminus_decision],
    )
    .expect("a native Terminus bundle must not require private identity headers");
}

#[test]
fn benchmark_bundle_joins_opaque_decisions_by_ingress_request_commitment() {
    let trace = benchmark_trace("br-bench-request-1");
    let usage = computed_usage("br-bench-request-1", "openai", "gpt-test", 10, 2, 30);
    let mut decision = benchmark_decision("trajectory-request-opaque");
    decision.ingress_request_id_sha256 = Some(ingress_request_id_sha256("br-bench-request-1"));

    WorkflowRunArtifact::validate_benchmark_integrity_with_decisions(
        &[trace],
        &[usage],
        &[decision],
    )
    .expect("an opaque policy identity must join through its ingress commitment");
}

#[test]
fn benchmark_bundle_rejects_mixed_or_duplicate_ingress_commitments() {
    let traces = vec![benchmark_trace("req-1"), benchmark_trace("req-2")];
    let usage = vec![
        computed_usage("req-1", "openai", "gpt-test", 10, 2, 30),
        computed_usage("req-2", "openai", "gpt-test", 10, 2, 30),
    ];
    let mut committed = benchmark_decision("trajectory-request-1");
    committed.ingress_request_id_sha256 = Some(ingress_request_id_sha256("req-1"));
    let legacy = benchmark_decision("req-2");

    let error = WorkflowRunArtifact::validate_benchmark_integrity_with_decisions(
        &traces,
        &usage,
        &[committed.clone(), legacy],
    )
    .expect_err("mixed committed and legacy decision identities must fail closed");
    assert!(error.to_string().contains("mix"), "{error}");

    let mut duplicate = benchmark_decision("trajectory-request-2");
    duplicate.ingress_request_id_sha256 = committed.ingress_request_id_sha256.clone();
    let error = WorkflowRunArtifact::validate_benchmark_integrity_with_decisions(
        &traces,
        &usage,
        &[committed, duplicate],
    )
    .expect_err("duplicate ingress commitments must fail closed");
    assert!(error.to_string().contains("duplicate"), "{error}");
}

#[test]
fn reward_feedback_joins_opaque_decisions_by_ingress_request_commitment() {
    let trace = benchmark_trace("br-bench-reward-1");
    let usage = computed_usage("br-bench-reward-1", "openai", "gpt-test", 10, 2, 30);
    let mut decision = benchmark_decision("trajectory-request-reward-opaque");
    decision.ingress_request_id_sha256 = Some(ingress_request_id_sha256("br-bench-reward-1"));
    let outcome = BenchmarkOutcomeRecord::new("episode-a", "terminal-bench/regex-log", 1.0)
        .with_request_id("br-bench-reward-1");

    WorkflowRunArtifact::validate_reward_feedback_integrity(
        &[trace],
        &[usage],
        &[outcome],
        &[decision],
    )
    .expect("reward feedback must use the same opaque ingress commitment join");
}

#[test]
fn benchmark_bundle_rejects_mismatched_or_duplicate_decision_request_ids() {
    let trace = benchmark_trace("req-1");
    let usage = computed_usage("req-1", "openai", "gpt-test", 10, 2, 30);
    let decision = benchmark_decision("req-1");

    let error = WorkflowRunArtifact::validate_benchmark_integrity_with_decisions(
        std::slice::from_ref(&trace),
        std::slice::from_ref(&usage),
        &[benchmark_decision("req-2")],
    )
    .expect_err("a decision for another request must fail closed");
    assert!(
        error
            .to_string()
            .contains("trace/decision request ids differ")
    );

    let error = WorkflowRunArtifact::validate_benchmark_integrity_with_decisions(
        &[trace],
        &[usage],
        &[decision.clone(), decision],
    )
    .expect_err("duplicate decisions must fail closed");
    assert!(
        error
            .to_string()
            .contains("duplicate policy decision request id")
    );
}

#[test]
fn replay_reports_coverage() {
    let fixtures = WorkflowTraceFixture::load_tree(fixture_root()).unwrap();
    let summary = ReplayEvaluator.run(&fixtures);
    assert!(summary.total >= 6);
    assert!(summary.coverage >= 0.80, "{summary:#?}");
}

#[test]
fn replay_keeps_observed_and_predictive_projections_separate_and_exact() {
    let fixtures = WorkflowTraceFixture::load_dir(fixture_root().join("predictive")).unwrap();

    let summary = ReplayEvaluator.run(&fixtures);

    assert_eq!(summary.predictive_expectation_count, 5);
    assert_eq!(summary.predictive_exact_count, 5);
    assert_eq!(summary.records.len(), 5);
    let records = summary
        .records
        .iter()
        .map(|record| {
            (
                record.fixture_id.as_str(),
                record.observed_route_key.clone(),
                record.predictive_route_key.clone(),
                record.next_action_class,
                record.prediction_matches_expected,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        records,
        vec![
            (
                "predictive-near-done-finalize-001",
                "agent_trace/v2|test|normal".to_string(),
                "agent_route/v1|finalize|normal".to_string(),
                NextActionClass::AnswerOrSummarize,
                Some(true),
            ),
            (
                "predictive-opening-plan-001",
                "agent_trace/v2|opening|normal".to_string(),
                "agent_route/v1|orchestrate|normal".to_string(),
                NextActionClass::ReasonOrPlan,
                Some(true),
            ),
            (
                "predictive-post-edit-verify-001",
                "agent_trace/v2|edit|normal".to_string(),
                "agent_route/v1|verify|normal".to_string(),
                NextActionClass::ExecuteOrTest,
                Some(true),
            ),
            (
                "predictive-post-read-implement-001",
                "agent_trace/v2|tool_followup|normal".to_string(),
                "agent_route/v1|implement|normal".to_string(),
                NextActionClass::Mutate,
                Some(true),
            ),
            (
                "predictive-repeated-failure-replan-001",
                "agent_trace/v2|test|guarded".to_string(),
                "agent_route/v1|orchestrate|guarded".to_string(),
                NextActionClass::ReasonOrPlan,
                Some(true),
            ),
        ]
    );
}

#[test]
fn old_fixture_without_prediction_remains_readable_and_replays_both_routes() {
    let fixture = WorkflowTraceFixture::from_value(json!({
        "id": "legacy-opening",
        "harness": "generic",
        "protocol": "chat_completions",
        "headers": {},
        "raw_body": {
            "model": "test",
            "messages": [{"role": "user", "content": "inspect this repository"}]
        },
        "expected": {
            "state_kind": "opening",
            "baseline_fingerprint": "opening",
            "confidence_min": 0.0
        }
    }))
    .unwrap();

    assert!(fixture.expected.prediction.is_none());
    let summary = ReplayEvaluator.run(&[fixture]);
    assert_eq!(summary.predictive_expectation_count, 0);
    assert_eq!(summary.predictive_exact_count, 0);
    assert_eq!(summary.records.len(), 1);
    assert_eq!(
        summary.records[0].observed_route_key.as_str(),
        "agent_trace/v2|opening|normal"
    );
    assert_eq!(
        summary.records[0].predictive_projection.next_step_role,
        NextStepRole::Orchestrate
    );
    assert_eq!(
        summary.records[0].predictive_route_key.as_str(),
        "agent_route/v1|orchestrate|normal"
    );
    assert_eq!(summary.records[0].prediction_matches_expected, None);
}

#[test]
fn new_fixture_prediction_is_compared_exactly() {
    let fixture = WorkflowTraceFixture::from_value(json!({
        "id": "new-mismatched-opening",
        "harness": "generic",
        "protocol": "chat_completions",
        "headers": {},
        "raw_body": {
            "model": "test",
            "messages": [{
                "role": "user",
                "content": "Investigate the repository architecture."
            }]
        },
        "expected": {
            "state_kind": "opening",
            "baseline_fingerprint": "opening",
            "confidence_min": 0.0,
            "prediction": {
                "next_step_role": "implement",
                "next_action_class": "mutate",
                "route_risk": "normal"
            }
        }
    }))
    .unwrap();

    let summary = ReplayEvaluator.run(&[fixture]);

    assert_eq!(summary.predictive_expectation_count, 1);
    assert_eq!(summary.predictive_exact_count, 0);
    assert_eq!(summary.records[0].prediction_matches_expected, Some(false));
    assert_eq!(
        summary.records[0].predictive_route_key,
        "agent_route/v1|orchestrate|normal"
    );
}

#[test]
fn evaluators_merge_equivalent_generic_and_native_terminus_projections() {
    let generic = WorkflowTraceFixture::from_value(json!({
        "id": "generic-opening",
        "harness": "generic",
        "protocol": "chat_completions",
        "headers": {},
        "raw_body": {
            "model": "test",
            "messages": [{"role": "user", "content": "inspect this repository"}]
        },
        "expected": {
            "state_kind": "opening",
            "baseline_fingerprint": "opening",
            "confidence_min": 0.0
        }
    }))
    .unwrap();
    let terminus = WorkflowTraceFixture::from_value(json!({
        "id": "terminus-opening",
        "harness": "terminus_2",
        "protocol": "chat_completions",
        "headers": {},
        "raw_body": {
            "model": "test",
            "messages": [{
                "role": "user",
                "content": "You are an AI assistant tasked with solving command-line tasks in a Linux environment. Format your response as JSON with analysis, plan, commands, and task_complete."
            }]
        },
        "expected": {
            "state_kind": "opening",
            "baseline_fingerprint": "opening",
            "confidence_min": 0.0
        }
    }))
    .unwrap();
    let fixtures = vec![generic, terminus];

    let replay = ReplayEvaluator.run(&fixtures);
    let shadow = ShadowPolicyEvaluator.run(&fixtures);

    assert_eq!(replay.ir_bucket_count, 1, "{replay:#?}");
    assert_eq!(
        shadow
            .decisions
            .iter()
            .map(|decision| decision.ir_key.as_str())
            .collect::<Vec<_>>(),
        vec!["agent_trace/v2|opening|normal"; 2]
    );
    assert_ne!(
        shadow.decisions[0].legacy_evidence_key, shadow.decisions[1].legacy_evidence_key,
        "detailed source evidence remains diagnostic instead of becoming a bucket"
    );
}

#[test]
fn replay_reports_baseline_vs_ir_collision_count() {
    let fixtures = WorkflowTraceFixture::load_tree(fixture_root()).unwrap();
    let summary = ReplayEvaluator.run(&fixtures);
    assert!(summary.baseline_bucket_count > 0);
    assert!(summary.ir_bucket_count >= summary.baseline_bucket_count);
    assert!(summary.collision_count <= summary.total);
}

#[test]
fn replay_reports_visibility_gaps_by_harness() {
    let fixtures = WorkflowTraceFixture::load_tree(fixture_root()).unwrap();
    let summary = ReplayEvaluator.run(&fixtures);
    assert!(summary.visibility_gap_count >= 1, "{summary:#?}");
    assert_eq!(summary.visibility_gaps_by_harness.get("codex"), Some(&1));
}

#[test]
fn ir_has_fewer_unknown_or_midstream_buckets_than_baseline_on_fixture_set() {
    let fixtures = WorkflowTraceFixture::load_tree(fixture_root()).unwrap();
    let summary = ReplayEvaluator.run(&fixtures);
    assert!(summary.baseline_midstream_count >= 1, "{summary:#?}");
    assert!(
        summary.ir_unknown_count < summary.baseline_midstream_count,
        "{summary:#?}"
    );
}

#[test]
fn workflow_constraints_report_model_ladder_compatibility() {
    let fixtures = WorkflowTraceFixture::load_tree(fixture_root()).unwrap();
    let summary = ReplayEvaluator.run(&fixtures);
    assert_eq!(summary.model_ladder.flagship, summary.total);
    assert!(summary.model_ladder.standard > 0, "{summary:#?}");
    assert!(summary.model_ladder.cheap_tool_safe > 0, "{summary:#?}");
    assert!(summary.model_ladder.cheap_fast > 0, "{summary:#?}");
}

#[test]
fn replay_summary_matches_current_experiment_fixture_set() {
    let fixtures = WorkflowTraceFixture::load_tree(fixture_root()).unwrap();
    for fixture in &fixtures {
        assert_eq!(
            fixture.expected.baseline_fingerprint,
            fixture.baseline_fingerprint(),
            "fixture {} has a stale baseline fingerprint",
            fixture.id
        );
    }
    let summary = ReplayEvaluator.run(&fixtures);
    assert_eq!(summary.total, 15, "{summary:#?}");
    assert_eq!(summary.covered, 15, "{summary:#?}");
    assert_eq!(summary.coverage, 1.0, "{summary:#?}");
    assert_eq!(summary.baseline_bucket_count, 5, "{summary:#?}");
    assert_eq!(summary.ir_bucket_count, 6, "{summary:#?}");
    assert_eq!(summary.collision_count, 0, "{summary:#?}");
    assert_eq!(summary.visibility_gap_count, 1, "{summary:#?}");
    assert_eq!(summary.baseline_midstream_count, 1, "{summary:#?}");
    assert_eq!(summary.ir_unknown_count, 0, "{summary:#?}");
    assert_eq!(summary.model_ladder.flagship, 15, "{summary:#?}");
    assert_eq!(summary.model_ladder.standard, 14, "{summary:#?}");
    assert_eq!(summary.model_ladder.cheap_tool_safe, 15, "{summary:#?}");
    assert_eq!(summary.model_ladder.cheap_fast, 7, "{summary:#?}");
}

#[test]
fn captured_real_agent_trace_serializes_to_replayable_fixture_and_redacts_secrets() {
    let trace = CapturedIngressTrace {
        id: "real-hermes-http-001".to_string(),
        captured_at: None,
        harness: HarnessId::Hermes,
        protocol: ProtocolKind::ChatCompletions,
        method: "POST".to_string(),
        path: "/v1/chat/completions".to_string(),
        headers: [
            ("authorization".to_string(), "Bearer brk_secret".to_string()),
            ("x-api-key".to_string(), "sk-secret".to_string()),
            ("user-agent".to_string(), "Hermes Agent v0.18.0".to_string()),
            (
                "x-bitrouter-workflow-session".to_string(),
                "session-real-1".to_string(),
            ),
            ("x-bitrouter-protocol".to_string(), "responses".to_string()),
            (
                "x-bitrouter-inbound-protocol".to_string(),
                "responses".to_string(),
            ),
        ]
        .into_iter()
        .collect(),
        raw_body: json!({
            "model": "openai/bitrouter-hermes-tbench",
            "messages": [{ "role": "user", "content": "reply ok" }],
            "tools": []
        }),
        outcome: RealTraceOutcome {
            http_status: 200,
            status: "completed".to_string(),
        },
    };

    let fixture_json = trace
        .to_replay_fixture_json(&TraceSanitizer::default())
        .expect("captured trace converts to fixture json");
    let headers = fixture_json["headers"].as_object().unwrap();
    assert!(!headers.contains_key("authorization"), "{fixture_json:#}");
    assert!(!headers.contains_key("x-api-key"), "{fixture_json:#}");
    assert_eq!(
        fixture_json["headers"]["user-agent"],
        "Hermes Agent v0.18.0"
    );
    assert_eq!(fixture_json["headers"]["x-bitrouter-protocol"], "responses");
    assert_eq!(
        fixture_json["headers"]["x-bitrouter-inbound-protocol"],
        "responses"
    );

    let fixture = WorkflowTraceFixture::from_value(fixture_json).unwrap();
    let summary = ReplayEvaluator.run(&[fixture]);
    assert_eq!(summary.total, 1, "{summary:#?}");
    assert_eq!(summary.covered, 1, "{summary:#?}");
    assert_eq!(summary.visibility_gap_count, 0, "{summary:#?}");
}

#[test]
fn trace_archive_round_trips_sanitized_jsonl_and_replay_fixtures() {
    let path = temp_path("trace-archive.jsonl");
    let traces = vec![CapturedIngressTrace {
        id: "trace-001".to_string(),
        captured_at: None,
        harness: HarnessId::Hermes,
        protocol: ProtocolKind::ChatCompletions,
        method: "POST".to_string(),
        path: "/v1/chat/completions".to_string(),
        headers: [
            ("authorization".to_string(), "Bearer brk_secret".to_string()),
            (
                "x-bitrouter-cloud-request-id".to_string(),
                "cloud-req-001".to_string(),
            ),
            (
                "x-bitrouter-workflow-session".to_string(),
                "session-a".to_string(),
            ),
        ]
        .into_iter()
        .collect(),
        raw_body: json!({
            "model": "openai/bitrouter-hermes-tbench",
            "messages": [{ "role": "user", "content": "reply ok" }],
            "tools": []
        }),
        outcome: RealTraceOutcome {
            http_status: 200,
            status: "completed".to_string(),
        },
    }];

    TraceArchive::write_jsonl(&path, &traces, &TraceSanitizer::default()).unwrap();
    let archived = TraceArchive::read_jsonl(&path).unwrap();
    let _ = std::fs::remove_file(&path);

    assert_eq!(archived.len(), 1);
    assert!(!archived[0].headers.contains_key("authorization"));
    assert_eq!(
        archived[0].headers.get("x-bitrouter-workflow-session"),
        Some(&"session-a".to_string())
    );
    assert_eq!(
        archived[0].headers.get("x-bitrouter-cloud-request-id"),
        Some(&"cloud-req-001".to_string())
    );

    let fixtures = TraceArchive::to_replay_fixtures(&archived).unwrap();
    let summary = ReplayEvaluator.run(&fixtures);
    assert_eq!(summary.total, 1, "{summary:#?}");
    assert_eq!(summary.covered, 1, "{summary:#?}");
}

#[tokio::test]
async fn real_trace_capture_writes_sanitized_trace_jsonl_to_archive_path() {
    let path = temp_path("daemon-traces.jsonl");
    let capture = RealTraceCapture::new(TraceCaptureOptions {
        harness: HarnessId::Hermes,
        archive_path: Some(path.clone()),
    });
    let router = axum::Router::new().route(
        "/v1/chat/completions",
        post(|| async { Json(json!({ "ok": true })) }),
    );
    let router = capture.router_wrapper()(router);
    let server = TestServer::new(router);

    let response = server
        .post("/v1/chat/completions")
        .add_header("authorization", "Bearer brk_secret")
        .add_header("x-bitrouter-workflow-session", "session-a")
        .json(&json!({
            "model": "openai/bitrouter-hermes-tbench",
            "messages": [{ "role": "user", "content": "reply ok" }],
            "tools": []
        }))
        .await;
    response.assert_status_ok();

    let archived = TraceArchive::read_jsonl(&path).unwrap();
    let _ = std::fs::remove_file(&path);

    assert_eq!(archived.len(), 1);
    assert!(
        capture.records().is_empty(),
        "archive-backed daemon capture must not retain full request bodies in an unbounded in-memory buffer"
    );
    assert_eq!(archived[0].harness, HarnessId::Hermes);
    assert!(!archived[0].headers.contains_key("authorization"));
    assert_eq!(
        archived[0].headers.get("x-bitrouter-workflow-session"),
        Some(&"session-a".to_string())
    );
    assert_eq!(archived[0].path, "/v1/chat/completions");
}

#[tokio::test]
async fn real_trace_capture_finishes_downstream_after_client_cancellation() {
    let path = temp_path("cancelled-daemon-traces.jsonl");
    let capture = RealTraceCapture::new(TraceCaptureOptions {
        harness: HarnessId::Terminus2,
        archive_path: Some(path.clone()),
    });
    let downstream_completed = Arc::new(AtomicBool::new(false));
    let completed = Arc::clone(&downstream_completed);
    let router = axum::Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let completed = Arc::clone(&completed);
            async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                completed.store(true, Ordering::SeqCst);
                Json(json!({ "ok": true }))
            }
        }),
    );
    let server = TestServer::new(capture.router_wrapper()(router));

    let request = server
        .post("/v1/chat/completions")
        .add_header("x-bitrouter-request-id", "cancelled-request-001")
        .add_header("x-bitrouter-trial-id", "trial-cancelled")
        .json(&json!({
            "model": "openai-codex:gpt-5.6-sol",
            "messages": [{ "role": "user", "content": "reply ok" }]
        }));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), request)
            .await
            .is_err()
    );

    tokio::time::timeout(Duration::from_secs(1), async {
        while !downstream_completed.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("downstream request should outlive the cancelled client future");

    let archived = TraceArchive::read_jsonl(&path).unwrap();
    let _ = std::fs::remove_file(&path);

    assert_eq!(archived.len(), 1);
    assert_eq!(archived[0].id, "cancelled-request-001");
    assert_eq!(archived[0].outcome.http_status, 200);
    assert_eq!(archived[0].outcome.status, "completed");
}

#[test]
fn cloud_usage_snapshot_jsonl_deduplicates_request_records() {
    let path = temp_path("cloud-usage.jsonl");
    std::fs::write(
        &path,
        [
            json!({
                "snapshot_at": "2026-07-07T00:00:00Z",
                "data": [{
                    "id": "usage-row-1",
                    "request_id": "cloud-req-001",
                    "provider_id": "bitrouter",
                    "model_id": "deepseek-v4-flash",
                    "prompt_tokens": 100,
                    "completion_tokens": 10,
                    "final_charge_micro_usd": null,
                    "status": "pending"
                }]
            })
            .to_string(),
            json!({
                "snapshot_at": "2026-07-07T00:00:10Z",
                "data": [{
                    "id": "usage-row-1",
                    "request_id": "cloud-req-001",
                    "provider_id": "bitrouter",
                    "model_id": "deepseek-v4-flash",
                    "prompt_tokens": 100,
                    "completion_tokens": 10,
                    "final_charge_micro_usd": 42,
                    "status": "succeeded"
                }]
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .unwrap();

    let records = CloudUsageRecord::load_snapshot_jsonl(&path).unwrap();
    let _ = std::fs::remove_file(&path);

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].request_id.as_deref(), Some("cloud-req-001"));
    assert_eq!(records[0].final_charge_micro_usd, Some(42));
    assert_eq!(records[0].status.as_deref(), Some("succeeded"));
}

#[test]
fn run_artifact_joins_trace_archive_with_cloud_usage_costs() {
    let traces = vec![
        CapturedIngressTrace {
            id: "cloud-req-001".to_string(),
            captured_at: None,
            harness: HarnessId::Hermes,
            protocol: ProtocolKind::ChatCompletions,
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            headers: [
                (
                    "x-bitrouter-cloud-request-id".to_string(),
                    "cloud-req-001".to_string(),
                ),
                (
                    "x-bitrouter-workflow-session".to_string(),
                    "session-a".to_string(),
                ),
            ]
            .into_iter()
            .collect(),
            raw_body: json!({
                "model": "openai/bitrouter-hermes-tbench",
                "messages": [{ "role": "user", "content": "reply ok" }],
                "tools": []
            }),
            outcome: RealTraceOutcome {
                http_status: 200,
                status: "completed".to_string(),
            },
        },
        CapturedIngressTrace {
            id: "cloud-req-002".to_string(),
            captured_at: None,
            harness: HarnessId::Hermes,
            protocol: ProtocolKind::ChatCompletions,
            method: "POST".to_string(),
            path: "/v1/chat/completions".to_string(),
            headers: [(
                "x-bitrouter-cloud-request-id".to_string(),
                "cloud-req-002".to_string(),
            )]
            .into_iter()
            .collect(),
            raw_body: json!({
                "model": "openai/bitrouter-hermes-tbench",
                "messages": [{ "role": "user", "content": "second" }],
                "tools": []
            }),
            outcome: RealTraceOutcome {
                http_status: 200,
                status: "completed".to_string(),
            },
        },
    ];
    let usage = vec![
        CloudUsageRecord {
            id: Some("usage-row-1".to_string()),
            request_id: Some("cloud-req-001".to_string()),
            provider_id: "bitrouter".to_string(),
            model_id: "deepseek-v4-flash".to_string(),
            prompt_tokens: 100,
            completion_tokens: 10,
            final_charge_micro_usd: Some(42),
            status: Some("succeeded".to_string()),
            ..Default::default()
        },
        CloudUsageRecord {
            id: Some("usage-row-2".to_string()),
            request_id: Some("cloud-req-extra".to_string()),
            provider_id: "moonshotai".to_string(),
            model_id: "kimi-k2.7-code".to_string(),
            prompt_tokens: 200,
            completion_tokens: 20,
            final_charge_micro_usd: Some(420),
            status: Some("succeeded".to_string()),
            ..Default::default()
        },
    ];

    let artifact = WorkflowRunArtifact::build("run-a", &traces, &usage).unwrap();
    assert_eq!(artifact.run_label, "run-a");
    assert_eq!(artifact.trace_count, 2);
    assert_eq!(artifact.replay.total, 2);
    assert_eq!(artifact.cost.request_count, 2);
    assert_eq!(artifact.cost.final_charge_micro_usd, 462);
    assert_eq!(
        artifact.cost.by_model_provider["bitrouter/deepseek-v4-flash"].request_count,
        1
    );
    assert_eq!(artifact.cost_join.matched_trace_count, 1);
    assert_eq!(artifact.cost_join.unmatched_trace_count, 1);
    assert_eq!(artifact.cost_join.unmatched_usage_count, 1);
}

#[test]
fn run_artifact_joins_trace_sessions_with_benchmark_outcomes() {
    let traces = vec![CapturedIngressTrace {
        id: "cloud-req-001".to_string(),
        captured_at: None,
        harness: HarnessId::Hermes,
        protocol: ProtocolKind::ChatCompletions,
        method: "POST".to_string(),
        path: "/v1/chat/completions".to_string(),
        headers: [(
            "x-bitrouter-workflow-session".to_string(),
            "session-a".to_string(),
        )]
        .into_iter()
        .collect(),
        raw_body: json!({
            "model": "openai/bitrouter-hermes-tbench",
            "messages": [{ "role": "user", "content": "reply ok" }],
            "tools": []
        }),
        outcome: RealTraceOutcome {
            http_status: 200,
            status: "completed".to_string(),
        },
    }];
    let outcomes = vec![BenchmarkOutcomeRecord {
        request_id: None,
        session_key: "session-a".to_string(),
        task_id: "filter-js-from-html".to_string(),
        reward: 0.0,
        failed_reason: Some("verifier_failed".to_string()),
        finished_at: None,
        trial_name: None,
        agent_started_at: None,
        agent_finished_at: None,
    }];

    let artifact =
        WorkflowRunArtifact::build_with_outcomes("run-a", &traces, &[], &outcomes).unwrap();

    assert_eq!(artifact.reward_join.matched_trace_count, 1);
    assert_eq!(artifact.reward_join.unmatched_outcome_count, 0);
    assert_eq!(artifact.semantic_inadequacy_candidates.len(), 1);
    assert_eq!(
        artifact.semantic_inadequacy_candidates[0].task_id,
        "filter-js-from-html"
    );
}

#[test]
fn terminus_subagent_reward_joins_by_explicit_trial_not_agent_session() {
    let mut trace = benchmark_trace("req-terminus-summary");
    trace.harness = HarnessId::Terminus2;
    trace.headers.extend([
        (
            "x-bitrouter-trial-id".to_string(),
            "regex-log__trial-a".to_string(),
        ),
        (
            "x-bitrouter-parent-session-id".to_string(),
            "root-session".to_string(),
        ),
        (
            "x-bitrouter-workflow-session".to_string(),
            "root-session-summarization-1-summary".to_string(),
        ),
    ]);
    let outcomes = vec![BenchmarkOutcomeRecord {
        request_id: None,
        session_key: "regex-log__trial-a".to_string(),
        task_id: "regex-log".to_string(),
        reward: 1.0,
        failed_reason: None,
        finished_at: None,
        trial_name: Some("regex-log__trial-a".to_string()),
        agent_started_at: None,
        agent_finished_at: None,
    }];

    let artifact =
        WorkflowRunArtifact::build_with_outcomes("run-a", &[trace], &[], &outcomes).unwrap();

    assert_eq!(artifact.reward_join.matched_trace_count, 1);
    assert_eq!(artifact.reward_join.unmatched_trace_count, 0);
    assert_eq!(artifact.reward_join.unmatched_outcome_count, 0);
}

#[test]
fn run_artifact_joins_trace_to_benchmark_outcome_by_agent_time_window() {
    let traces = vec![CapturedIngressTrace {
        id: "req-001".to_string(),
        captured_at: Some("2026-07-09T08:01:30Z".to_string()),
        harness: HarnessId::Codex,
        protocol: ProtocolKind::Responses,
        method: "POST".to_string(),
        path: "/v1/responses".to_string(),
        headers: [(
            "x-bitrouter-request-id".to_string(),
            "trace-001".to_string(),
        )]
        .into_iter()
        .collect(),
        raw_body: json!({
            "model": "gpt-5.5",
            "input": "solve the task",
            "stream": true
        }),
        outcome: RealTraceOutcome {
            http_status: 200,
            status: "completed".to_string(),
        },
    }];
    let outcomes = vec![BenchmarkOutcomeRecord {
        request_id: None,
        session_key: "regex-log__abc123".to_string(),
        task_id: "terminal-bench/regex-log".to_string(),
        reward: 0.0,
        failed_reason: Some("verifier_failed".to_string()),
        finished_at: Some("2026-07-09T08:05:00Z".to_string()),
        trial_name: Some("regex-log__abc123".to_string()),
        agent_started_at: Some("2026-07-09T08:00:00Z".to_string()),
        agent_finished_at: Some("2026-07-09T08:04:00Z".to_string()),
    }];

    let artifact =
        WorkflowRunArtifact::build_with_outcomes("run-a", &traces, &[], &outcomes).unwrap();

    assert_eq!(artifact.reward_join.matched_trace_count, 1);
    assert_eq!(artifact.reward_join.unmatched_outcome_count, 0);
    assert_eq!(artifact.semantic_inadequacy_candidates.len(), 1);
    assert_eq!(
        artifact.semantic_inadequacy_candidates[0].session_key,
        "regex-log__abc123"
    );
}

#[test]
fn complete_benchmark_integrity_rejects_time_only_reward_join() {
    let mut trace = benchmark_trace("req-time-only");
    trace.harness = HarnessId::Codex;
    trace.captured_at = Some("2026-07-09T08:01:30Z".to_string());
    let usage = computed_usage("req-time-only", "openai", "gpt-test", 10, 2, 30);
    let outcome = BenchmarkOutcomeRecord {
        request_id: None,
        session_key: "regex-log__abc123".to_string(),
        task_id: "terminal-bench/regex-log".to_string(),
        reward: 1.0,
        failed_reason: None,
        finished_at: Some("2026-07-09T08:05:00Z".to_string()),
        trial_name: Some("regex-log__abc123".to_string()),
        agent_started_at: Some("2026-07-09T08:00:00Z".to_string()),
        agent_finished_at: Some("2026-07-09T08:04:00Z".to_string()),
    };

    let error = WorkflowRunArtifact::validate_complete_benchmark_integrity(
        &[trace],
        &[usage],
        &[outcome],
        &[],
    )
    .expect_err("strict benchmark validation must not attribute reward by time alone");

    assert!(error.to_string().contains("outcome join incomplete"));
}

#[test]
fn reward_join_does_not_time_window_match_ambiguous_parallel_trials() {
    let traces = vec![CapturedIngressTrace {
        id: "req-001".to_string(),
        captured_at: Some("2026-07-09T08:01:30Z".to_string()),
        harness: HarnessId::Codex,
        protocol: ProtocolKind::Responses,
        method: "POST".to_string(),
        path: "/v1/responses".to_string(),
        headers: [(
            "x-bitrouter-request-id".to_string(),
            "trace-001".to_string(),
        )]
        .into_iter()
        .collect(),
        raw_body: json!({
            "model": "gpt-5.5",
            "input": "solve the task",
            "stream": true
        }),
        outcome: RealTraceOutcome {
            http_status: 200,
            status: "completed".to_string(),
        },
    }];
    let outcomes = vec![
        BenchmarkOutcomeRecord {
            request_id: None,
            session_key: "regex-log__abc123".to_string(),
            task_id: "terminal-bench/regex-log".to_string(),
            reward: 0.0,
            failed_reason: Some("verifier_failed".to_string()),
            finished_at: Some("2026-07-09T08:05:00Z".to_string()),
            trial_name: Some("regex-log__abc123".to_string()),
            agent_started_at: Some("2026-07-09T08:00:00Z".to_string()),
            agent_finished_at: Some("2026-07-09T08:04:00Z".to_string()),
        },
        BenchmarkOutcomeRecord {
            request_id: None,
            session_key: "fix-git__def456".to_string(),
            task_id: "terminal-bench/fix-git".to_string(),
            reward: 1.0,
            failed_reason: None,
            finished_at: Some("2026-07-09T08:05:10Z".to_string()),
            trial_name: Some("fix-git__def456".to_string()),
            agent_started_at: Some("2026-07-09T08:01:00Z".to_string()),
            agent_finished_at: Some("2026-07-09T08:04:30Z".to_string()),
        },
    ];

    let artifact =
        WorkflowRunArtifact::build_with_outcomes("run-a", &traces, &[], &outcomes).unwrap();

    assert_eq!(artifact.reward_join.matched_trace_count, 0);
    assert_eq!(artifact.reward_join.unmatched_trace_count, 1);
    assert_eq!(artifact.reward_join.unmatched_outcome_count, 2);
    assert!(artifact.semantic_inadequacy_candidates.is_empty());
}

#[test]
fn harbor_result_dir_exports_benchmark_outcomes_with_trial_windows() {
    let run_dir = temp_path("harbor-result-dir");
    let trial_dir = run_dir.join("regex-log__abc123");
    std::fs::create_dir_all(&trial_dir).unwrap();
    std::fs::write(
        run_dir.join("result.json"),
        json!({
            "id": "job-1",
            "n_total_trials": 1,
            "stats": {
                "evals": {
                    "codex__gpt-5.5__terminal-bench/terminal-bench-2-1": {
                        "reward_stats": { "reward": { "1.0": ["regex-log__abc123"] } }
                    }
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        trial_dir.join("result.json"),
        json!({
            "task_name": "terminal-bench/regex-log",
            "trial_name": "regex-log__abc123",
            "finished_at": "2026-07-09T08:05:00Z",
            "agent_execution": {
                "started_at": "2026-07-09T08:00:00Z",
                "finished_at": "2026-07-09T08:04:00Z"
            },
            "verifier_result": { "rewards": { "reward": 1.0 } },
            "exception_info": null
        })
        .to_string(),
    )
    .unwrap();

    let outcomes = BenchmarkOutcomeRecord::load_harbor_run_dir(&run_dir).unwrap();

    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].session_key, "regex-log__abc123");
    assert_eq!(outcomes[0].task_id, "terminal-bench/regex-log");
    assert_eq!(outcomes[0].reward, 1.0);
    assert_eq!(
        outcomes[0].agent_started_at.as_deref(),
        Some("2026-07-09T08:00:00Z")
    );
    assert_eq!(
        outcomes[0].agent_finished_at.as_deref(),
        Some("2026-07-09T08:04:00Z")
    );

    let _ = std::fs::remove_dir_all(&run_dir);
}

#[test]
fn harbor_result_dir_exports_outcomes_from_nested_case_jobs() {
    let run_dir = temp_path("harbor-nested-case-jobs");
    let job_dir = run_dir.join("case-01-job");
    let trial_dir = job_dir.join("regex-log__abc123");
    std::fs::create_dir_all(&trial_dir).unwrap();
    std::fs::write(
        job_dir.join("result.json"),
        json!({
            "id": "job-1",
            "n_total_trials": 1,
            "stats": {
                "evals": {
                    "codex__gpt-5.6-terra__terminal-bench/terminal-bench-2-1": {
                        "reward_stats": { "reward": { "1.0": ["regex-log__abc123"] } }
                    }
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        trial_dir.join("result.json"),
        json!({
            "task_name": "terminal-bench/regex-log",
            "trial_name": "regex-log__abc123",
            "finished_at": "2026-07-17T21:05:00Z",
            "agent_execution": {
                "started_at": "2026-07-17T21:00:00Z",
                "finished_at": "2026-07-17T21:04:00Z"
            },
            "verifier_result": { "rewards": { "reward": 1.0 } },
            "exception_info": null
        })
        .to_string(),
    )
    .unwrap();

    let outcomes = BenchmarkOutcomeRecord::load_harbor_run_dir(&run_dir).unwrap();

    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].session_key, "regex-log__abc123");
    assert_eq!(outcomes[0].task_id, "terminal-bench/regex-log");
    assert_eq!(outcomes[0].reward, 1.0);

    let _ = std::fs::remove_dir_all(&run_dir);
}

#[test]
fn benchmark_outcome_jsonl_reader_parses_records() {
    let path = temp_path("benchmark-outcomes.jsonl");
    std::fs::write(
        &path,
        json!({
            "session_key": "session-a",
            "task_id": "filter-js-from-html",
            "reward": 0.0,
            "failed_reason": "verifier_failed",
            "finished_at": "2026-07-08T00:00:00Z"
        })
        .to_string(),
    )
    .unwrap();

    let records = BenchmarkOutcomeRecord::load_jsonl(&path).unwrap();
    let _ = std::fs::remove_file(&path);

    assert_eq!(records.len(), 1);
    assert_eq!(records[0].session_key, "session-a");
    assert_eq!(records[0].reward, 0.0);
    assert_eq!(records[0].failed_reason.as_deref(), Some("verifier_failed"));
}

#[test]
fn run_artifact_embeds_offline_shadow_policy_summary() {
    let traces = vec![CapturedIngressTrace {
        id: "trace-001".to_string(),
        captured_at: None,
        harness: HarnessId::Hermes,
        protocol: ProtocolKind::ChatCompletions,
        method: "POST".to_string(),
        path: "/v1/chat/completions".to_string(),
        headers: [(
            "x-bitrouter-cloud-request-id".to_string(),
            "cloud-req-001".to_string(),
        )]
        .into_iter()
        .collect(),
        raw_body: json!({
            "model": "openai/bitrouter-hermes-tbench",
            "messages": [{ "role": "user", "content": "reply ok" }],
            "tools": []
        }),
        outcome: RealTraceOutcome {
            http_status: 200,
            status: "completed".to_string(),
        },
    }];
    let usage = vec![computed_usage(
        "cloud-req-001",
        "deepseek",
        "deepseek-v4-flash",
        100,
        10,
        42,
    )];

    let artifact = WorkflowRunArtifact::build("run-a", &traces, &usage).unwrap();

    assert_eq!(artifact.shadow_policy.total, 1);
    assert_eq!(
        artifact
            .shadow_policy
            .ir_route_counts
            .get(&TierName::CheapFast),
        Some(&1)
    );
    assert_eq!(artifact.shadow_policy.unsafe_cheap_fast_violations, 0);

    let value = serde_json::to_value(&artifact).unwrap();
    assert_eq!(value["shadow_policy"]["total"], 1);
    assert_eq!(value["shadow_policy"]["ir_route_counts"]["cheap_fast"], 1);
}

#[test]
fn run_artifact_bundle_writes_fixed_benchmark_layout() {
    let output_dir = temp_path("workflow-run-bundle");
    let traces = vec![CapturedIngressTrace {
        id: "cloud-req-001".to_string(),
        captured_at: None,
        harness: HarnessId::Hermes,
        protocol: ProtocolKind::ChatCompletions,
        method: "POST".to_string(),
        path: "/v1/chat/completions".to_string(),
        headers: [
            ("authorization".to_string(), "Bearer brk_secret".to_string()),
            (
                "x-bitrouter-cloud-request-id".to_string(),
                "cloud-req-001".to_string(),
            ),
        ]
        .into_iter()
        .collect(),
        raw_body: json!({
            "model": "openai/bitrouter-hermes-tbench",
            "messages": [{ "role": "user", "content": "reply ok" }],
            "tools": []
        }),
        outcome: RealTraceOutcome {
            http_status: 200,
            status: "completed".to_string(),
        },
    }];
    let usage = vec![computed_usage(
        "cloud-req-001",
        "deepseek",
        "deepseek-v4-flash",
        100,
        10,
        42,
    )];

    let artifact = WorkflowRunArtifact::write_bundle(
        "run-a",
        &output_dir,
        &traces,
        &usage,
        &TraceSanitizer::default(),
    )
    .unwrap();

    assert_eq!(artifact.run_label, "run-a");
    assert!(output_dir.join("traces.jsonl").exists());
    assert!(output_dir.join("cloud-usage.jsonl").exists());
    assert!(output_dir.join("benchmark-outcomes.jsonl").exists());
    assert!(output_dir.join("run-artifact.json").exists());
    assert!(output_dir.join("shadow-policy.json").exists());

    let archived = std::fs::read_to_string(output_dir.join("traces.jsonl")).unwrap();
    assert!(!archived.contains("brk_secret"), "{archived}");
    assert!(archived.contains("cloud-req-001"), "{archived}");

    let shadow_policy: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(output_dir.join("shadow-policy.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(shadow_policy["total"], 1);
    assert_eq!(shadow_policy["ir_route_counts"]["cheap_fast"], 1);

    let run_artifact: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(output_dir.join("run-artifact.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(run_artifact["shadow_policy"]["total"], 1);

    let _ = std::fs::remove_dir_all(&output_dir);
}

#[test]
fn run_artifact_bundle_includes_policy_decision_summary() {
    let output_dir = temp_path("workflow-run-bundle-decisions");
    let traces = vec![CapturedIngressTrace {
        id: "req-001".to_string(),
        captured_at: None,
        harness: HarnessId::Codex,
        protocol: ProtocolKind::Responses,
        method: "POST".to_string(),
        path: "/v1/responses".to_string(),
        headers: [("x-bitrouter-request-id".to_string(), "req-001".to_string())]
            .into_iter()
            .collect(),
        raw_body: json!({
            "model": "gpt-5.5",
            "input": "reply ok",
            "stream": true
        }),
        outcome: RealTraceOutcome {
            http_status: 200,
            status: "completed".to_string(),
        },
    }];
    let usage = vec![computed_usage(
        "req-001",
        "bitrouter",
        "moonshotai/kimi-k2.7-code",
        100,
        10,
        42,
    )];
    let decisions = vec![PolicyDecisionRecord {
        captured_at: None,
        request_id: Some("req-001".to_string()),
        ingress_request_id_sha256: None,
        input_model: "gpt-5.5".to_string(),
        key_strategy: "workflow_state".to_string(),
        request_key: "agent_trace/v1|tool_followup|normal".to_string(),
        ledger_key: None,
        policy: None,
        policy_digest: None,
        preset_variant: None,
        baseline_tier: Some("capable".to_string()),
        legacy_fingerprint: "after_bash".to_string(),
        workflow_state: "tool_followup".to_string(),
        workflow_identity: Default::default(),
        static_tier: Some("capable".to_string()),
        static_model: Some("openai-codex:gpt-5.5".to_string()),
        selected_tier: Some("cheap".to_string()),
        selected_model: Some("bitrouter:moonshotai/kimi-k2.7-code".to_string()),
        continuation_proposed_tier: None,
        continuation_proposed_model: None,
        continuation_adjustment: None,
        predicted_role: None,
        predicted_action: None,
        prediction_confidence_ppm: None,
        predictor_contract_digest: None,
        prediction_confidence_kind: None,
        prediction_reason_codes: Vec::new(),
        observed_route_projection: None,
        trajectory_episode_id: None,
        trajectory_sequence: None,
        trajectory_completeness: None,
        trajectory_health_digest: None,
        candidate_tier: None,
        progress_clause_ids: Vec::new(),
        reason: "exploration_locked".to_string(),
        pinned: false,
        request_qualified: true,
        semantic_successes: 2,
        semantic_success_threshold: 2,
        locked: true,
        trialed: false,
    }];

    let summary = PolicyDecisionSummary::from_records(&decisions);
    assert_eq!(summary.total, 1);
    assert_eq!(summary.by_selected_tier.get("cheap"), Some(&1));
    assert_eq!(summary.by_reason.get("exploration_locked"), Some(&1));

    let artifact = WorkflowRunArtifact::write_bundle_with_decisions(
        "run-a",
        &output_dir,
        &traces,
        &usage,
        &[],
        &decisions,
        &TraceSanitizer::default(),
    )
    .unwrap();

    assert_eq!(artifact.policy_decisions.total, 1);
    assert_eq!(
        artifact
            .policy_decisions
            .by_selected_model
            .get("bitrouter:moonshotai/kimi-k2.7-code"),
        Some(&1)
    );
    assert!(output_dir.join("policy-decisions.jsonl").exists());

    let run_artifact: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(output_dir.join("run-artifact.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(run_artifact["policy_decisions"]["total"], 1);
    assert_eq!(
        run_artifact["policy_decisions"]["by_reason"]["exploration_locked"],
        1
    );

    let _ = std::fs::remove_dir_all(&output_dir);
}

#[test]
fn policy_decision_summary_counts_static_to_selected_replacements() {
    let path = temp_path("policy-decision-transitions.jsonl");
    std::fs::write(
        &path,
        r#"{"captured_at":null,"request_id":"req-001","input_model":"gpt-5.5","key_strategy":"workflow_state","request_key":"codex|responses|tool_followup","legacy_fingerprint":"after_bash","workflow_state":"tool_followup","static_tier":"capable","static_model":"openai-codex:gpt-5.5","selected_tier":"cheap","selected_model":"bitrouter:moonshotai/kimi-k2.7-code","reason":"exploration_locked","pinned":false,"locked":true,"trialed":false}
"#,
    )
    .unwrap();
    let records = PolicyDecisionRecord::load_jsonl(&path).unwrap();

    let summary = PolicyDecisionSummary::from_records(&records);
    let value = serde_json::to_value(&summary).unwrap();

    assert_eq!(value["static_tier_replaced_count"], 1);
    assert_eq!(value["by_tier_transition"]["capable -> cheap"], 1);
    assert_eq!(value["static_model_replaced_count"], 1);
    assert_eq!(
        value["by_model_transition"]["openai-codex:gpt-5.5 -> bitrouter:moonshotai/kimi-k2.7-code"],
        1
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn legacy_source_specific_decisions_remain_legacy_while_new_agent_trace_decisions_are_canonical() {
    let old_path = temp_path("legacy-source-specific-policy-decision.jsonl");
    std::fs::write(
        &old_path,
        r#"{"captured_at":null,"request_id":"legacy-001","input_model":"gpt-5.5","key_strategy":"workflow_state","request_key":"codex|responses|tool_followup","legacy_fingerprint":"after_bash","workflow_state":"tool_followup","static_tier":"strong","static_model":"openai-codex:gpt-5.5","selected_tier":"economy","selected_model":"bitrouter:deepseek/deepseek-v4-pro","reason":"static_table","pinned":false,"locked":false,"trialed":false}
"#,
    )
    .unwrap();
    let legacy = PolicyDecisionRecord::load_jsonl(&old_path).unwrap();
    assert_eq!(legacy.len(), 1);
    assert_eq!(legacy[0].key_strategy, "workflow_state");
    assert_eq!(legacy[0].request_key, "codex|responses|tool_followup");
    assert_eq!(legacy[0].workflow_state, "tool_followup");

    let new_path = temp_path("canonical-agent-trace-policy-decision.jsonl");
    let mut canonical = benchmark_decision("agent-trace-001");
    canonical.key_strategy = "agent_trace".to_string();
    canonical.request_key = "agent_trace/v1|tool_followup|normal".to_string();
    PolicyDecisionRecord::write_jsonl(&new_path, &[canonical]).unwrap();
    let rendered = std::fs::read_to_string(&new_path).unwrap();
    let new_value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(new_value["key_strategy"], "agent_trace");
    assert_eq!(
        new_value["request_key"],
        "agent_trace/v1|tool_followup|normal"
    );
    assert_eq!(new_value["trace_state"], "opening");
    assert!(new_value.get("workflow_state").is_none());

    let _ = std::fs::remove_file(old_path);
    let _ = std::fs::remove_file(new_path);
}

#[tokio::test]
async fn legacy_workflow_state_lock_and_artifact_replay_without_projection_migration() {
    let root = temp_path("legacy-workflow-state-replay");
    std::fs::create_dir_all(&root).unwrap();
    let config_path = root.join("bitrouter.yaml");
    let lock_path = root.join("policy-lock.yaml");
    std::fs::write(
        &config_path,
        r#"
policy:
  path: "./policy-lock.yaml"
presets:
  legacy:
    model: "vendor/strong"
    policy: legacy
"#,
    )
    .unwrap();
    std::fs::write(
        &lock_path,
        r#"
lockfileVersion: 1
policies:
  legacy:
    key_strategy: workflow_state
    tiers:
      economy: "vendor/economy"
      strong: "vendor/strong"
    routes:
      "codex|responses|tool_followup": economy
    default_tier: strong
    tool_use_tier: strong
    tool_safe_tiers:
      - strong
      - economy
"#,
    )
    .unwrap();
    let config =
        config::parse_with(&std::fs::read_to_string(&config_path).unwrap(), |_| None).unwrap();
    let loaded = policy_lock::load_for_config(&config, Some(&config_path))
        .await
        .unwrap()
        .expect("legacy bound lock loads");
    let definition = loaded.document.policies.get("legacy").unwrap();
    assert_eq!(
        definition
            .as_table_config(bitrouter_sdk::config::PolicyRuntimeMode::Frozen)
            .key_strategy,
        bitrouter_sdk::config::PolicyKeyStrategy::AgentTrace
    );
    assert!(
        definition
            .routes
            .contains_key("codex|responses|tool_followup"),
        "legacy source-specific route remains legacy evidence"
    );

    let trace_path = root.join("traces.jsonl");
    let legacy_trace = CapturedIngressTrace {
        id: "legacy-request".to_string(),
        captured_at: None,
        harness: HarnessId::Codex,
        protocol: ProtocolKind::Responses,
        method: "POST".to_string(),
        path: "/v1/responses".to_string(),
        headers: Default::default(),
        raw_body: json!({
            "model": "vendor/strong",
            "previous_response_id": "resp_legacy",
            "input": "continue"
        }),
        outcome: RealTraceOutcome {
            http_status: 200,
            status: "completed".to_string(),
        },
    };
    TraceArchive::write_jsonl(&trace_path, &[legacy_trace], &TraceSanitizer::default()).unwrap();
    let fixtures = TraceArchive::read_replay_fixtures(&trace_path).unwrap();
    let replay = ReplayEvaluator.run(&fixtures);
    assert_eq!(replay.total, 1);
    assert_eq!(replay.covered, 1);

    let decision_path = root.join("legacy-decisions.jsonl");
    let mut legacy_decision = benchmark_decision("legacy-request");
    legacy_decision.key_strategy = "workflow_state".to_string();
    legacy_decision.request_key = "codex|responses|tool_followup".to_string();
    PolicyDecisionRecord::write_jsonl(&decision_path, &[legacy_decision]).unwrap();
    let legacy_records = PolicyDecisionRecord::load_jsonl(&decision_path).unwrap();
    assert_eq!(
        legacy_records[0].request_key,
        "codex|responses|tool_followup"
    );
    assert!(!legacy_records[0].request_key.starts_with("agent_trace/"));

    let canonical_path = root.join("agent-trace-decisions.jsonl");
    let mut canonical = benchmark_decision("agent-trace-request");
    canonical.key_strategy = "agent_trace".to_string();
    canonical.request_key = "agent_trace/v1|tool_followup|normal".to_string();
    PolicyDecisionRecord::write_jsonl(&canonical_path, &[canonical]).unwrap();
    let canonical_records = PolicyDecisionRecord::load_jsonl(&canonical_path).unwrap();
    assert_eq!(
        canonical_records[0].request_key,
        "agent_trace/v1|tool_followup|normal"
    );

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn run_artifact_attributes_failed_task_to_policy_transition() {
    let output_dir = temp_path("workflow-run-bundle-semantic-policy-transition");
    let traces = vec![CapturedIngressTrace {
        id: "req-001".to_string(),
        captured_at: None,
        harness: HarnessId::Codex,
        protocol: ProtocolKind::Responses,
        method: "POST".to_string(),
        path: "/v1/responses".to_string(),
        headers: [
            ("x-bitrouter-request-id".to_string(), "req-001".to_string()),
            (
                "x-bitrouter-workflow-session".to_string(),
                "trial-a".to_string(),
            ),
        ]
        .into_iter()
        .collect(),
        raw_body: json!({
            "model": "gpt-5.5",
            "input": "continue",
            "stream": true
        }),
        outcome: RealTraceOutcome {
            http_status: 200,
            status: "completed".to_string(),
        },
    }];
    let outcomes = vec![BenchmarkOutcomeRecord {
        request_id: Some("req-001".to_string()),
        session_key: "trial-a".to_string(),
        task_id: "filter-js-from-html".to_string(),
        reward: 0.0,
        failed_reason: Some("verifier_failed".to_string()),
        finished_at: None,
        trial_name: Some("trial-a".to_string()),
        agent_started_at: None,
        agent_finished_at: None,
    }];
    let decisions = vec![PolicyDecisionRecord {
        captured_at: None,
        request_id: Some("req-001".to_string()),
        ingress_request_id_sha256: None,
        input_model: "gpt-5.5".to_string(),
        key_strategy: "workflow_state".to_string(),
        request_key: "agent_trace/v1|tool_followup|normal".to_string(),
        ledger_key: None,
        policy: None,
        policy_digest: None,
        preset_variant: None,
        baseline_tier: Some("capable".to_string()),
        legacy_fingerprint: "after_bash".to_string(),
        workflow_state: "tool_followup".to_string(),
        workflow_identity: Default::default(),
        static_tier: Some("capable".to_string()),
        static_model: Some("openai-codex:gpt-5.5".to_string()),
        selected_tier: Some("cheap".to_string()),
        selected_model: Some("bitrouter:moonshotai/kimi-k2.7-code".to_string()),
        continuation_proposed_tier: None,
        continuation_proposed_model: None,
        continuation_adjustment: None,
        predicted_role: None,
        predicted_action: None,
        prediction_confidence_ppm: None,
        predictor_contract_digest: None,
        prediction_confidence_kind: None,
        prediction_reason_codes: Vec::new(),
        observed_route_projection: None,
        trajectory_episode_id: None,
        trajectory_sequence: None,
        trajectory_completeness: None,
        trajectory_health_digest: None,
        candidate_tier: None,
        progress_clause_ids: Vec::new(),
        reason: "exploration_locked".to_string(),
        pinned: false,
        request_qualified: true,
        semantic_successes: 2,
        semantic_success_threshold: 2,
        locked: true,
        trialed: false,
    }];
    let usage = vec![computed_usage(
        "req-001",
        "bitrouter",
        "moonshotai/kimi-k2.7-code",
        10,
        2,
        30,
    )];

    let artifact = WorkflowRunArtifact::write_bundle_with_decisions(
        "run-a",
        &output_dir,
        &traces,
        &usage,
        &outcomes,
        &decisions,
        &TraceSanitizer::default(),
    )
    .unwrap();
    let value = serde_json::to_value(&artifact).unwrap();

    assert_eq!(
        value["semantic_policy_transition_candidates"][0]["task_id"],
        "filter-js-from-html"
    );
    assert_eq!(
        value["semantic_policy_transition_candidates"][0]["request_id"],
        "req-001"
    );
    assert_eq!(
        value["semantic_policy_transition_candidates"][0]["tier_transition"],
        "capable -> cheap"
    );
    assert_eq!(
        value["semantic_policy_transition_candidates"][0]["model_transition"],
        "openai-codex:gpt-5.5 -> bitrouter:moonshotai/kimi-k2.7-code"
    );

    let _ = std::fs::remove_dir_all(&output_dir);
}

#[test]
fn run_artifact_attributes_successful_task_to_policy_transition() {
    let traces = vec![CapturedIngressTrace {
        id: "req-success-001".to_string(),
        captured_at: None,
        harness: HarnessId::Codex,
        protocol: ProtocolKind::Responses,
        method: "POST".to_string(),
        path: "/v1/responses".to_string(),
        headers: [
            (
                "x-bitrouter-request-id".to_string(),
                "req-success-001".to_string(),
            ),
            (
                "x-bitrouter-workflow-session".to_string(),
                "trial-success-a".to_string(),
            ),
        ]
        .into_iter()
        .collect(),
        raw_body: json!({
            "model": "gpt-5.5",
            "input": "continue",
            "stream": true
        }),
        outcome: RealTraceOutcome {
            http_status: 200,
            status: "completed".to_string(),
        },
    }];
    let outcomes = vec![BenchmarkOutcomeRecord {
        request_id: Some("req-success-001".to_string()),
        session_key: "trial-success-a".to_string(),
        task_id: "terminal-bench/regex-log".to_string(),
        reward: 1.0,
        failed_reason: None,
        finished_at: None,
        trial_name: Some("trial-success-a".to_string()),
        agent_started_at: None,
        agent_finished_at: None,
    }];
    let decisions = vec![PolicyDecisionRecord {
        captured_at: None,
        request_id: Some("req-success-001".to_string()),
        ingress_request_id_sha256: None,
        input_model: "gpt-5.5".to_string(),
        key_strategy: "workflow_state".to_string(),
        request_key: "agent_trace/v1|tool_followup|normal".to_string(),
        ledger_key: Some("coding\0agent_trace/v1|tool_followup|normal".to_string()),
        policy: Some("coding".to_string()),
        policy_digest: Some(
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
        ),
        preset_variant: Some("coding".to_string()),
        baseline_tier: Some("capable".to_string()),
        legacy_fingerprint: "after_exec_command".to_string(),
        workflow_state: "tool_followup".to_string(),
        workflow_identity: Default::default(),
        static_tier: Some("capable".to_string()),
        static_model: Some("openai-codex:gpt-5.5".to_string()),
        selected_tier: Some("cheap".to_string()),
        selected_model: Some("bitrouter:moonshotai/kimi-k2.7-code".to_string()),
        continuation_proposed_tier: None,
        continuation_proposed_model: None,
        continuation_adjustment: None,
        predicted_role: None,
        predicted_action: None,
        prediction_confidence_ppm: None,
        predictor_contract_digest: None,
        prediction_confidence_kind: None,
        prediction_reason_codes: Vec::new(),
        observed_route_projection: None,
        trajectory_episode_id: None,
        trajectory_sequence: None,
        trajectory_completeness: None,
        trajectory_health_digest: None,
        candidate_tier: None,
        progress_clause_ids: Vec::new(),
        reason: "exploration_trial".to_string(),
        pinned: false,
        request_qualified: false,
        semantic_successes: 0,
        semantic_success_threshold: 2,
        locked: false,
        trialed: true,
    }];

    let mut usage = computed_usage(
        "req-success-001",
        "bitrouter",
        "moonshotai/kimi-k2.7-code",
        10,
        2,
        30,
    );
    usage.status = Some("completed".to_string());
    let artifact = WorkflowRunArtifact::build_with_decisions(
        "successful-transition",
        &traces,
        &[usage],
        &outcomes,
        &decisions,
    )
    .unwrap();

    assert_eq!(artifact.semantic_policy_transition_candidates.len(), 1);
    let candidate = &artifact.semantic_policy_transition_candidates[0];
    assert_eq!(candidate.task_id, "terminal-bench/regex-log");
    assert_eq!(candidate.reward, 1.0);
    assert_eq!(
        candidate.request_transport_outcome,
        RequestTransportOutcome::Completed
    );
    assert_eq!(
        candidate.settlement_outcome,
        SemanticSettlementOutcome::ProviderReportedComputed
    );
    assert_eq!(candidate.request_key, "agent_trace/v1|tool_followup|normal");
    assert_eq!(
        candidate.ledger_key.as_deref(),
        Some("coding\0agent_trace/v1|tool_followup|normal")
    );
    assert_eq!(
        candidate.tier_transition.as_deref(),
        Some("capable -> cheap")
    );
}

#[test]
fn run_artifact_bundle_writes_benchmark_outcomes_and_reward_join() {
    let output_dir = temp_path("workflow-run-bundle-outcomes");
    let traces = vec![CapturedIngressTrace {
        id: "req-outcome-a".to_string(),
        captured_at: None,
        harness: HarnessId::Hermes,
        protocol: ProtocolKind::ChatCompletions,
        method: "POST".to_string(),
        path: "/v1/chat/completions".to_string(),
        headers: [
            (
                "x-bitrouter-request-id".to_string(),
                "req-outcome-a".to_string(),
            ),
            (
                "x-bitrouter-workflow-session".to_string(),
                "session-a".to_string(),
            ),
        ]
        .into_iter()
        .collect(),
        raw_body: json!({
            "model": "openai/bitrouter-hermes-tbench",
            "messages": [{ "role": "user", "content": "reply ok" }],
            "tools": []
        }),
        outcome: RealTraceOutcome {
            http_status: 200,
            status: "completed".to_string(),
        },
    }];
    let outcomes = vec![BenchmarkOutcomeRecord {
        request_id: Some("req-outcome-a".to_string()),
        session_key: "session-a".to_string(),
        task_id: "filter-js-from-html".to_string(),
        reward: 0.0,
        failed_reason: Some("verifier_failed".to_string()),
        finished_at: None,
        trial_name: None,
        agent_started_at: None,
        agent_finished_at: None,
    }];

    let artifact = WorkflowRunArtifact::write_bundle_with_outcomes(
        "run-a",
        &output_dir,
        &traces,
        &[computed_usage(
            "req-outcome-a",
            "openai",
            "gpt-test",
            10,
            2,
            30,
        )],
        &outcomes,
        &TraceSanitizer::default(),
    )
    .unwrap();

    assert_eq!(artifact.reward_join.matched_trace_count, 1);
    assert!(output_dir.join("benchmark-outcomes.jsonl").exists());
    let run_artifact: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(output_dir.join("run-artifact.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(run_artifact["reward_join"]["matched_trace_count"], 1);
    assert_eq!(
        run_artifact["semantic_inadequacy_candidates"][0]["task_id"],
        "filter-js-from-html"
    );

    let _ = std::fs::remove_dir_all(&output_dir);
}

#[test]
fn benchmark_bundle_rejects_unmatched_outcomes_before_writing() {
    let output_dir = temp_path("workflow-run-bundle-unmatched-outcomes");
    let mut trace = benchmark_trace("req-1");
    trace
        .headers
        .insert("x-bitrouter-trial-id".to_string(), "trial-a".to_string());
    let outcomes = vec![BenchmarkOutcomeRecord {
        request_id: Some("req-2".to_string()),
        session_key: "trial-b".to_string(),
        task_id: "filter-js-from-html".to_string(),
        reward: 0.0,
        failed_reason: Some("verifier_failed".to_string()),
        finished_at: None,
        trial_name: Some("trial-b".to_string()),
        agent_started_at: None,
        agent_finished_at: None,
    }];

    let error = WorkflowRunArtifact::write_bundle_with_outcomes(
        "run-a",
        &output_dir,
        &[trace],
        &[computed_usage("req-1", "openai", "gpt-test", 10, 2, 30)],
        &outcomes,
        &TraceSanitizer::default(),
    )
    .expect_err("unmatched outcome must reject benchmark bundle");

    assert!(error.to_string().contains("outcome join"));
    assert!(!output_dir.exists());
}

#[test]
fn shadow_policy_compares_baseline_fingerprints_to_ir_model_ladder() {
    let fixtures = WorkflowTraceFixture::load_tree(fixture_root()).unwrap();
    let summary = ShadowPolicyEvaluator.run(&fixtures);
    assert_eq!(summary.total, fixtures.len());
    assert!(summary.changed_count > 0, "{summary:#?}");
    assert_eq!(summary.unsafe_cheap_fast_violations, 0, "{summary:#?}");
    assert!(
        summary
            .ir_route_counts
            .get(&TierName::CheapFast)
            .copied()
            .unwrap_or(0)
            > 0,
        "{summary:#?}"
    );

    let tool_followup = summary
        .decisions
        .iter()
        .find(|decision| decision.fixture_id == "hermes-tool-followup-001")
        .expect("tool follow-up fixture has a shadow decision");
    assert_eq!(tool_followup.baseline_key, "after_bash");
    assert_eq!(tool_followup.ir_state_kind.to_string(), "test");
    assert_eq!(tool_followup.ir_tier, TierName::CheapToolSafe);
}
