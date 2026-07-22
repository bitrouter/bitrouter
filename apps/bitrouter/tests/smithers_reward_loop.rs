use std::collections::BTreeMap;

use bitrouter::adequacy::store::AdequacyStore;
use bitrouter::metering::{
    ChargeEvidence, ChargeStatus, EffectivePricingRates, PricingSource, ReconciliationStatus,
};
use bitrouter::policy_lock::{
    PolicyDefinition, PolicyLock, deterministic_yaml, evolve_document, freeze_document,
    semantic_digest,
};
use bitrouter::workflow_state::archive::{CloudUsageRecord, WorkflowRunArtifact};
use bitrouter::workflow_state::decision::PolicyDecisionRecord;
use bitrouter::workflow_state::ir::{HarnessId, ProtocolKind};
use bitrouter::workflow_state::real_trace::{CapturedIngressTrace, RealTraceOutcome};
use bitrouter::workflow_state::reward::BenchmarkOutcomeRecord;
use bitrouter::workflow_state::reward_feedback::apply_semantic_reward_feedback;
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
    policy.adequacy.escalation_tier = Some("strong".to_string());
    policy.adequacy.min_semantic_successes_for_lock = 1;
    policy
}

#[tokio::test]
async fn smithers_terminal_reward_materializes_only_the_credited_route() {
    let request_id = "req-smithers-1";
    let run_id = "run-smithers-1";
    let task_id = "case-release-review";
    let target_key = "smithers|chat_completions|opening|release-review|analyze-risk|-|none|small|none|low|low|low|low|medium|-";
    let other_key = "smithers|chat_completions|opening|release-review|summarize|-|none|small|none|low|low|low|low|medium|-";
    let ledger_key = format!("smithers\0{target_key}");
    let trace = CapturedIngressTrace {
        id: "trace-smithers-1".to_string(),
        captured_at: None,
        harness: HarnessId::Smithers,
        protocol: ProtocolKind::ChatCompletions,
        method: "POST".to_string(),
        path: "/v1/chat/completions".to_string(),
        headers: [
            ("x-bitrouter-request-id".to_string(), request_id.to_string()),
            (
                "x-bitrouter-workflow-session".to_string(),
                run_id.to_string(),
            ),
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
    let outcome = BenchmarkOutcomeRecord {
        session_key: run_id.to_string(),
        task_id: task_id.to_string(),
        reward: 1.0,
        failed_reason: None,
        finished_at: None,
        trial_name: Some(run_id.to_string()),
        agent_started_at: None,
        agent_finished_at: None,
    };
    let decision = PolicyDecisionRecord {
        captured_at: None,
        request_id: Some(request_id.to_string()),
        input_model: "@smithers".to_string(),
        key_strategy: "workflow_state".to_string(),
        request_key: target_key.to_string(),
        ledger_key: Some(ledger_key.clone()),
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
    )
    .unwrap();
    let artifact = WorkflowRunArtifact::build_with_decisions(
        "smithers-reward",
        &[trace],
        &[usage],
        &[outcome],
        &[decision],
    )
    .unwrap();
    assert_eq!(artifact.reward_join.unmatched_trace_count, 0);
    assert_eq!(artifact.reward_join.unmatched_outcome_count, 0);
    assert_eq!(artifact.semantic_policy_transition_candidates.len(), 1);

    let db = bitrouter::db::connect("sqlite::memory:").await.unwrap();
    bitrouter::db::run_migrations(&db).await.unwrap();
    let store = AdequacyStore::new(db);
    store
        .upsert_exploration(&ledger_key, 3, 3, true)
        .await
        .unwrap();
    let feedback =
        apply_semantic_reward_feedback(&store, &artifact.semantic_policy_transition_candidates)
            .await
            .unwrap();
    assert_eq!(feedback.semantic_success_evidence_count, 1);

    let lock = PolicyLock {
        lockfile_version: 1,
        policies: BTreeMap::from([("smithers".to_string(), policy())]),
    };
    let exploration = store.load_exploration_all().await.unwrap();
    let semantic = store.load_semantic_success_counts().await.unwrap();
    let frozen = freeze_document(
        evolve_document(&lock, &exploration, &semantic)
            .unwrap()
            .document,
    );
    assert_eq!(frozen.policies["smithers"].routes[target_key], "economy");
    assert!(!frozen.policies["smithers"].routes.contains_key(other_key));
    assert!(frozen.policies["smithers"].adequacy.enabled);
    assert!(!frozen.policies["smithers"].adequacy.explore_enabled);

    let independently_frozen = freeze_document(
        evolve_document(&lock, &exploration, &semantic)
            .unwrap()
            .document,
    );
    assert_eq!(
        deterministic_yaml(&frozen).unwrap().as_bytes(),
        deterministic_yaml(&independently_frozen)
            .unwrap()
            .as_bytes()
    );
    assert_eq!(
        semantic_digest(&frozen).unwrap(),
        semantic_digest(&independently_frozen).unwrap()
    );
}
