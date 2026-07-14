use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use bitrouter_sdk::Result;
use serde::{Deserialize, Serialize};

use crate::adequacy::store::AdequacyStore;
use crate::workflow_state::archive::SemanticPolicyTransitionCandidate;

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RewardFeedbackSummary {
    pub candidate_count: usize,
    pub pinned_count: usize,
    pub semantic_success_evidence_count: usize,
    pub pinned_request_keys: Vec<String>,
    pub semantic_success_request_keys: Vec<String>,
}

pub async fn apply_semantic_reward_feedback(
    store: &AdequacyStore,
    candidates: &[SemanticPolicyTransitionCandidate],
) -> Result<RewardFeedbackSummary> {
    let mut failed_keys = BTreeSet::new();
    let mut successful_evidence = BTreeSet::new();
    for candidate in candidates {
        let key = candidate
            .ledger_key
            .as_deref()
            .unwrap_or(&candidate.request_key)
            .trim();
        if key.is_empty() {
            continue;
        }
        if candidate.reward >= 1.0 {
            let task_id = candidate.task_id.trim();
            if !task_id.is_empty() {
                successful_evidence.insert((key.to_string(), task_id.to_string()));
            }
        } else {
            failed_keys.insert(key.to_string());
        }
    }
    successful_evidence.retain(|(key, _)| !failed_keys.contains(key));

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
    for (key, task_id) in successful_evidence {
        if store.record_semantic_success(&key, &task_id).await? {
            semantic_success_evidence_count += 1;
            semantic_success_request_keys.insert(key);
        }
    }

    Ok(RewardFeedbackSummary {
        candidate_count: candidates.len(),
        pinned_count: failed_keys.len(),
        semantic_success_evidence_count,
        pinned_request_keys: failed_keys.into_iter().collect(),
        semantic_success_request_keys: semantic_success_request_keys.into_iter().collect(),
    })
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
}
