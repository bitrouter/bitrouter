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

    pub fn load_harbor_run_dir(path: impl AsRef<Path>) -> Result<Vec<Self>> {
        let path = path.as_ref();
        let entries = fs::read_dir(path).map_err(|e| {
            BitrouterError::internal(format!("harbor run dir read {}: {e}", path.display()))
        })?;
        let mut result_paths = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| {
                BitrouterError::internal(format!("harbor run dir entry {}: {e}", path.display()))
            })?;
            let result_path = entry.path().join("result.json");
            if result_path.is_file() {
                result_paths.push(result_path);
            }
        }
        result_paths.sort();

        result_paths
            .into_iter()
            .map(|path| Self::from_harbor_trial_result(&path))
            .collect()
    }

    fn from_harbor_trial_result(path: &Path) -> Result<Self> {
        let value =
            serde_json::from_str::<serde_json::Value>(&fs::read_to_string(path).map_err(|e| {
                BitrouterError::internal(format!(
                    "harbor trial result read {}: {e}",
                    path.display()
                ))
            })?)
            .map_err(|e| {
                BitrouterError::bad_request(format!(
                    "harbor trial result parse {}: {e}",
                    path.display()
                ))
            })?;
        let trial_name = json_str(&value, &["trial_name"])
            .ok_or_else(|| missing_harbor_field(path, "trial_name"))?;
        let task_id = json_str(&value, &["task_name"])
            .or_else(|| harbor_task_id(&value))
            .ok_or_else(|| missing_harbor_field(path, "task_name"))?;
        let reward = value
            .get("verifier_result")
            .and_then(|v| v.get("rewards"))
            .and_then(|v| v.get("reward"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let finished_at = json_str(&value, &["finished_at"]);
        let agent_started_at = json_str(&value, &["agent_execution", "started_at"]);
        let agent_finished_at = json_str(&value, &["agent_execution", "finished_at"]);
        let failed_reason = if reward >= 1.0 {
            None
        } else {
            harbor_exception_reason(&value).or_else(|| Some("verifier_failed".to_string()))
        };

        Ok(Self {
            session_key: trial_name.clone(),
            task_id,
            reward,
            failed_reason,
            finished_at,
            trial_name: Some(trial_name),
            agent_started_at,
            agent_finished_at,
        })
    }
}

impl RewardJoin {
    pub fn from_traces_and_outcomes(
        traces: &[CapturedIngressTrace],
        outcomes: &[BenchmarkOutcomeRecord],
    ) -> Self {
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
            if let Some(session_key) = trace_session_key(trace)
                && let Some(session_outcome_indices) = outcomes_by_session.get(&session_key)
            {
                for index in session_outcome_indices {
                    matched_outcome_indices.insert(*index);
                    matched.push(&outcomes[*index]);
                }
            }
            if matched.is_empty() {
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

fn trace_session_key(trace: &CapturedIngressTrace) -> Option<String> {
    [
        "x-bitrouter-trial-id",
        "x-bitrouter-parent-session-id",
        "x-bitrouter-workflow-session",
    ]
    .into_iter()
    .find_map(|name| {
        trace
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim())
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    })
}

fn json_str(value: &serde_json::Value, path: &[&str]) -> Option<String> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    current
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn harbor_task_id(value: &serde_json::Value) -> Option<String> {
    let task_id = value.get("task_id")?;
    let org = task_id.get("org").and_then(|v| v.as_str())?;
    let name = task_id.get("name").and_then(|v| v.as_str())?;
    Some(format!("{org}/{name}"))
}

fn harbor_exception_reason(value: &serde_json::Value) -> Option<String> {
    let exception = value.get("exception_info")?;
    if exception.is_null() {
        return None;
    }
    json_str(exception, &["type"])
        .or_else(|| json_str(exception, &["class"]))
        .or_else(|| json_str(exception, &["message"]))
        .or_else(|| Some("agent_exception".to_string()))
}

fn missing_harbor_field(path: &Path, field: &str) -> BitrouterError {
    BitrouterError::bad_request(format!(
        "harbor trial result {} missing required field {field}",
        path.display()
    ))
}
