use super::*;
use crate::gateway::model::decision::types::{
    Answer, DecisionRequest, DecisionResponse, DecisionUsage, Question,
};
use crate::storage::checkpoint::snapshot::context::evidence::{EvidenceBlock, EvidenceSource};
use crate::storage::checkpoint::snapshot::context::view::{Candidate, CandidateSet};
use crate::storage::checkpoint::snapshot::context::{DecisionReceipt, WorkUnit};
use bitrouter_ai::types::{Message, Role};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

// This test is deliberately available with core alone: historical receipt
// shapes remain readable without enabling any owned Choice execution methods.
#[test]
fn ordinary_full_context_ignores_historical_omission_and_keeps_hard_limit() -> Result<(), CoreError>
{
    let messages = vec![Message::text(Role::Assistant, "retain historical evidence")];
    let block = EvidenceBlock::new(
        messages.clone(),
        EvidenceSource {
            task_id: "task".into(),
            agent_id: "agent".into(),
            workspace_id: "workspace".into(),
            workspace_revision: None,
            permission_revision: 1,
            tool_manifest_digest: "a".repeat(64),
            skill_binding: None,
        },
    )?;
    assert!(!block.protected);
    let id = block.block_id.clone();
    let prompt: Prompt = serde_json::from_value(json!({
        "model": "generation", "system": "required instructions",
        "messages": messages, "stream": false
    }))
    .map_err(|error| invalid(error.to_string()))?;
    let source = SourceRevision {
        context_revision: 1,
        signal_revision: 1,
        manifest_sha256: "b".repeat(64),
        prompt_sha256: digest(&prompt)?,
    };
    let work: WorkUnit = serde_json::from_value(json!({
        "task_id": "task", "run_id": "run", "agent_id": "agent",
        "text": "current task", "acceptance_criteria": [], "instructions": [],
        "evidence": [id], "recalled": [], "status": "runnable", "result_evidence": []
    }))
    .map_err(|error| invalid(error.to_string()))?;
    let request = DecisionRequest {
        model: "historical-host-model".into(),
        state: json!({}),
        questions: BTreeMap::from([(
            "evidence".into(),
            Question::Choice {
                instructions: None,
                criteria: BTreeMap::from([
                    ("full".into(), json!(null)),
                    ("hide".into(), json!(null)),
                ]),
            },
        )]),
    };
    let receipt = DecisionReceipt {
        decision_id: "historical".into(),
        run_id: "run".into(),
        task_id: "task".into(),
        agent_id: "agent".into(),
        source: source.clone(),
        candidates: CandidateSet {
            task_id: "task".into(),
            ordered_blocks: vec![id.clone()],
            required: BTreeSet::new(),
            candidates: BTreeMap::from([(
                "evidence".into(),
                Candidate {
                    block_id: id.clone(),
                    summary_id: None,
                    extract_id: None,
                    excerpt: "retain historical evidence".into(),
                },
            )]),
        },
        request_sha256: digest(&request)?,
        request,
        policy: Default::default(),
        pricing: None,
        intent_state_revision: 1,
        response_limit_bytes: 4096,
        outcome: Some(Ok(DecisionResponse {
            model: "historical-host-model".into(),
            answers: BTreeMap::from([(
                "evidence".into(),
                Answer::Choice {
                    choice: "hide".into(),
                    confidence: 1.0,
                    probabilities: BTreeMap::from([("full".into(), 0.0), ("hide".into(), 1.0)]),
                },
            )]),
            usage: DecisionUsage {
                input_tokens: 1,
                output_tokens: 1,
            },
        })),
        elapsed_ms: Some(1),
        stale: false,
        view_ids: Vec::new(),
    };
    receipt
        .outcome
        .as_ref()
        .and_then(|outcome| outcome.as_ref().ok())
        .ok_or_else(|| invalid("historical response missing"))?
        .validate(&receipt.request)
        .map_err(|error| invalid(error.to_string()))?;
    let store = ContextStore {
        work: BTreeMap::from([("task".into(), work)]),
        evidence: BTreeMap::from([(id.clone(), block)]),
        decisions: BTreeMap::from([("historical".into(), receipt)]),
        ..Default::default()
    };
    let (view, compiled) = compile_full(
        &store,
        "task",
        vec![id.clone()],
        &prompt,
        source.clone(),
        4096,
    )?;
    assert_eq!(compiled, prompt);
    assert_eq!(view.required, vec![id.clone()]);
    assert!(view.omitted.is_empty());
    assert!(view.decision_id.is_none());
    assert_eq!(view.reason, "explicit_context_only");
    let error = compile_full(&store, "task", vec![id], &prompt, source, 1)
        .err()
        .ok_or_else(|| invalid("ordinary full context bypassed its limit"))?;
    assert_eq!(error.code, ErrorCode::NoFeasibleRoute);
    Ok(())
}
