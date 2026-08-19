use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use bitrouter_sdk::{BitrouterError, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::workflow_state::real_trace::CapturedIngressTrace;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkOutcomeRecord {
    /// Stable source-neutral request identity for strict reward attribution.
    /// Older analytical artifacts did not persist it and remain readable, but
    /// cannot be admitted to the learning path without an exact join.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    pub session_key: String,
    pub task_id: String,
    pub reward: f64,
    #[serde(default)]
    pub failed_reason: Option<String>,
    #[serde(default)]
    pub finished_at: Option<String>,
    #[serde(default)]
    pub trial_name: Option<String>,
    #[serde(default)]
    pub agent_started_at: Option<String>,
    #[serde(default)]
    pub agent_finished_at: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticOutcomeCandidate {
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
    pub semantic_outcome_candidates: Vec<SemanticOutcomeCandidate>,
}

impl BenchmarkOutcomeRecord {
    /// Construct an outcome artifact without a strict-feedback identity.
    ///
    /// Analytical callers may omit it; feedback import requires
    /// [`Self::with_request_id`] for every outcome.
    pub fn new(session_key: impl Into<String>, task_id: impl Into<String>, reward: f64) -> Self {
        Self {
            request_id: None,
            session_key: session_key.into(),
            task_id: task_id.into(),
            reward,
            failed_reason: None,
            finished_at: None,
            trial_name: None,
            agent_started_at: None,
            agent_finished_at: None,
        }
    }

    /// Attach the canonical persisted trace identity used by strict feedback.
    pub fn with_request_id(mut self, request_id: impl AsRef<str>) -> Self {
        let request_id = request_id.as_ref().trim();
        self.request_id = (!request_id.is_empty()).then(|| request_id.to_string());
        self
    }

    /// Attach an optional analytical trial label without affecting strict
    /// request-identity attribution.
    pub fn with_trial_name(mut self, trial_name: impl Into<String>) -> Self {
        self.trial_name = Some(trial_name.into());
        self
    }

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

    pub fn write_jsonl(path: impl AsRef<Path>, records: &[Self]) -> Result<()> {
        if let Some(parent) = path.as_ref().parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|e| {
                BitrouterError::internal(format!(
                    "benchmark outcome jsonl mkdir {}: {e}",
                    parent.display()
                ))
            })?;
        }
        let file = File::create(path.as_ref()).map_err(|e| {
            BitrouterError::internal(format!(
                "benchmark outcome jsonl create {}: {e}",
                path.as_ref().display()
            ))
        })?;
        let mut writer = BufWriter::new(file);
        for record in records {
            serde_json::to_writer(&mut writer, record).map_err(|e| {
                BitrouterError::internal(format!("benchmark outcome jsonl serialize: {e}"))
            })?;
            writer.write_all(b"\n").map_err(|e| {
                BitrouterError::internal(format!("benchmark outcome jsonl write: {e}"))
            })?;
        }
        writer
            .flush()
            .map_err(|e| BitrouterError::internal(format!("benchmark outcome jsonl flush: {e}")))
    }
}

impl RewardJoin {
    pub fn from_traces_and_outcomes(
        traces: &[CapturedIngressTrace],
        outcomes: &[BenchmarkOutcomeRecord],
    ) -> Self {
        Self::join(traces, outcomes, true)
    }

    /// Benchmark-grade join that requires a canonical request ID on both
    /// artifacts and rejects every non-one-to-one match.
    /// Strict feedback attribution uses canonical request IDs only. Timestamp
    /// and legacy workflow/session attribution remain analytical-only.
    pub fn from_traces_and_outcomes_strict(
        traces: &[CapturedIngressTrace],
        outcomes: &[BenchmarkOutcomeRecord],
    ) -> Self {
        Self::join_by_request_id(traces, outcomes)
    }

    fn join(
        traces: &[CapturedIngressTrace],
        outcomes: &[BenchmarkOutcomeRecord],
        allow_time_fallback: bool,
    ) -> Self {
        let outcomes_by_request_id = outcomes.iter().enumerate().fold(
            BTreeMap::<String, Vec<usize>>::new(),
            |mut acc, (index, outcome)| {
                if let Some(request_id) = outcome.request_id.as_deref().map(str::trim)
                    && !request_id.is_empty()
                {
                    acc.entry(request_id.to_string()).or_default().push(index);
                }
                acc
            },
        );
        let outcomes_by_session = outcomes.iter().enumerate().fold(
            BTreeMap::<String, Vec<usize>>::new(),
            |mut acc, (index, outcome)| {
                acc.entry(outcome.session_key.clone())
                    .or_default()
                    .push(index);
                acc
            },
        );
        let mut matched_outcome_indices = BTreeSet::new();

        let mut summary = RewardJoinSummary {
            outcome_count: outcomes.len(),
            ..RewardJoinSummary::default()
        };
        let mut inadequacy_candidates = Vec::new();
        let mut outcome_candidates = Vec::new();
        for trace in traces {
            let mut matched = Vec::new();
            if let Some(request_id) = trace.artifact_request_id()
                && let Some(request_outcome_indices) = outcomes_by_request_id.get(request_id)
            {
                for index in request_outcome_indices {
                    matched_outcome_indices.insert(*index);
                    matched.push(&outcomes[*index]);
                }
            }
            if matched.is_empty()
                && let Some(session_key) = legacy_trace_session_key(trace)
                && let Some(session_outcome_indices) = outcomes_by_session.get(&session_key)
            {
                for index in session_outcome_indices {
                    matched_outcome_indices.insert(*index);
                    matched.push(&outcomes[*index]);
                }
            }
            if matched.is_empty() && allow_time_fallback {
                let time_matches = outcomes
                    .iter()
                    .enumerate()
                    .filter(|(_, outcome)| trace_captured_during_outcome(trace, outcome))
                    .collect::<Vec<_>>();
                if let [(idx, outcome)] = time_matches.as_slice() {
                    matched_outcome_indices.insert(*idx);
                    matched.push(*outcome);
                }
            }

            if matched.is_empty() {
                summary.unmatched_trace_count += 1;
                continue;
            }
            summary.matched_trace_count += 1;
            for outcome in matched {
                outcome_candidates.push(SemanticOutcomeCandidate {
                    trace_id: trace.id.clone(),
                    session_key: outcome.session_key.clone(),
                    task_id: outcome.task_id.clone(),
                    reward: outcome.reward,
                    failed_reason: outcome.failed_reason.clone(),
                });
                if outcome.reward < 1.0 {
                    inadequacy_candidates.push(SemanticInadequacyCandidate {
                        trace_id: trace.id.clone(),
                        session_key: outcome.session_key.clone(),
                        task_id: outcome.task_id.clone(),
                        reward: outcome.reward,
                        failed_reason: outcome.failed_reason.clone(),
                    });
                }
            }
        }
        summary.unmatched_outcome_count =
            outcomes.len().saturating_sub(matched_outcome_indices.len());

        Self {
            summary,
            semantic_inadequacy_candidates: inadequacy_candidates,
            semantic_outcome_candidates: outcome_candidates,
        }
    }

    fn join_by_request_id(
        traces: &[CapturedIngressTrace],
        outcomes: &[BenchmarkOutcomeRecord],
    ) -> Self {
        let outcomes_by_request_id = outcomes.iter().enumerate().fold(
            BTreeMap::<String, Vec<usize>>::new(),
            |mut acc, (index, outcome)| {
                if let Some(request_id) = outcome.request_id.as_deref().map(str::trim)
                    && !request_id.is_empty()
                {
                    acc.entry(request_id.to_string()).or_default().push(index);
                }
                acc
            },
        );
        let mut matched_outcome_indices = BTreeSet::new();
        let mut summary = RewardJoinSummary {
            outcome_count: outcomes.len(),
            ..RewardJoinSummary::default()
        };
        let mut inadequacy_candidates = Vec::new();
        let mut outcome_candidates = Vec::new();

        for trace in traces {
            let Some(request_id) = trace.artifact_request_id() else {
                summary.unmatched_trace_count += 1;
                continue;
            };
            let Some(indices) = outcomes_by_request_id.get(request_id) else {
                summary.unmatched_trace_count += 1;
                continue;
            };
            let [index] = indices.as_slice() else {
                summary.unmatched_trace_count += 1;
                continue;
            };
            if !matched_outcome_indices.insert(*index) {
                summary.unmatched_trace_count += 1;
                continue;
            }

            let outcome = &outcomes[*index];
            summary.matched_trace_count += 1;
            outcome_candidates.push(SemanticOutcomeCandidate {
                trace_id: trace.id.clone(),
                session_key: outcome.session_key.clone(),
                task_id: outcome.task_id.clone(),
                reward: outcome.reward,
                failed_reason: outcome.failed_reason.clone(),
            });
            if outcome.reward < 1.0 {
                inadequacy_candidates.push(SemanticInadequacyCandidate {
                    trace_id: trace.id.clone(),
                    session_key: outcome.session_key.clone(),
                    task_id: outcome.task_id.clone(),
                    reward: outcome.reward,
                    failed_reason: outcome.failed_reason.clone(),
                });
            }
        }
        summary.unmatched_outcome_count =
            outcomes.len().saturating_sub(matched_outcome_indices.len());
        Self {
            summary,
            semantic_inadequacy_candidates: inadequacy_candidates,
            semantic_outcome_candidates: outcome_candidates,
        }
    }
}

fn trace_captured_during_outcome(
    trace: &CapturedIngressTrace,
    outcome: &BenchmarkOutcomeRecord,
) -> bool {
    let Some(captured_at) = trace.captured_at.as_deref().and_then(parse_timestamp) else {
        return false;
    };
    let Some(started_at) = outcome
        .agent_started_at
        .as_deref()
        .and_then(parse_timestamp)
    else {
        return false;
    };
    let Some(finished_at) = outcome
        .agent_finished_at
        .as_deref()
        .and_then(parse_timestamp)
    else {
        return false;
    };
    captured_at >= started_at && captured_at <= finished_at
}

fn parse_timestamp(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|dt| dt.and_utc())
        })
}

fn legacy_trace_session_key(trace: &CapturedIngressTrace) -> Option<String> {
    [
        "x-bitrouter-trial-id",
        "x-bitrouter-parent-session-id",
        "x-bitrouter-workflow-session",
    ]
    .into_iter()
    .find_map(|name| header_value(trace, name))
}

fn header_value(trace: &CapturedIngressTrace, name: &str) -> Option<String> {
    trace
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}
