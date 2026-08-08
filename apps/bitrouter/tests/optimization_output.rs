use std::collections::BTreeMap;

use anyhow::Result;
use bitrouter::optimization::orchestrator::{
    OptimizationReport, VariantReport, WorkflowFingerprint,
};
use bitrouter::optimization::{OptimizationPreference, OptimizationVerdict};
use bitrouter::output::reports::optimization::{
    OptimizationReviewReport, OptimizationSetupReport, OptimizationStatusReport,
};
use bitrouter::output::{Format, Output};

fn variant(cost: u64, latency: u64) -> VariantReport {
    VariantReport {
        verdict: OptimizationVerdict::Pass,
        confidence: "high".into(),
        reason: "observable contract passed".into(),
        evidence_digest: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            .into(),
        policy_digest: "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            .into(),
        request_count: 4,
        normalized_cost_micro_usd: cost,
        observed_latency_ms: latency,
        elapsed_ms: 2_000,
    }
}

#[test]
fn human_review_leads_with_quality_and_cost_tradeoff_for_auto() -> Result<()> {
    let report = OptimizationReport {
        run_id: "run-20260808".into(),
        created_at: "2026-08-08T00:00:00Z".into(),
        source_policy_digest:
            "sha256:1111111111111111111111111111111111111111111111111111111111111111".into(),
        source_config_digest:
            "sha256:2222222222222222222222222222222222222222222222222222222222222222".into(),
        workflow_fingerprint: WorkflowFingerprint {
            argv_digest: "sha256:3333333333333333333333333333333333333333333333333333333333333333"
                .into(),
            referenced_files: BTreeMap::new(),
            workspace_digest:
                "sha256:4444444444444444444444444444444444444444444444444444444444444444".into(),
        },
        target_request_key: "agent_trace/v2|edit|normal".into(),
        preference: OptimizationPreference::Balanced,
        baseline: variant(10_000, 1_000),
        candidate: variant(6_000, 1_100),
        normalized_cost_delta_micro_usd: -4_000,
        normalized_cost_delta_ppm: Some(-400_000),
        latency_observe_only: true,
        eval_snapshot_digest:
            "sha256:5555555555555555555555555555555555555555555555555555555555555555".into(),
        candidate_digest: "sha256:6666666666666666666666666666666666666666666666666666666666666666"
            .into(),
        candidate_path: "candidate.yaml".into(),
        publishable: true,
        caveats: Vec::new(),
    };
    let view = OptimizationReviewReport::new(report, false, false, true);

    let rendered = String::from_utf8(Output::new(Format::Human).render_to_vec(&view))?;

    assert!(rendered.contains("@auto candidate is ready for review"));
    assert!(rendered.contains("pass → pass"));
    assert!(rendered.contains("$0.010000 → $0.006000"));
    assert!(rendered.contains("40.0% lower"));
    assert!(rendered.contains("latency is observe-only"));
    assert!(rendered.contains("bitrouter optimize publish --run run-20260808"));
    Ok(())
}

#[test]
fn human_setup_explains_auto_workflow_and_next_step() -> Result<()> {
    let view = OptimizationSetupReport {
        action: "optimize.setup",
        model: "@auto",
        intent: "bitrouter.optimize.yaml".into(),
        lock: "bitrouter.optimize.lock.yaml".into(),
        contract: "bitrouter.eval.md".into(),
        workflow: ["npm", "run", "eval"].map(String::from).to_vec(),
        strong: "openai-codex:gpt-5.6-sol".into(),
        economy: "bitrouter:deepseek/deepseek-v4-flash-0731".into(),
        evaluator: "codex-acp (gpt-5.6-sol, direct)".into(),
        evaluator_lock: None,
        normalized_price_overrides: Vec::new(),
        preference: OptimizationPreference::Balanced,
        active_policy_digest:
            "sha256:7777777777777777777777777777777777777777777777777777777777777777".into(),
        latency: "observe_only",
    };

    let rendered = String::from_utf8(Output::new(Format::Human).render_to_vec(&view))?;

    assert!(rendered.contains("@auto optimization is configured"));
    assert!(rendered.contains("npm run eval"));
    assert!(rendered.contains("openai-codex:gpt-5.6-sol → bitrouter:deepseek"));
    assert!(rendered.contains("edit bitrouter.eval.md"));
    assert!(rendered.contains("bitrouter optimize run"));
    Ok(())
}

#[test]
fn human_status_explains_ready_state_and_repair_before_running() -> Result<()> {
    let view = OptimizationStatusReport {
        action: "optimize.status",
        model: "@auto",
        intent: "bitrouter.optimize.yaml".into(),
        intent_digest: "sha256:8888888888888888888888888888888888888888888888888888888888888888"
            .into(),
        lock_active_policy_digest:
            "sha256:9999999999999999999999999999999999999999999999999999999999999999".into(),
        actual_active_policy_digest:
            "sha256:9999999999999999999999999999999999999999999999999999999999999999".into(),
        policy_mode: "frozen".into(),
        lineage_consistent: true,
        latest_candidate_active: false,
        rolled_back: false,
        repair_hint: None,
        preference: OptimizationPreference::Balanced,
        evaluator: "codex-acp (gpt-5.6-sol, direct)".into(),
        evaluator_lock: None,
        latest_run: None,
        latency: "observe_only",
    };

    let rendered = String::from_utf8(Output::new(Format::Human).render_to_vec(&view))?;

    assert!(rendered.contains("@auto is ready to measure"));
    assert!(rendered.contains("lineage"));
    assert!(rendered.contains("consistent"));
    assert!(rendered.contains("bitrouter optimize run --human"));
    Ok(())
}
