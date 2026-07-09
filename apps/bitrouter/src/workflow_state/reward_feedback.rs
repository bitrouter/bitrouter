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
    pub pinned_request_keys: Vec<String>,
}

pub async fn apply_semantic_failure_pins(
    store: &AdequacyStore,
    candidates: &[SemanticPolicyTransitionCandidate],
) -> Result<RewardFeedbackSummary> {
    let mut keys = BTreeSet::new();
    for candidate in candidates {
        if candidate.reward >= 1.0 {
            continue;
        }
        let key = candidate.request_key.trim();
        if !key.is_empty() {
            keys.insert(key.to_string());
        }
    }

    let exploration = store.load_exploration_all().await?;
    let now = now_unix();
    for key in &keys {
        store.upsert_pin(key, now as i64).await?;
        if let Some(row) = exploration.iter().find(|row| row.fingerprint == *key) {
            store
                .upsert_exploration(key, row.observed, 0, false)
                .await?;
        }
    }

    Ok(RewardFeedbackSummary {
        candidate_count: candidates.len(),
        pinned_count: keys.len(),
        pinned_request_keys: keys.into_iter().collect(),
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

    fn candidate(request_key: &str) -> SemanticPolicyTransitionCandidate {
        SemanticPolicyTransitionCandidate {
            trace_id: "trace-1".to_string(),
            request_id: "req-1".to_string(),
            session_key: "trial-1".to_string(),
            task_id: "terminal-bench/regex-log".to_string(),
            reward: 0.0,
            failed_reason: Some("verifier_failed".to_string()),
            request_key: request_key.to_string(),
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

        let summary =
            apply_semantic_failure_pins(&store, &[candidate(request_key), candidate(request_key)])
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
}
