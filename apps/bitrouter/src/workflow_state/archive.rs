use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use bitrouter_sdk::{BitrouterError, Result};
use serde::{Deserialize, Serialize};

use crate::workflow_state::decision::{PolicyDecisionRecord, PolicyDecisionSummary};
use crate::workflow_state::fixture::WorkflowTraceFixture;
use crate::workflow_state::real_trace::{CapturedIngressTrace, TraceSanitizer};
use crate::workflow_state::replay::{ReplayEvaluator, ReplaySummary};
use crate::workflow_state::reward::{
    BenchmarkOutcomeRecord, RewardJoin, RewardJoinSummary, SemanticInadequacyCandidate,
};
use crate::workflow_state::shadow_policy::{ShadowPolicyEvaluator, ShadowPolicySummary};

pub struct TraceArchive;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudUsageRecord {
    pub id: Option<String>,
    pub request_id: Option<String>,
    pub provider_id: String,
    pub model_id: String,
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    pub final_charge_micro_usd: Option<u64>,
    pub status: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CloudUsageSummary {
    pub request_count: usize,
    pub settled_request_count: usize,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub final_charge_micro_usd: u64,
    pub final_charge_usd: f64,
    pub by_model_provider: BTreeMap<String, CloudUsageModelSummary>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CloudUsageModelSummary {
    pub request_count: usize,
    pub settled_request_count: usize,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub final_charge_micro_usd: u64,
    pub final_charge_usd: f64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CostJoinSummary {
    pub matched_trace_count: usize,
    pub unmatched_trace_count: usize,
    pub unmatched_usage_count: usize,
}

#[derive(Debug, Serialize)]
pub struct WorkflowRunArtifact {
    pub run_label: String,
    pub trace_count: usize,
    pub replay: ReplaySummary,
    pub shadow_policy: ShadowPolicySummary,
    pub policy_decisions: PolicyDecisionSummary,
    pub cost: CloudUsageSummary,
    pub cost_join: CostJoinSummary,
    pub reward_join: RewardJoinSummary,
    pub semantic_inadequacy_candidates: Vec<SemanticInadequacyCandidate>,
    pub route_counts: BTreeMap<String, usize>,
}

impl TraceArchive {
    pub fn write_jsonl(
        path: impl AsRef<Path>,
        traces: &[CapturedIngressTrace],
        sanitizer: &TraceSanitizer,
    ) -> Result<()> {
        if let Some(parent) = path.as_ref().parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|e| {
                BitrouterError::internal(format!(
                    "workflow trace archive mkdir {}: {e}",
                    parent.display()
                ))
            })?;
        }
        let file = File::create(path.as_ref()).map_err(|e| {
            BitrouterError::internal(format!(
                "workflow trace archive create {}: {e}",
                path.as_ref().display()
            ))
        })?;
        let mut writer = BufWriter::new(file);
        for trace in traces {
            let sanitized = sanitizer.sanitize_trace(trace);
            serde_json::to_writer(&mut writer, &sanitized).map_err(|e| {
                BitrouterError::internal(format!("workflow trace archive serialize: {e}"))
            })?;
            writer.write_all(b"\n").map_err(|e| {
                BitrouterError::internal(format!("workflow trace archive write: {e}"))
            })?;
        }
        writer
            .flush()
            .map_err(|e| BitrouterError::internal(format!("workflow trace archive flush: {e}")))
    }

    pub fn read_jsonl(path: impl AsRef<Path>) -> Result<Vec<CapturedIngressTrace>> {
        read_jsonl_values(path.as_ref())?
            .into_iter()
            .map(|value| {
                serde_json::from_value(value).map_err(|e| {
                    BitrouterError::bad_request(format!("workflow trace archive parse: {e}"))
                })
            })
            .collect()
    }

    pub fn to_replay_fixtures(
        traces: &[CapturedIngressTrace],
    ) -> Result<Vec<WorkflowTraceFixture>> {
        traces
            .iter()
            .map(|trace| {
                trace
                    .to_replay_fixture_json(&TraceSanitizer::default())
                    .and_then(WorkflowTraceFixture::from_value)
            })
            .collect()
    }

    pub fn read_replay_fixtures(path: impl AsRef<Path>) -> Result<Vec<WorkflowTraceFixture>> {
        let traces = Self::read_jsonl(path)?;
        Self::to_replay_fixtures(&traces)
    }

    pub fn join_outcomes(
        traces: &[CapturedIngressTrace],
        outcomes: &[BenchmarkOutcomeRecord],
    ) -> RewardJoin {
        RewardJoin::from_traces_and_outcomes(traces, outcomes)
    }
}

impl CloudUsageRecord {
    pub fn load_snapshot_jsonl(path: impl AsRef<Path>) -> Result<Vec<Self>> {
        let mut records_by_key = BTreeMap::new();
        for (line_idx, value) in read_jsonl_values(path.as_ref())?.into_iter().enumerate() {
            let values = if let Some(data) = value.get("data").and_then(|data| data.as_array()) {
                data.clone()
            } else if let Some(array) = value.as_array() {
                array.clone()
            } else {
                vec![value]
            };

            for (item_idx, item) in values.into_iter().enumerate() {
                let record = Self::from_value(item)?;
                let key = record
                    .id
                    .clone()
                    .or_else(|| record.request_id.clone())
                    .unwrap_or_else(|| format!("line-{line_idx}-item-{item_idx}"));
                records_by_key.insert(key, record);
            }
        }
        Ok(records_by_key.into_values().collect())
    }

    fn from_value(value: serde_json::Value) -> Result<Self> {
        serde_json::from_value(value)
            .map_err(|e| BitrouterError::bad_request(format!("cloud usage record parse: {e}")))
    }
}

impl CloudUsageSummary {
    pub fn from_records(records: &[CloudUsageRecord]) -> Self {
        let mut summary = Self {
            request_count: records.len(),
            ..Self::default()
        };
        for record in records {
            summary.prompt_tokens += record.prompt_tokens;
            summary.completion_tokens += record.completion_tokens;
            if let Some(charge) = record.final_charge_micro_usd {
                summary.settled_request_count += 1;
                summary.final_charge_micro_usd += charge;
            }

            let key = format!("{}/{}", record.provider_id, record.model_id);
            let model = summary.by_model_provider.entry(key).or_default();
            model.request_count += 1;
            model.prompt_tokens += record.prompt_tokens;
            model.completion_tokens += record.completion_tokens;
            if let Some(charge) = record.final_charge_micro_usd {
                model.settled_request_count += 1;
                model.final_charge_micro_usd += charge;
            }
        }
        summary.final_charge_usd = micro_usd_to_usd(summary.final_charge_micro_usd);
        for model in summary.by_model_provider.values_mut() {
            model.final_charge_usd = micro_usd_to_usd(model.final_charge_micro_usd);
        }
        summary
    }
}

impl CostJoinSummary {
    pub fn from_traces_and_usage(
        traces: &[CapturedIngressTrace],
        usage: &[CloudUsageRecord],
    ) -> Self {
        let usage_request_ids = usage
            .iter()
            .filter_map(|record| record.request_id.as_deref())
            .map(ToString::to_string)
            .collect::<BTreeSet<_>>();
        let trace_request_ids = traces
            .iter()
            .filter_map(trace_request_id)
            .collect::<BTreeSet<_>>();

        let matched_trace_count = traces
            .iter()
            .filter_map(trace_request_id)
            .filter(|request_id| usage_request_ids.contains(request_id))
            .count();
        let unmatched_trace_count = traces.len().saturating_sub(matched_trace_count);
        let unmatched_usage_count = usage
            .iter()
            .filter(|record| {
                record
                    .request_id
                    .as_deref()
                    .is_none_or(|request_id| !trace_request_ids.contains(request_id))
            })
            .count();

        Self {
            matched_trace_count,
            unmatched_trace_count,
            unmatched_usage_count,
        }
    }
}

impl WorkflowRunArtifact {
    pub fn build(
        run_label: impl Into<String>,
        traces: &[CapturedIngressTrace],
        usage: &[CloudUsageRecord],
    ) -> Result<Self> {
        Self::build_with_outcomes(run_label, traces, usage, &[])
    }

    pub fn build_with_outcomes(
        run_label: impl Into<String>,
        traces: &[CapturedIngressTrace],
        usage: &[CloudUsageRecord],
        outcomes: &[BenchmarkOutcomeRecord],
    ) -> Result<Self> {
        Self::build_with_decisions(run_label, traces, usage, outcomes, &[])
    }

    pub fn build_with_decisions(
        run_label: impl Into<String>,
        traces: &[CapturedIngressTrace],
        usage: &[CloudUsageRecord],
        outcomes: &[BenchmarkOutcomeRecord],
        decisions: &[PolicyDecisionRecord],
    ) -> Result<Self> {
        let fixtures = TraceArchive::to_replay_fixtures(traces)?;
        let replay = ReplayEvaluator::default().run(&fixtures);
        let shadow_policy = ShadowPolicyEvaluator::default().run(&fixtures);
        let policy_decisions = PolicyDecisionSummary::from_records(decisions);
        let cost = CloudUsageSummary::from_records(usage);
        let cost_join = CostJoinSummary::from_traces_and_usage(traces, usage);
        let reward_join = TraceArchive::join_outcomes(traces, outcomes);
        let route_counts = cost
            .by_model_provider
            .iter()
            .map(|(key, summary)| (key.clone(), summary.request_count))
            .collect();
        Ok(Self {
            run_label: run_label.into(),
            trace_count: traces.len(),
            replay,
            shadow_policy,
            policy_decisions,
            cost,
            cost_join,
            reward_join: reward_join.summary,
            semantic_inadequacy_candidates: reward_join.semantic_inadequacy_candidates,
            route_counts,
        })
    }

    pub fn write_bundle(
        run_label: impl Into<String>,
        output_dir: impl AsRef<Path>,
        traces: &[CapturedIngressTrace],
        usage: &[CloudUsageRecord],
        sanitizer: &TraceSanitizer,
    ) -> Result<Self> {
        Self::write_bundle_with_outcomes(run_label, output_dir, traces, usage, &[], sanitizer)
    }

    pub fn write_bundle_with_outcomes(
        run_label: impl Into<String>,
        output_dir: impl AsRef<Path>,
        traces: &[CapturedIngressTrace],
        usage: &[CloudUsageRecord],
        outcomes: &[BenchmarkOutcomeRecord],
        sanitizer: &TraceSanitizer,
    ) -> Result<Self> {
        Self::write_bundle_with_decisions(
            run_label,
            output_dir,
            traces,
            usage,
            outcomes,
            &[],
            sanitizer,
        )
    }

    pub fn write_bundle_with_decisions(
        run_label: impl Into<String>,
        output_dir: impl AsRef<Path>,
        traces: &[CapturedIngressTrace],
        usage: &[CloudUsageRecord],
        outcomes: &[BenchmarkOutcomeRecord],
        decisions: &[PolicyDecisionRecord],
        sanitizer: &TraceSanitizer,
    ) -> Result<Self> {
        let output_dir = output_dir.as_ref();
        fs::create_dir_all(output_dir).map_err(|e| {
            BitrouterError::internal(format!(
                "workflow run artifact bundle mkdir {}: {e}",
                output_dir.display()
            ))
        })?;

        TraceArchive::write_jsonl(output_dir.join("traces.jsonl"), traces, sanitizer)?;
        write_jsonl_records(output_dir.join("cloud-usage.jsonl"), usage)?;
        write_jsonl_records(output_dir.join("benchmark-outcomes.jsonl"), outcomes)?;
        PolicyDecisionRecord::write_jsonl(output_dir.join("policy-decisions.jsonl"), decisions)?;

        let sanitized_traces = traces
            .iter()
            .map(|trace| sanitizer.sanitize_trace(trace))
            .collect::<Vec<_>>();
        let artifact =
            Self::build_with_decisions(run_label, &sanitized_traces, usage, outcomes, decisions)?;
        artifact.write_json(output_dir.join("run-artifact.json"))?;
        write_pretty_json(
            output_dir.join("shadow-policy.json"),
            &artifact.shadow_policy,
            "workflow shadow policy",
        )?;
        Ok(artifact)
    }

    pub fn write_json(&self, path: impl AsRef<Path>) -> Result<()> {
        write_pretty_json(path, self, "workflow run artifact")
    }
}

fn write_jsonl_records<T: Serialize>(path: impl AsRef<Path>, records: &[T]) -> Result<()> {
    if let Some(parent) = path.as_ref().parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| {
            BitrouterError::internal(format!("workflow jsonl mkdir {}: {e}", parent.display()))
        })?;
    }
    let file = File::create(path.as_ref()).map_err(|e| {
        BitrouterError::internal(format!(
            "workflow jsonl create {}: {e}",
            path.as_ref().display()
        ))
    })?;
    let mut writer = BufWriter::new(file);
    for record in records {
        serde_json::to_writer(&mut writer, record)
            .map_err(|e| BitrouterError::internal(format!("workflow jsonl serialize: {e}")))?;
        writer
            .write_all(b"\n")
            .map_err(|e| BitrouterError::internal(format!("workflow jsonl write: {e}")))?;
    }
    writer
        .flush()
        .map_err(|e| BitrouterError::internal(format!("workflow jsonl flush: {e}")))
}

fn write_pretty_json<T: Serialize>(path: impl AsRef<Path>, value: &T, label: &str) -> Result<()> {
    if let Some(parent) = path.as_ref().parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| {
            BitrouterError::internal(format!("{label} mkdir {}: {e}", parent.display()))
        })?;
    }
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| BitrouterError::internal(format!("{label} serialize: {e}")))?;
    fs::write(path.as_ref(), text).map_err(|e| {
        BitrouterError::internal(format!("{label} write {}: {e}", path.as_ref().display()))
    })
}

fn read_jsonl_values(path: &Path) -> Result<Vec<serde_json::Value>> {
    let file = File::open(path).map_err(|e| {
        BitrouterError::internal(format!("workflow jsonl open {}: {e}", path.display()))
    })?;
    let reader = BufReader::new(file);
    let mut values = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| {
            BitrouterError::internal(format!("workflow jsonl read {}: {e}", path.display()))
        })?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value = serde_json::from_str(trimmed).map_err(|e| {
            BitrouterError::bad_request(format!(
                "workflow jsonl parse {} line {}: {e}",
                path.display(),
                idx + 1
            ))
        })?;
        values.push(value);
    }
    Ok(values)
}

fn trace_request_id(trace: &CapturedIngressTrace) -> Option<String> {
    [
        "x-bitrouter-cloud-request-id",
        "x-bitrouter-request-id",
        "x-request-id",
    ]
    .into_iter()
    .find_map(|name| header_value(&trace.headers, name))
}

fn header_value(headers: &BTreeMap<String, String>, name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn micro_usd_to_usd(value: u64) -> f64 {
    value as f64 / 1_000_000.0
}
