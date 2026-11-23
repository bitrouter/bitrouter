//! Serving-time workflow and persisted policy compatibility without a learner.

#[path = "support/workflow.rs"]
mod workflow;
use workflow::{benchmark_decision, temp_path};

use bitrouter::gateway::model::protocol::inbound_adapter_for;
use bitrouter::gateway::routing::workflow::decision::{
    PolicyDecisionRecord, PolicyDecisionSummary,
};
use bitrouter::gateway::routing::workflow::ir::ProtocolKind;
use bitrouter::gateway::routing::workflow::online::OnlineWorkflowState;
use bitrouter::{config, policy_lock};
use bitrouter_ai::types::ApiProtocol;
use http::HeaderMap;
use serde::Deserialize;

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
            "unknown|unknown|normal",
            "unknown|implement|guarded",
            "unknown|implement|guarded",
            "unknown|implement|guarded",
            "unknown|implement|normal",
            "unknown|implement|normal",
            "unknown|implement|normal",
        ]
    );
    Ok(())
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
async fn legacy_workflow_state_lock_is_rejected_before_replay() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let config_path = root.path().join("bitrouter.yaml");
    let policy_path = root.path().join("policy.yaml");
    let raw = r#"
inherit_defaults: false
policy:
  path: "./policy.yaml"
routers:
  legacy:
    selection:
      kind: policy
      policy: legacy
      base_model: "vendor/strong"
"#;
    std::fs::write(&config_path, raw)?;
    // Keep a small old-input rejection fixture, without a legacy parser or
    // old configuration syntax masking which source boundary rejects it.
    std::fs::write(
        &policy_path,
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
    tool_safe_tiers: [strong, economy]
"#,
    )?;
    let config = config::parse_with(raw, |_| None)?;
    let error = policy_lock::load_for_config(&config, Some(&config_path))
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("retired source was accepted"))?;
    assert!(format!("{error:#}").contains("invalid model policy source"));
    Ok(())
}
