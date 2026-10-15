use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use bitrouter::gateway::routing::workflow::decision::PolicyDecisionRecord;

pub fn temp_path(name: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "bitrouter-workflow-state-{name}-{}-{unique}",
        std::process::id()
    ))
}

pub fn benchmark_decision(request_id: &str) -> PolicyDecisionRecord {
    PolicyDecisionRecord {
        captured_at: None,
        request_id: Some(request_id.to_string()),
        ingress_request_id_sha256: None,
        input_model: "inbound".to_string(),
        input_effort: None,
        key_strategy: "workflow_state".to_string(),
        route_projection: None,
        request_key: "agent_trace/v1|opening|normal".to_string(),
        ledger_key: None,
        policy: None,
        policy_digest: None,
        preset_variant: None,
        baseline_tier: Some("strong".to_string()),
        baseline_effort: None,
        legacy_fingerprint: "opening".to_string(),
        workflow_state: "opening".to_string(),
        workflow_identity: Default::default(),
        static_tier: Some("strong".to_string()),
        static_model: Some("vendor/strong".to_string()),
        static_effort: None,
        selected_tier: Some("strong".to_string()),
        selected_model: Some("vendor/strong".to_string()),
        selected_effort: None,
        continuation_proposed_tier: None,
        continuation_proposed_model: None,
        continuation_proposed_effort: None,
        continuation_adjustment: None,
        predicted_role: None,
        predicted_task_family: None,
        predicted_action: None,
        prediction_confidence_ppm: None,
        task_family_confidence_ppm: None,
        predictor_contract_digest: None,
        prediction_confidence_kind: None,
        prediction_reason_codes: Vec::new(),
        task_family_reason_codes: Vec::new(),
        experiment: None,
        route_measurement: None,
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
