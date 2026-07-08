use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use bitrouter_sdk::{BitrouterError, Result};
use serde::{Deserialize, Serialize};

use crate::workflow_state::real_trace::CapturedIngressTrace;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkOutcomeRecord {
    pub session_key: String,
    pub task_id: String,
    pub reward: f64,
    #[serde(default)]
    pub failed_reason: Option<String>,
    #[serde(default)]
    pub finished_at: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct RewardJoinSummary {
    pub outcome_count: usize,
    pub matched_trace_count: usize,
    pub unmatched_trace_count: usize,
    pub unmatched_outcome_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticInadequacyCandidate {
    pub trace_id: String,
    pub session_key: String,
    pub task_id: String,
    pub reward: f64,
    pub failed_reason: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct RewardJoin {
    pub summary: RewardJoinSummary,
    pub semantic_inadequacy_candidates: Vec<SemanticInadequacyCandidate>,
}

impl BenchmarkOutcomeRecord {
    pub fn load_jsonl(path: impl AsRef<Path>) -> Result<Vec<Self>> {
        let file = File::open(path.as_ref()).map_err(|e| {
            BitrouterError::internal(format!(
                "benchmark outcome jsonl open {}: {e}",
                path.as_ref().display()
            ))
        })?;
        let reader = BufReader::new(file);
        let mut records = Vec::new();
        for (idx, line) in reader.lines().enumerate() {
            let line = line.map_err(|e| {
                BitrouterError::internal(format!(
                    "benchmark outcome jsonl read {}: {e}",
                    path.as_ref().display()
                ))
            })?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            records.push(serde_json::from_str(trimmed).map_err(|e| {
                BitrouterError::bad_request(format!(
                    "benchmark outcome jsonl parse {} line {}: {e}",
                    path.as_ref().display(),
                    idx + 1
                ))
            })?);
        }
        Ok(records)
    }
}

impl RewardJoin {
    pub fn from_traces_and_outcomes(
        traces: &[CapturedIngressTrace],
        outcomes: &[BenchmarkOutcomeRecord],
    ) -> Self {
        let outcomes_by_session = outcomes.iter().fold(
            BTreeMap::<String, Vec<&BenchmarkOutcomeRecord>>::new(),
            |mut acc, outcome| {
                acc.entry(outcome.session_key.clone())
                    .or_default()
                    .push(outcome);
                acc
            },
        );
        let trace_sessions = traces
            .iter()
            .filter_map(trace_session_key)
            .collect::<BTreeSet<_>>();

        let mut summary = RewardJoinSummary {
            outcome_count: outcomes.len(),
            ..RewardJoinSummary::default()
        };
        let mut candidates = Vec::new();
        for trace in traces {
            let Some(session_key) = trace_session_key(trace) else {
                summary.unmatched_trace_count += 1;
                continue;
            };
            let Some(session_outcomes) = outcomes_by_session.get(&session_key) else {
                summary.unmatched_trace_count += 1;
                continue;
            };
            summary.matched_trace_count += 1;
            for outcome in session_outcomes {
                if outcome.reward < 1.0 {
                    candidates.push(SemanticInadequacyCandidate {
                        trace_id: trace.id.clone(),
                        session_key: session_key.clone(),
                        task_id: outcome.task_id.clone(),
                        reward: outcome.reward,
                        failed_reason: outcome.failed_reason.clone(),
                    });
                }
            }
        }
        summary.unmatched_outcome_count = outcomes
            .iter()
            .filter(|outcome| !trace_sessions.contains(&outcome.session_key))
            .count();

        Self {
            summary,
            semantic_inadequacy_candidates: candidates,
        }
    }
}

fn trace_session_key(trace: &CapturedIngressTrace) -> Option<String> {
    trace
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("x-bitrouter-workflow-session"))
        .map(|(_, value)| value.trim())
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}
