use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::routing::post;
use axum_test::TestServer;
use bitrouter::workflow_state::archive::{CloudUsageRecord, TraceArchive, WorkflowRunArtifact};
use bitrouter::workflow_state::decision::{PolicyDecisionRecord, PolicyDecisionSummary};
use bitrouter::workflow_state::fixture::WorkflowTraceFixture;
use bitrouter::workflow_state::ir::{HarnessId, ProtocolKind};
use bitrouter::workflow_state::real_trace::{
    CapturedIngressTrace, RealTraceCapture, RealTraceOutcome, TraceCaptureOptions, TraceSanitizer,
};
use bitrouter::workflow_state::replay::ReplayEvaluator;
use bitrouter::workflow_state::reward::BenchmarkOutcomeRecord;
use bitrouter::workflow_state::shadow_policy::{ShadowPolicyEvaluator, TierName};
use serde_json::json;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/workflow_state/hermes")
        .join(name)
}

#[test]
fn loads_workflow_trace_fixture() {
    let fixture = WorkflowTraceFixture::load_file(&fixture_path("opening.json")).unwrap();
    assert_eq!(fixture.id, "hermes-opening-001");
    assert_eq!(fixture.expected.state_kind.to_string(), "opening");
    assert_eq!(fixture.prompt.model, "bitrouter-mvp-alias");
}

#[test]
fn fixture_exposes_policy_table_baseline_fingerprint() {
    let fixture = WorkflowTraceFixture::load_file(&fixture_path("tool_followup.json")).unwrap();
    assert_eq!(fixture.baseline_fingerprint(), "after_bash");
    assert_eq!(fixture.expected.baseline_fingerprint, "after_bash");
}

#[test]
fn loads_runtime_fixture_with_canonical_prompt_fallback() {
    let fixture = WorkflowTraceFixture::load_file(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
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

#[test]
fn replay_reports_coverage() {
    let fixtures = WorkflowTraceFixture::load_tree(&fixture_root()).unwrap();
    let summary = ReplayEvaluator::default().run(&fixtures);
    assert!(summary.total >= 6);
    assert!(summary.coverage >= 0.80, "{summary:#?}");
}

#[test]
fn replay_reports_baseline_vs_ir_collision_count() {
    let fixtures = WorkflowTraceFixture::load_tree(&fixture_root()).unwrap();
    let summary = ReplayEvaluator::default().run(&fixtures);
    assert!(summary.baseline_bucket_count > 0);
    assert!(summary.ir_bucket_count >= summary.baseline_bucket_count);
    assert!(summary.collision_count <= summary.total);
}

#[test]
fn replay_reports_visibility_gaps_by_harness() {
    let fixtures = WorkflowTraceFixture::load_tree(&fixture_root()).unwrap();
    let summary = ReplayEvaluator::default().run(&fixtures);
    assert!(summary.visibility_gap_count >= 1, "{summary:#?}");
    assert_eq!(summary.visibility_gaps_by_harness.get("codex"), Some(&1));
}

#[test]
fn ir_has_fewer_unknown_or_midstream_buckets_than_baseline_on_fixture_set() {
    let fixtures = WorkflowTraceFixture::load_tree(&fixture_root()).unwrap();
    let summary = ReplayEvaluator::default().run(&fixtures);
    assert!(summary.baseline_midstream_count >= 1, "{summary:#?}");
    assert!(
        summary.ir_unknown_count < summary.baseline_midstream_count,
        "{summary:#?}"
    );
}

#[test]
fn workflow_constraints_report_model_ladder_compatibility() {
    let fixtures = WorkflowTraceFixture::load_tree(&fixture_root()).unwrap();
    let summary = ReplayEvaluator::default().run(&fixtures);
    assert_eq!(summary.model_ladder.flagship, summary.total);
    assert!(summary.model_ladder.standard > 0, "{summary:#?}");
    assert!(summary.model_ladder.cheap_tool_safe > 0, "{summary:#?}");
    assert!(summary.model_ladder.cheap_fast > 0, "{summary:#?}");
}

#[test]
fn replay_summary_matches_current_experiment_fixture_set() {
    let fixtures = WorkflowTraceFixture::load_tree(&fixture_root()).unwrap();
    let summary = ReplayEvaluator::default().run(&fixtures);
    assert_eq!(summary.total, 7, "{summary:#?}");
    assert_eq!(summary.covered, 7, "{summary:#?}");
    assert_eq!(summary.coverage, 1.0, "{summary:#?}");
    assert_eq!(summary.baseline_bucket_count, 3, "{summary:#?}");
    assert_eq!(summary.ir_bucket_count, 6, "{summary:#?}");
    assert_eq!(summary.collision_count, 0, "{summary:#?}");
    assert_eq!(summary.visibility_gap_count, 1, "{summary:#?}");
    assert_eq!(summary.baseline_midstream_count, 1, "{summary:#?}");
    assert_eq!(summary.ir_unknown_count, 0, "{summary:#?}");
    assert_eq!(summary.model_ladder.flagship, 7, "{summary:#?}");
    assert_eq!(summary.model_ladder.standard, 7, "{summary:#?}");
    assert_eq!(summary.model_ladder.cheap_tool_safe, 7, "{summary:#?}");
    assert_eq!(summary.model_ladder.cheap_fast, 6, "{summary:#?}");
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
    let summary = ReplayEvaluator::default().run(&[fixture]);
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
    let summary = ReplayEvaluator::default().run(&fixtures);
    assert_eq!(summary.total, 1, "{summary:#?}");
    assert_eq!(summary.covered, 1, "{summary:#?}");
}

#[tokio::test]
async fn real_trace_capture_writes_sanitized_trace_jsonl_to_archive_path() {
    let path = temp_path("daemon-traces.jsonl");
    let capture = RealTraceCapture::new(TraceCaptureOptions {
        harness: HarnessId::Hermes,
        session_header: Some("x-bitrouter-workflow-session".to_string()),
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
    assert_eq!(archived[0].harness, HarnessId::Hermes);
    assert!(!archived[0].headers.contains_key("authorization"));
    assert_eq!(
        archived[0].headers.get("x-bitrouter-workflow-session"),
        Some(&"session-a".to_string())
    );
    assert_eq!(archived[0].path, "/v1/chat/completions");
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
            id: "trace-001".to_string(),
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
            id: "trace-002".to_string(),
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
        id: "trace-001".to_string(),
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
fn run_artifact_joins_trace_to_benchmark_outcome_by_agent_time_window() {
    let traces = vec![CapturedIngressTrace {
        id: "trace-001".to_string(),
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
fn reward_join_does_not_time_window_match_ambiguous_parallel_trials() {
    let traces = vec![CapturedIngressTrace {
        id: "trace-001".to_string(),
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
    let usage = vec![CloudUsageRecord {
        id: Some("usage-row-1".to_string()),
        request_id: Some("cloud-req-001".to_string()),
        provider_id: "deepseek".to_string(),
        model_id: "deepseek-v4-flash".to_string(),
        prompt_tokens: 100,
        completion_tokens: 10,
        final_charge_micro_usd: Some(42),
        status: Some("succeeded".to_string()),
    }];

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
    let usage = vec![CloudUsageRecord {
        id: Some("usage-row-1".to_string()),
        request_id: Some("cloud-req-001".to_string()),
        provider_id: "deepseek".to_string(),
        model_id: "deepseek-v4-flash".to_string(),
        prompt_tokens: 100,
        completion_tokens: 10,
        final_charge_micro_usd: Some(42),
        status: Some("succeeded".to_string()),
    }];

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
        id: "trace-001".to_string(),
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
    let usage = vec![CloudUsageRecord {
        id: Some("usage-row-1".to_string()),
        request_id: Some("req-001".to_string()),
        provider_id: "bitrouter".to_string(),
        model_id: "moonshotai/kimi-k2.7-code".to_string(),
        prompt_tokens: 100,
        completion_tokens: 10,
        final_charge_micro_usd: Some(42),
        status: Some("succeeded".to_string()),
    }];
    let decisions = vec![PolicyDecisionRecord {
        captured_at: None,
        request_id: Some("req-001".to_string()),
        input_model: "gpt-5.5".to_string(),
        key_strategy: "workflow_state".to_string(),
        request_key: "codex|responses|tool_followup|-|-|bash|low|small|none|high|low|low|low|medium|medium|requires_structured_tools".to_string(),
        legacy_fingerprint: "after_bash".to_string(),
        workflow_state: "tool_followup".to_string(),
        static_tier: Some("capable".to_string()),
        static_model: Some("openai-codex:gpt-5.5".to_string()),
        selected_tier: Some("cheap".to_string()),
        selected_model: Some("bitrouter:moonshotai/kimi-k2.7-code".to_string()),
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
fn run_artifact_attributes_failed_task_to_policy_transition() {
    let output_dir = temp_path("workflow-run-bundle-semantic-policy-transition");
    let traces = vec![CapturedIngressTrace {
        id: "trace-001".to_string(),
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
        input_model: "gpt-5.5".to_string(),
        key_strategy: "workflow_state".to_string(),
        request_key: "codex|responses|tool_followup".to_string(),
        legacy_fingerprint: "after_bash".to_string(),
        workflow_state: "tool_followup".to_string(),
        static_tier: Some("capable".to_string()),
        static_model: Some("openai-codex:gpt-5.5".to_string()),
        selected_tier: Some("cheap".to_string()),
        selected_model: Some("bitrouter:moonshotai/kimi-k2.7-code".to_string()),
        reason: "exploration_locked".to_string(),
        pinned: false,
        request_qualified: true,
        semantic_successes: 2,
        semantic_success_threshold: 2,
        locked: true,
        trialed: false,
    }];

    let artifact = WorkflowRunArtifact::write_bundle_with_decisions(
        "run-a",
        &output_dir,
        &traces,
        &[],
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
        id: "trace-success-001".to_string(),
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
        input_model: "gpt-5.5".to_string(),
        key_strategy: "workflow_state".to_string(),
        request_key: "codex|responses|tool_followup".to_string(),
        legacy_fingerprint: "after_exec_command".to_string(),
        workflow_state: "tool_followup".to_string(),
        static_tier: Some("capable".to_string()),
        static_model: Some("openai-codex:gpt-5.5".to_string()),
        selected_tier: Some("cheap".to_string()),
        selected_model: Some("bitrouter:moonshotai/kimi-k2.7-code".to_string()),
        reason: "exploration_trial".to_string(),
        pinned: false,
        request_qualified: false,
        semantic_successes: 0,
        semantic_success_threshold: 2,
        locked: false,
        trialed: true,
    }];

    let artifact = WorkflowRunArtifact::build_with_decisions(
        "successful-transition",
        &traces,
        &[],
        &outcomes,
        &decisions,
    )
    .unwrap();

    assert_eq!(artifact.semantic_policy_transition_candidates.len(), 1);
    let candidate = &artifact.semantic_policy_transition_candidates[0];
    assert_eq!(candidate.task_id, "terminal-bench/regex-log");
    assert_eq!(candidate.reward, 1.0);
    assert_eq!(candidate.request_key, "codex|responses|tool_followup");
    assert_eq!(
        candidate.tier_transition.as_deref(),
        Some("capable -> cheap")
    );
}

#[test]
fn run_artifact_bundle_writes_benchmark_outcomes_and_reward_join() {
    let output_dir = temp_path("workflow-run-bundle-outcomes");
    let traces = vec![CapturedIngressTrace {
        id: "trace-001".to_string(),
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
        &[],
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
fn shadow_policy_compares_baseline_fingerprints_to_ir_model_ladder() {
    let fixtures = WorkflowTraceFixture::load_tree(&fixture_root()).unwrap();
    let summary = ShadowPolicyEvaluator::default().run(&fixtures);
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
    assert_eq!(tool_followup.ir_state_kind.to_string(), "tool_followup");
    assert_eq!(tool_followup.ir_tier, TierName::CheapToolSafe);
}
