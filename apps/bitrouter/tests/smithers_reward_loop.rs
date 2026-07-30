use std::collections::BTreeMap;

use bitrouter::eval::EvalService;
use bitrouter::eval::compiler::EvalEvidenceSnapshot;
use bitrouter::eval::store::EvalStore;
use bitrouter::metering::{
    ChargeEvidence, ChargeStatus, EffectivePricingRates, PricingSource, ReconciliationStatus,
};
use bitrouter::policy_compile::{CompileInput, LegacyAdequacySnapshot, compile_candidate};
use bitrouter::policy_lock::{PolicyDefinition, PolicyLock, deterministic_yaml, semantic_digest};
use bitrouter::workflow_state::archive::{CloudUsageRecord, WorkflowRunArtifact};
use bitrouter::workflow_state::decision::PolicyDecisionRecord;
use bitrouter::workflow_state::ir::{HarnessId, ProtocolKind};
use bitrouter::workflow_state::real_trace::{CapturedIngressTrace, RealTraceOutcome};
use bitrouter::workflow_state::reward::BenchmarkOutcomeRecord;
use bitrouter::workflow_state::reward_feedback::import_semantic_reward_feedback;
use bitrouter_sdk::language_model::{NormalizedUsage, UsageOrigin};
use serde_json::json;

fn provider_usage(request_id: &str) -> CloudUsageRecord {
    let normalized = NormalizedUsage {
        uncached_input_tokens: 100,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        output_tokens: 20,
        reasoning_tokens: 0,
    };
    CloudUsageRecord {
        id: Some(format!("usage-{request_id}")),
        request_id: Some(request_id.to_string()),
        provider_id: "local-economy".to_string(),
        model_id: "economy".to_string(),
        prompt_tokens: 100,
        completion_tokens: 20,
        reasoning_tokens: 0,
        uncached_input_tokens: 100,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        output_tokens: 20,
        usage_origin: UsageOrigin::ProviderReported,
        raw_usage: Some(json!({"prompt_tokens": 100, "completion_tokens": 20})),
        final_charge_micro_usd: Some(120),
        charge_status: ChargeStatus::Computed,
        charge_evidence: Some(ChargeEvidence {
            status: ChargeStatus::Computed,
            charge_micro_usd: Some(120),
            normalized_usage: normalized,
            effective_rates: EffectivePricingRates {
                uncached_input_micro_usd_per_token: Some(1.0),
                cache_read_micro_usd_per_token: Some(0.0),
                cache_write_micro_usd_per_token: Some(0.0),
                output_micro_usd_per_token: Some(1.0),
            },
            pricing_source: PricingSource::Configured,
            pricing_version: format!("sha256:{}", "0".repeat(64)),
            unknown_reason: None,
        }),
        reconciliation_status: ReconciliationStatus::NotApplicable,
        reconciliation_attempts: 0,
        authoritative_receipt: None,
        status: Some("completed".to_string()),
    }
}

fn policy() -> PolicyDefinition {
    let mut policy = PolicyDefinition {
        tiers: BTreeMap::from([
            ("economy".to_string(), "local/economy".to_string()),
            ("strong".to_string(), "local/strong".to_string()),
        ]),
        default_tier: Some("strong".to_string()),
        tool_use_tier: Some("strong".to_string()),
        tool_safe_tiers: vec!["strong".to_string()],
        ..PolicyDefinition::default()
    };
    policy.adequacy.enabled = true;
    policy.adequacy.explore_enabled = true;
    policy.adequacy.explore_tier = Some("economy".to_string());
    policy.adequacy.explore_opening = true;
    policy.adequacy.escalation_tier = Some("strong".to_string());
    policy.adequacy.min_semantic_successes_for_lock = 1;
    policy
}

#[tokio::test]
async fn smithers_terminal_reward_materializes_only_the_credited_route() -> anyhow::Result<()> {
    let request_id = "req-smithers-1";
    let run_id = "run-smithers-1";
    let task_id = "case-release-review";
    let target_key = "agent_trace/v1|opening|normal";
    let other_key = "agent_trace/v1|tool_followup|normal";
    let ledger_key = format!("smithers\0{target_key}");
    let trace = CapturedIngressTrace {
        id: request_id.to_string(),
        captured_at: None,
        harness: HarnessId::Smithers,
        protocol: ProtocolKind::ChatCompletions,
        method: "POST".to_string(),
        path: "/v1/chat/completions".to_string(),
        headers: [
            ("x-bitrouter-request-id".to_string(), request_id.to_string()),
            (
                "x-smithers-workflow-id".to_string(),
                "release-review".to_string(),
            ),
            ("x-smithers-node-id".to_string(), "analyze-risk".to_string()),
        ]
        .into_iter()
        .collect(),
        raw_body: json!({
            "model": "@smithers",
            "messages": [{"role": "user", "content": "review the release"}]
        }),
        outcome: RealTraceOutcome {
            http_status: 200,
            status: "completed".to_string(),
        },
    };
    let outcome = BenchmarkOutcomeRecord::new(run_id, task_id, 1.0)
        .with_request_id(request_id)
        .with_trial_name(run_id);
    let decision = PolicyDecisionRecord {
        captured_at: None,
        request_id: Some(request_id.to_string()),
        input_model: "@smithers".to_string(),
        key_strategy: "agent_trace".to_string(),
        request_key: target_key.to_string(),
        ledger_key: Some(ledger_key.clone()),
        policy: Some("smithers".to_string()),
        policy_digest: Some(
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
        ),
        preset_variant: Some("smithers".to_string()),
        baseline_tier: Some("strong".to_string()),
        legacy_fingerprint: "opening".to_string(),
        workflow_state: "opening".to_string(),
        workflow_identity: Default::default(),
        static_tier: Some("strong".to_string()),
        static_model: Some("local/strong".to_string()),
        selected_tier: Some("economy".to_string()),
        selected_model: Some("local/economy".to_string()),
        reason: "exploration_trial".to_string(),
        pinned: false,
        request_qualified: true,
        semantic_successes: 0,
        semantic_success_threshold: 1,
        locked: true,
        trialed: true,
    };
    let usage = provider_usage(request_id);
    WorkflowRunArtifact::validate_complete_benchmark_integrity(
        std::slice::from_ref(&trace),
        std::slice::from_ref(&usage),
        std::slice::from_ref(&outcome),
        std::slice::from_ref(&decision),
    )?;
    let artifact = WorkflowRunArtifact::build_with_decisions(
        "smithers-reward",
        &[trace],
        &[usage],
        &[outcome],
        &[decision],
    )?;
    assert_eq!(artifact.reward_join.unmatched_trace_count, 0);
    assert_eq!(artifact.reward_join.unmatched_outcome_count, 0);
    assert_eq!(artifact.semantic_policy_transition_candidates.len(), 1);

    let db = bitrouter::db::connect("sqlite::memory:").await?;
    bitrouter::db::run_migrations(&db).await?;
    let eval_store = EvalStore::new(db);
    let service = EvalService::new(eval_store.clone(), Default::default());
    let feedback =
        import_semantic_reward_feedback(&service, &artifact.semantic_policy_transition_candidates)
            .await?;
    assert_eq!(feedback.admitted_count, 1);
    let manifest = eval_store.freeze_snapshot("2026-07-30T00:02:00Z").await?;
    let evidence = EvalEvidenceSnapshot::load(&eval_store, &manifest.evidence_root).await?;

    let lock = PolicyLock {
        lockfile_version: 1,
        artifact: None,
        policies: BTreeMap::from([("smithers".to_string(), policy())]),
        certificates: BTreeMap::new(),
    };
    let legacy = LegacyAdequacySnapshot {
        snapshot_time_unix_ms: 1_785_369_600_000,
        pins: Vec::new(),
        exploration: Vec::new(),
        semantic_successes: Vec::new(),
        reliability_events: Vec::new(),
    };
    let parent_digest = semantic_digest(&lock)?;
    let evolved = compile_candidate(CompileInput {
        current: &lock,
        parent_digest: Some(&parent_digest),
        legacy: &legacy,
        eval: Some(&evidence),
    })?
    .document;
    assert_eq!(evolved.policies["smithers"].routes[target_key], "economy");
    assert!(!evolved.policies["smithers"].routes.contains_key(other_key));

    let independently_evolved = compile_candidate(CompileInput {
        current: &lock,
        parent_digest: Some(&parent_digest),
        legacy: &legacy,
        eval: Some(&evidence),
    })?
    .document;
    assert_eq!(
        deterministic_yaml(&evolved)?.as_bytes(),
        deterministic_yaml(&independently_evolved)?.as_bytes()
    );
    assert_eq!(
        semantic_digest(&evolved)?,
        semantic_digest(&independently_evolved)?
    );
    Ok(())
}
