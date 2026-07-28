use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

use bitrouter_sdk::Result;
use serde::{Deserialize, Serialize};

use crate::adequacy::store::AdequacyStore;
use crate::workflow_state::archive::{
    RequestTransportOutcome, SemanticPolicyTransitionCandidate, SemanticSettlementOutcome,
};

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewardFeedbackSummary {
    pub candidate_count: usize,
    pub skipped_candidate_count: usize,
    pub skipped_reasons: BTreeMap<String, usize>,
    pub pinned_count: usize,
    pub semantic_success_evidence_count: usize,
    pub pinned_request_keys: Vec<String>,
    pub semantic_success_request_keys: Vec<String>,
    pub decisions: Vec<RewardFeedbackDecision>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewardFeedbackDecision {
    pub request_id: String,
    pub ledger_key: Option<String>,
    pub action: String,
    pub reason: String,
}

pub async fn apply_semantic_reward_feedback(
    store: &AdequacyStore,
    candidates: &[SemanticPolicyTransitionCandidate],
) -> Result<RewardFeedbackSummary> {
    let mut failed_keys = BTreeSet::new();
    let mut successful_candidates = Vec::new();
    let mut skipped_reasons = BTreeMap::new();
    let mut decisions = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let key = candidate
            .ledger_key
            .as_deref()
            .unwrap_or(&candidate.request_key)
            .trim();
        if key.is_empty() {
            record_skip(&mut skipped_reasons, "empty_ledger_key");
            decisions.push(feedback_decision(
                candidate,
                None,
                "skipped",
                "empty_ledger_key",
            ));
            continue;
        }
        if candidate.request_transport_outcome != RequestTransportOutcome::Completed {
            record_skip(&mut skipped_reasons, "request_not_completed");
            decisions.push(feedback_decision(
                candidate,
                Some(key),
                "skipped",
                "request_not_completed",
            ));
            continue;
        }
        if !matches!(
            candidate.settlement_outcome,
            SemanticSettlementOutcome::AuthoritativeComputed
                | SemanticSettlementOutcome::ProviderReportedComputed
        ) {
            record_skip(
                &mut skipped_reasons,
                "settlement_not_authoritative_computed",
            );
            decisions.push(feedback_decision(
                candidate,
                Some(key),
                "skipped",
                "settlement_not_authoritative_computed",
            ));
            continue;
        }
        let task_id = candidate.task_id.trim();
        if task_id.is_empty() {
            record_skip(&mut skipped_reasons, "empty_task_id");
            decisions.push(feedback_decision(
                candidate,
                Some(key),
                "skipped",
                "empty_task_id",
            ));
            continue;
        }
        if candidate.reward >= 1.0 {
            let decision_index = decisions.len();
            decisions.push(feedback_decision(
                candidate,
                Some(key),
                "pending",
                "semantic_success_eligible",
            ));
            successful_candidates.push((decision_index, key.to_string(), task_id.to_string()));
        } else {
            failed_keys.insert(key.to_string());
            decisions.push(feedback_decision(
                candidate,
                Some(key),
                "applied",
                "semantic_failure_pinned",
            ));
        }
    }

    let exploration = store.load_exploration_all().await?;
    let now = now_unix();
    for key in &failed_keys {
        store.upsert_pin(key, now as i64).await?;
        store.clear_semantic_successes(key).await?;
        if let Some(row) = exploration.iter().find(|row| row.fingerprint == *key) {
            store
                .upsert_exploration(key, row.observed, 0, false)
                .await?;
        }
    }

    let mut semantic_success_evidence_count = 0;
    let mut semantic_success_request_keys = BTreeSet::new();
    for (decision_index, key, task_id) in successful_candidates {
        if failed_keys.contains(&key) {
            record_skip(&mut skipped_reasons, "semantic_failure_wins");
            decisions[decision_index].action = "skipped".to_string();
            decisions[decision_index].reason = "semantic_failure_wins".to_string();
        } else if store.record_semantic_success(&key, &task_id).await? {
            semantic_success_evidence_count += 1;
            semantic_success_request_keys.insert(key);
            decisions[decision_index].action = "applied".to_string();
            decisions[decision_index].reason = "semantic_success_recorded".to_string();
        } else {
            record_skip(&mut skipped_reasons, "duplicate_semantic_evidence");
            decisions[decision_index].action = "skipped".to_string();
            decisions[decision_index].reason = "duplicate_semantic_evidence".to_string();
        }
    }

    Ok(RewardFeedbackSummary {
        candidate_count: candidates.len(),
        skipped_candidate_count: skipped_reasons.values().sum(),
        skipped_reasons,
        pinned_count: failed_keys.len(),
        semantic_success_evidence_count,
        pinned_request_keys: failed_keys.into_iter().collect(),
        semantic_success_request_keys: semantic_success_request_keys.into_iter().collect(),
        decisions,
    })
}

fn feedback_decision(
    candidate: &SemanticPolicyTransitionCandidate,
    ledger_key: Option<&str>,
    action: &str,
    reason: &str,
) -> RewardFeedbackDecision {
    RewardFeedbackDecision {
        request_id: candidate.request_id.clone(),
        ledger_key: ledger_key.map(ToString::to_string),
        action: action.to_string(),
        reason: reason.to_string(),
    }
}

fn record_skip(reasons: &mut BTreeMap<String, usize>, reason: &str) {
    *reasons.entry(reason.to_string()).or_default() += 1;
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    async fn store() -> AdequacyStore {
        let db = db::connect("sqlite::memory:").await.unwrap();
        db::run_migrations(&db).await.unwrap();
        AdequacyStore::new(db)
    }

    fn candidate(
        request_key: &str,
        task_id: &str,
        reward: f64,
    ) -> SemanticPolicyTransitionCandidate {
        SemanticPolicyTransitionCandidate {
            trace_id: "trace-1".to_string(),
            request_id: "req-1".to_string(),
            session_key: "trial-1".to_string(),
            task_id: task_id.to_string(),
            reward,
            failed_reason: (reward < 1.0).then(|| "verifier_failed".to_string()),
            request_transport_outcome: RequestTransportOutcome::Completed,
            settlement_outcome: SemanticSettlementOutcome::AuthoritativeComputed,
            request_key: request_key.to_string(),
            ledger_key: None,
            workflow_state: "tool_followup".to_string(),
            static_tier: Some("capable".to_string()),
            selected_tier: Some("cheap".to_string()),
            tier_transition: Some("capable -> cheap".to_string()),
            static_model: Some("openai-codex:gpt-5.5".to_string()),
            selected_model: Some("bitrouter:moonshotai/kimi-k2.7-code".to_string()),
            model_transition: Some(
                "openai-codex:gpt-5.5 -> bitrouter:moonshotai/kimi-k2.7-code".to_string(),
            ),
            reason: "exploration_locked".to_string(),
        }
    }

    #[tokio::test]
    async fn semantic_failure_feedback_pins_and_unlocks_request_keys() {
        let store = store().await;
        let request_key = "codex|responses|tool_followup|-|-|exec_command";
        store
            .upsert_exploration(request_key, 8, 4, true)
            .await
            .unwrap();

        let summary = apply_semantic_reward_feedback(
            &store,
            &[
                candidate(request_key, "terminal-bench/regex-log", 0.0),
                candidate(request_key, "terminal-bench/regex-log", 0.0),
            ],
        )
        .await
        .unwrap();

        assert_eq!(summary.candidate_count, 2);
        assert_eq!(summary.pinned_count, 1);
        assert_eq!(summary.pinned_request_keys, vec![request_key.to_string()]);
        let pins = store.load_all().await.unwrap();
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].0, request_key);
        let exploration = store.load_exploration_all().await.unwrap();
        assert_eq!(exploration.len(), 1);
        assert_eq!(exploration[0].fingerprint, request_key);
        assert_eq!(exploration[0].observed, 8);
        assert_eq!(exploration[0].adequate_trials, 0);
        assert!(!exploration[0].locked);
    }

    #[tokio::test]
    async fn semantic_success_feedback_counts_distinct_tasks_once() {
        let store = store().await;
        let request_key = "codex|responses|tool_followup|-|-|exec_command";

        let summary = apply_semantic_reward_feedback(
            &store,
            &[
                candidate(request_key, "terminal-bench/regex-log", 1.0),
                candidate(request_key, "terminal-bench/regex-log", 1.0),
                candidate(request_key, "terminal-bench/fix-git", 1.0),
            ],
        )
        .await
        .unwrap();

        assert_eq!(summary.pinned_count, 0);
        assert_eq!(summary.semantic_success_evidence_count, 2);
        assert_eq!(
            store.load_semantic_success_counts().await.unwrap(),
            [(request_key.to_string(), 2)].into_iter().collect()
        );

        let replayed = apply_semantic_reward_feedback(
            &store,
            &[candidate(request_key, "terminal-bench/regex-log", 1.0)],
        )
        .await
        .unwrap();
        assert_eq!(replayed.semantic_success_evidence_count, 0);
        assert_eq!(
            store.load_semantic_success_counts().await.unwrap(),
            [(request_key.to_string(), 2)].into_iter().collect()
        );
    }

    #[tokio::test]
    async fn semantic_feedback_uses_the_named_policy_ledger_key() {
        let store = store().await;
        let request_key = "codex|responses|tool_followup|-|-|exec_command";
        let ledger_key = format!("coding\0{request_key}");
        let mut named = candidate(request_key, "terminal-bench/regex-log", 1.0);
        named.ledger_key = Some(ledger_key.clone());

        apply_semantic_reward_feedback(&store, &[named])
            .await
            .unwrap();

        assert_eq!(
            store.load_semantic_success_counts().await.unwrap(),
            [(ledger_key, 1)].into_iter().collect()
        );
    }

    #[tokio::test]
    async fn semantic_failure_wins_over_success_for_the_same_request_key() {
        let store = store().await;
        let request_key = "codex|responses|tool_followup|-|-|exec_command";

        let summary = apply_semantic_reward_feedback(
            &store,
            &[
                candidate(request_key, "terminal-bench/regex-log", 1.0),
                candidate(request_key, "terminal-bench/fix-git", 0.0),
            ],
        )
        .await
        .unwrap();

        assert_eq!(summary.pinned_count, 1);
        assert_eq!(summary.semantic_success_evidence_count, 0);
        assert!(
            store
                .load_semantic_success_counts()
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn transport_failure_cannot_hitchhike_on_a_successful_task_reward() {
        let store = store().await;
        let request_key = "codex|responses|tool_followup|-|-|exec_command";
        let mut timed_out = candidate(request_key, "terminal-bench/regex-log", 1.0);
        timed_out.request_transport_outcome = RequestTransportOutcome::Failed;
        timed_out.settlement_outcome = SemanticSettlementOutcome::AuthoritativeComputed;

        let summary = apply_semantic_reward_feedback(&store, &[timed_out])
            .await
            .unwrap();

        assert_eq!(summary.semantic_success_evidence_count, 0);
        assert_eq!(summary.pinned_count, 0);
        assert_eq!(summary.decisions.len(), 1);
        assert_eq!(summary.decisions[0].request_id, "req-1");
        assert_eq!(summary.decisions[0].action, "skipped");
        assert_eq!(summary.decisions[0].reason, "request_not_completed");
        assert!(
            store
                .load_semantic_success_counts()
                .await
                .unwrap()
                .is_empty()
        );
        assert!(store.load_all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn unsettled_request_cannot_write_semantic_evidence() {
        let store = store().await;
        let request_key = "codex|responses|tool_followup|-|-|exec_command";
        let mut pending = candidate(request_key, "terminal-bench/regex-log", 1.0);
        pending.settlement_outcome = SemanticSettlementOutcome::Pending;

        let summary = apply_semantic_reward_feedback(&store, &[pending])
            .await
            .unwrap();

        assert_eq!(summary.semantic_success_evidence_count, 0);
        assert_eq!(summary.pinned_count, 0);
        assert_eq!(summary.decisions[0].action, "skipped");
        assert_eq!(
            summary.decisions[0].reason,
            "settlement_not_authoritative_computed"
        );
        assert!(
            store
                .load_semantic_success_counts()
                .await
                .unwrap()
                .is_empty()
        );
    }
}
