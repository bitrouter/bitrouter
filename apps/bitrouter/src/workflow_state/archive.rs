use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use bitrouter_cloud_sdk::settlement::{SettlementReceipt, SettlementState};
use bitrouter_sdk::language_model::UsageOrigin;
use bitrouter_sdk::{BitrouterError, Result};
use serde::{Deserialize, Serialize};

use crate::metering::pricing::MAX_TRUSTED_TOKENS;
use crate::metering::{ChargeEvidence, ChargeStatus, PricingSource, ReconciliationStatus};
use crate::workflow_state::decision::{PolicyDecisionRecord, PolicyDecisionSummary};
use crate::workflow_state::fixture::WorkflowTraceFixture;
use crate::workflow_state::ir::{AgentRole, ContextTransition, HarnessId};
use crate::workflow_state::real_trace::{CapturedIngressTrace, TraceSanitizer};
use crate::workflow_state::replay::{ReplayEvaluator, ReplaySummary};
use crate::workflow_state::reward::{
    BenchmarkOutcomeRecord, RewardJoin, RewardJoinSummary, SemanticInadequacyCandidate,
    SemanticOutcomeCandidate,
};
use crate::workflow_state::session::identity_fingerprint;
use crate::workflow_state::shadow_policy::{ShadowPolicyEvaluator, ShadowPolicySummary};

pub struct TraceArchive;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CloudUsageRecord {
    pub id: Option<String>,
    pub request_id: Option<String>,
    pub provider_id: String,
    pub model_id: String,
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
    #[serde(default)]
    pub uncached_input_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub usage_origin: UsageOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_usage: Option<serde_json::Value>,
    pub final_charge_micro_usd: Option<u64>,
    #[serde(default)]
    pub charge_status: ChargeStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charge_evidence: Option<ChargeEvidence>,
    #[serde(default)]
    pub reconciliation_status: ReconciliationStatus,
    #[serde(default)]
    pub reconciliation_attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authoritative_receipt: Option<serde_json::Value>,
    pub status: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CloudUsageSummary {
    pub request_count: usize,
    pub settled_request_count: usize,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub reasoning_tokens: u64,
    pub uncached_input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub unknown_charge_count: usize,
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
    pub reasoning_tokens: u64,
    pub uncached_input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub unknown_charge_count: usize,
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
    pub semantic_policy_transition_candidates: Vec<SemanticPolicyTransitionCandidate>,
    pub route_counts: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticPolicyTransitionCandidate {
    pub trace_id: String,
    pub request_id: String,
    pub session_key: String,
    pub task_id: String,
    pub reward: f64,
    pub failed_reason: Option<String>,
    pub request_key: String,
    #[serde(default)]
    pub ledger_key: Option<String>,
    pub workflow_state: String,
    pub static_tier: Option<String>,
    pub selected_tier: Option<String>,
    pub tier_transition: Option<String>,
    pub static_model: Option<String>,
    pub selected_model: Option<String>,
    pub model_transition: Option<String>,
    pub reason: String,
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
            summary.reasoning_tokens += record.reasoning_tokens;
            summary.uncached_input_tokens += record.uncached_input_tokens;
            summary.cache_read_tokens += record.cache_read_tokens;
            summary.cache_write_tokens += record.cache_write_tokens;
            summary.output_tokens += record.output_tokens;
            accumulate_charge(
                record,
                &mut summary.settled_request_count,
                &mut summary.unknown_charge_count,
                &mut summary.final_charge_micro_usd,
            );

            let key = format!("{}/{}", record.provider_id, record.model_id);
            let model = summary.by_model_provider.entry(key).or_default();
            model.request_count += 1;
            model.prompt_tokens += record.prompt_tokens;
            model.completion_tokens += record.completion_tokens;
            model.reasoning_tokens += record.reasoning_tokens;
            model.uncached_input_tokens += record.uncached_input_tokens;
            model.cache_read_tokens += record.cache_read_tokens;
            model.cache_write_tokens += record.cache_write_tokens;
            model.output_tokens += record.output_tokens;
            accumulate_charge(
                record,
                &mut model.settled_request_count,
                &mut model.unknown_charge_count,
                &mut model.final_charge_micro_usd,
            );
        }
        summary.final_charge_usd = micro_usd_to_usd(summary.final_charge_micro_usd);
        for model in summary.by_model_provider.values_mut() {
            model.final_charge_usd = micro_usd_to_usd(model.final_charge_micro_usd);
        }
        summary
    }
}

fn accumulate_charge(
    record: &CloudUsageRecord,
    settled: &mut usize,
    unknown: &mut usize,
    total: &mut u64,
) {
    match record.final_charge_micro_usd {
        Some(charge) => {
            *settled += 1;
            *total += charge;
        }
        None if record.charge_status == ChargeStatus::NotCharged => *settled += 1,
        None => *unknown += 1,
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
    /// Validate the usage side of a benchmark bundle before any artifact files
    /// are written. Analytical builds with no usage remain supported; once a
    /// usage snapshot is supplied it must be complete and auditable.
    pub fn validate_benchmark_integrity(
        traces: &[CapturedIngressTrace],
        usage: &[CloudUsageRecord],
    ) -> Result<()> {
        if usage.is_empty() {
            return Ok(());
        }

        let mut trace_ids = BTreeSet::new();
        for trace in traces {
            let request_id = trace_request_id(trace).ok_or_else(|| {
                BitrouterError::bad_request(format!(
                    "benchmark integrity: trace {} has no request id",
                    trace.id
                ))
            })?;
            if !trace_ids.insert(request_id.clone()) {
                return Err(BitrouterError::bad_request(format!(
                    "benchmark integrity: duplicate trace request id {request_id}"
                )));
            }
        }

        let mut usage_ids = BTreeSet::new();
        for record in usage {
            let request_id = record.request_id.as_deref().ok_or_else(|| {
                BitrouterError::bad_request("benchmark integrity: usage row has no request id")
            })?;
            if !usage_ids.insert(request_id.to_string()) {
                return Err(BitrouterError::bad_request(format!(
                    "benchmark integrity: duplicate usage request id {request_id}"
                )));
            }
            validate_usage_evidence(record, request_id)?;
        }

        if trace_ids != usage_ids {
            let missing_usage = trace_ids
                .difference(&usage_ids)
                .cloned()
                .collect::<Vec<_>>();
            let unmatched_usage = usage_ids
                .difference(&trace_ids)
                .cloned()
                .collect::<Vec<_>>();
            return Err(BitrouterError::bad_request(format!(
                "benchmark integrity: trace/usage request ids differ; missing_usage={missing_usage:?}, unmatched_usage={unmatched_usage:?}"
            )));
        }
        Ok(())
    }

    /// Validate trace, settlement, and policy-decision joins for a benchmark
    /// bundle. Decision capture is optional, but when present it must cover the
    /// trace set exactly and Terminus 2 identity must be explicit and complete.
    pub fn validate_benchmark_integrity_with_decisions(
        traces: &[CapturedIngressTrace],
        usage: &[CloudUsageRecord],
        decisions: &[PolicyDecisionRecord],
    ) -> Result<()> {
        Self::validate_benchmark_integrity(traces, usage)?;
        if decisions.is_empty() {
            return Ok(());
        }

        let mut traces_by_request_id = BTreeMap::new();
        for trace in traces {
            let request_id = trace_request_id(trace).ok_or_else(|| {
                BitrouterError::bad_request(format!(
                    "benchmark integrity: trace {} has no request id for decision join",
                    trace.id
                ))
            })?;
            if traces_by_request_id
                .insert(request_id.clone(), trace)
                .is_some()
            {
                return Err(BitrouterError::bad_request(format!(
                    "benchmark integrity: duplicate trace request id {request_id}"
                )));
            }
        }

        let mut decisions_by_request_id = BTreeMap::new();
        for decision in decisions {
            let request_id = decision.request_id.as_deref().ok_or_else(|| {
                BitrouterError::bad_request(
                    "benchmark integrity: policy decision has no request id",
                )
            })?;
            if decisions_by_request_id
                .insert(request_id.to_string(), decision)
                .is_some()
            {
                return Err(BitrouterError::bad_request(format!(
                    "benchmark integrity: duplicate policy decision request id {request_id}"
                )));
            }
        }

        let trace_ids = traces_by_request_id
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let decision_ids = decisions_by_request_id
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        if trace_ids != decision_ids {
            let missing_decisions = trace_ids
                .difference(&decision_ids)
                .cloned()
                .collect::<Vec<_>>();
            let unmatched_decisions = decision_ids
                .difference(&trace_ids)
                .cloned()
                .collect::<Vec<_>>();
            return Err(BitrouterError::bad_request(format!(
                "benchmark integrity: trace/decision request ids differ; missing_decisions={missing_decisions:?}, unmatched_decisions={unmatched_decisions:?}"
            )));
        }

        for (request_id, trace) in traces_by_request_id {
            if trace.harness == HarnessId::Terminus2 {
                let decision = decisions_by_request_id.get(&request_id).ok_or_else(|| {
                    BitrouterError::bad_request(format!(
                        "benchmark integrity: trace {request_id} has no policy decision"
                    ))
                })?;
                validate_terminus_identity(trace, decision, &request_id)?;
            }
        }
        Ok(())
    }

    /// Validate every benchmark-grade join before an artifact directory is
    /// accepted. Outcomes are optional for analytical bundles; when supplied,
    /// every trace and outcome must join through an explicit session/trial key.
    pub fn validate_complete_benchmark_integrity(
        traces: &[CapturedIngressTrace],
        usage: &[CloudUsageRecord],
        outcomes: &[BenchmarkOutcomeRecord],
        decisions: &[PolicyDecisionRecord],
    ) -> Result<()> {
        Self::validate_benchmark_integrity_with_decisions(traces, usage, decisions)?;
        if outcomes.is_empty() {
            return Ok(());
        }
        let reward_join = TraceArchive::join_outcomes(traces, outcomes);
        if reward_join.summary.unmatched_trace_count != 0
            || reward_join.summary.unmatched_outcome_count != 0
            || reward_join.summary.matched_trace_count != traces.len()
        {
            return Err(BitrouterError::bad_request(format!(
                "benchmark integrity: outcome join incomplete; matched_traces={}, unmatched_traces={}, unmatched_outcomes={}",
                reward_join.summary.matched_trace_count,
                reward_join.summary.unmatched_trace_count,
                reward_join.summary.unmatched_outcome_count
            )));
        }
        Ok(())
    }

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
        let replay = ReplayEvaluator.run(&fixtures);
        let shadow_policy = ShadowPolicyEvaluator.run(&fixtures);
        let policy_decisions = PolicyDecisionSummary::from_records(decisions);
        let cost = CloudUsageSummary::from_records(usage);
        let cost_join = CostJoinSummary::from_traces_and_usage(traces, usage);
        let reward_join = TraceArchive::join_outcomes(traces, outcomes);
        let semantic_policy_transition_candidates = semantic_policy_transition_candidates(
            &reward_join.semantic_outcome_candidates,
            traces,
            decisions,
        );
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
            semantic_policy_transition_candidates,
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
        Self::validate_complete_benchmark_integrity(traces, usage, outcomes, decisions)?;
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

fn validate_usage_evidence(record: &CloudUsageRecord, request_id: &str) -> Result<()> {
    match record.reconciliation_status {
        ReconciliationStatus::Pending => {
            return Err(BitrouterError::bad_request(format!(
                "benchmark integrity: usage {request_id} reconciliation is pending"
            )));
        }
        ReconciliationStatus::Unknown => {
            return Err(BitrouterError::bad_request(format!(
                "benchmark integrity: usage {request_id} reconciliation is unknown"
            )));
        }
        ReconciliationStatus::NotCharged => {
            validate_authoritative_receipt(record, request_id, SettlementState::NotCharged)?;
            if record.usage_origin != UsageOrigin::AuthoritativeReceipt
                || record.charge_status != ChargeStatus::NotCharged
                || record.final_charge_micro_usd.is_some()
                || record.prompt_tokens != 0
                || record.completion_tokens != 0
            {
                return Err(BitrouterError::bad_request(format!(
                    "benchmark integrity: usage {request_id} has invalid not-charged evidence"
                )));
            }
            return Ok(());
        }
        ReconciliationStatus::Computed => {
            validate_authoritative_receipt(record, request_id, SettlementState::Computed)?;
            if record.usage_origin != UsageOrigin::AuthoritativeReceipt {
                return Err(BitrouterError::bad_request(format!(
                    "benchmark integrity: usage {request_id} is not from its authoritative receipt"
                )));
            }
        }
        ReconciliationStatus::NotApplicable => {
            if record.usage_origin != UsageOrigin::ProviderReported {
                return Err(BitrouterError::bad_request(format!(
                    "benchmark integrity: usage {request_id} is not provider reported"
                )));
            }
        }
    }
    if record.raw_usage.is_none() {
        return Err(BitrouterError::bad_request(format!(
            "benchmark integrity: usage {request_id} has no raw usage evidence"
        )));
    }
    if record.charge_status != ChargeStatus::Computed {
        return Err(BitrouterError::bad_request(format!(
            "benchmark integrity: usage {request_id} charge is not computed"
        )));
    }
    let charge = record.final_charge_micro_usd.ok_or_else(|| {
        BitrouterError::bad_request(format!(
            "benchmark integrity: usage {request_id} has no final charge"
        ))
    })?;
    let evidence = record.charge_evidence.as_ref().ok_or_else(|| {
        BitrouterError::bad_request(format!(
            "benchmark integrity: usage {request_id} has no charge evidence"
        ))
    })?;
    if evidence.status != ChargeStatus::Computed
        || evidence.charge_micro_usd != Some(charge as i64)
        || evidence.pricing_source == PricingSource::Unknown
        || !valid_pricing_version(&evidence.pricing_version)
    {
        return Err(BitrouterError::bad_request(format!(
            "benchmark integrity: usage {request_id} has invalid pricing evidence"
        )));
    }
    let recomputed_charge = recompute_evidence_charge(evidence).ok_or_else(|| {
        BitrouterError::bad_request(format!(
            "benchmark integrity: usage {request_id} has invalid effective rates"
        ))
    })?;
    if recomputed_charge != charge as i64 {
        return Err(BitrouterError::bad_request(format!(
            "benchmark integrity: usage {request_id} charge does not match effective rates"
        )));
    }
    let normalized = &evidence.normalized_usage;
    if normalized.uncached_input_tokens != record.uncached_input_tokens
        || normalized.cache_read_tokens != record.cache_read_tokens
        || normalized.cache_write_tokens != record.cache_write_tokens
        || normalized.output_tokens != record.output_tokens
        || normalized.reasoning_tokens != record.reasoning_tokens
        || normalized
            .uncached_input_tokens
            .checked_add(normalized.cache_read_tokens)
            .and_then(|tokens| tokens.checked_add(normalized.cache_write_tokens))
            != Some(record.prompt_tokens)
        || normalized
            .output_tokens
            .checked_add(normalized.reasoning_tokens)
            != Some(record.completion_tokens)
    {
        return Err(BitrouterError::bad_request(format!(
            "benchmark integrity: usage {request_id} has inconsistent normalized buckets"
        )));
    }
    Ok(())
}

fn validate_authoritative_receipt(
    record: &CloudUsageRecord,
    request_id: &str,
    expected_state: SettlementState,
) -> Result<()> {
    let receipt: SettlementReceipt = serde_json::from_value(
        record.authoritative_receipt.clone().ok_or_else(|| {
            BitrouterError::bad_request(format!(
                "benchmark integrity: usage {request_id} has no authoritative receipt"
            ))
        })?,
    )
    .map_err(|error| {
        BitrouterError::bad_request(format!(
            "benchmark integrity: usage {request_id} has malformed authoritative receipt: {error}"
        ))
    })?;
    if receipt.request_id != request_id || receipt.state != expected_state {
        return Err(BitrouterError::bad_request(format!(
            "benchmark integrity: usage {request_id} authoritative receipt identity/state mismatch"
        )));
    }
    let expected_charge = record.final_charge_micro_usd.map(|charge| charge as i64);
    if receipt.final_charge_micro_usd != expected_charge
        || receipt
            .provider_id
            .as_deref()
            .is_some_and(|provider| provider != record.provider_id)
        || receipt
            .model_id
            .as_deref()
            .is_some_and(|model| model != record.model_id)
        || u64::try_from(receipt.usage.uncached_input_tokens).ok()
            != Some(record.uncached_input_tokens)
        || u64::try_from(receipt.usage.cache_read_tokens).ok() != Some(record.cache_read_tokens)
        || u64::try_from(receipt.usage.cache_write_tokens).ok() != Some(record.cache_write_tokens)
        || u64::try_from(receipt.usage.output_tokens).ok() != Some(record.output_tokens)
        || u64::try_from(receipt.usage.reasoning_tokens).ok() != Some(record.reasoning_tokens)
    {
        return Err(BitrouterError::bad_request(format!(
            "benchmark integrity: usage {request_id} does not match authoritative receipt"
        )));
    }
    Ok(())
}

fn recompute_evidence_charge(evidence: &ChargeEvidence) -> Option<i64> {
    let normalized = &evidence.normalized_usage;
    let rates = &evidence.effective_rates;
    let buckets = [
        (
            normalized.uncached_input_tokens,
            rates.uncached_input_micro_usd_per_token,
        ),
        (
            normalized.cache_read_tokens,
            rates.cache_read_micro_usd_per_token,
        ),
        (
            normalized.cache_write_tokens,
            rates.cache_write_micro_usd_per_token,
        ),
        (normalized.output_tokens, rates.output_micro_usd_per_token),
        (
            normalized.reasoning_tokens,
            rates.output_micro_usd_per_token,
        ),
    ];
    let mut charge = 0.0;
    for (tokens, rate) in buckets {
        if tokens == 0 {
            continue;
        }
        let rate = rate.filter(|value| value.is_finite() && *value >= 0.0)?;
        charge += tokens.min(MAX_TRUSTED_TOKENS) as f64 * rate;
    }
    charge.is_finite().then_some(charge.round().max(0.0) as i64)
}

fn validate_terminus_identity(
    trace: &CapturedIngressTrace,
    decision: &PolicyDecisionRecord,
    request_id: &str,
) -> Result<()> {
    let identity = &decision.workflow_identity;
    if identity.role == AgentRole::Unknown
        || identity.parent_session_id.is_none()
        || identity.agent_session_id.is_none()
        || identity.benchmark_run_id.is_none()
        || identity.trial_id.is_none()
        || identity.source != "explicit_headers"
        || !valid_pricing_version(&identity.fingerprint)
    {
        return Err(BitrouterError::bad_request(format!(
            "benchmark integrity: Terminus 2 decision {request_id} has incomplete workflow identity"
        )));
    }
    let expected_fingerprint = identity_fingerprint(
        identity.benchmark_run_id.as_deref(),
        identity.trial_id.as_deref(),
        identity.parent_session_id.as_deref(),
        identity.context_epoch,
    );
    if identity.fingerprint != expected_fingerprint {
        return Err(BitrouterError::bad_request(format!(
            "benchmark integrity: Terminus 2 decision {request_id} has invalid session fingerprint"
        )));
    }

    let expected = [
        (
            "x-bitrouter-benchmark-run-id",
            identity.benchmark_run_id.as_deref().unwrap_or_default(),
        ),
        (
            "x-bitrouter-trial-id",
            identity.trial_id.as_deref().unwrap_or_default(),
        ),
        (
            "x-bitrouter-parent-session-id",
            identity.parent_session_id.as_deref().unwrap_or_default(),
        ),
        (
            "x-bitrouter-agent-session-id",
            identity.agent_session_id.as_deref().unwrap_or_default(),
        ),
        ("x-bitrouter-agent-role", identity.role.as_str()),
        (
            "x-bitrouter-session-fingerprint",
            identity.fingerprint.as_str(),
        ),
    ];
    for (header, expected_value) in expected {
        if header_value(&trace.headers, header).as_deref() != Some(expected_value) {
            return Err(BitrouterError::bad_request(format!(
                "benchmark integrity: Terminus 2 trace/decision identity mismatch for {request_id} header {header}"
            )));
        }
    }
    if header_value(&trace.headers, "x-bitrouter-context-epoch")
        .and_then(|value| value.parse::<u32>().ok())
        != Some(identity.context_epoch)
        || header_value(&trace.headers, "x-bitrouter-context-transition").as_deref()
            != Some(context_transition_value(identity.transition))
    {
        return Err(BitrouterError::bad_request(format!(
            "benchmark integrity: Terminus 2 trace/decision context mismatch for {request_id}"
        )));
    }
    Ok(())
}

fn context_transition_value(transition: ContextTransition) -> &'static str {
    match transition {
        ContextTransition::None => "none",
        ContextTransition::CompactionStart => "compaction_start",
        ContextTransition::CompactionContinuation => "compaction_continuation",
        ContextTransition::MainResume => "main_resume",
    }
}

fn valid_pricing_version(version: &str) -> bool {
    version.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
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

fn semantic_policy_transition_candidates(
    semantic_candidates: &[SemanticOutcomeCandidate],
    traces: &[CapturedIngressTrace],
    decisions: &[PolicyDecisionRecord],
) -> Vec<SemanticPolicyTransitionCandidate> {
    let request_id_by_trace_id = traces
        .iter()
        .filter_map(|trace| {
            trace_request_id(trace).map(|request_id| (trace.id.clone(), request_id))
        })
        .collect::<BTreeMap<_, _>>();
    let decisions_by_request_id = decisions
        .iter()
        .filter_map(|decision| {
            decision
                .request_id
                .as_ref()
                .map(|request_id| (request_id.clone(), decision))
        })
        .collect::<BTreeMap<_, _>>();

    semantic_candidates
        .iter()
        .filter_map(|candidate| {
            let request_id = request_id_by_trace_id.get(&candidate.trace_id)?;
            let decision = decisions_by_request_id.get(request_id)?;
            let tier_changed = changed(&decision.static_tier, &decision.selected_tier);
            let model_changed = changed(&decision.static_model, &decision.selected_model);
            if !tier_changed && !model_changed {
                return None;
            }
            Some(SemanticPolicyTransitionCandidate {
                trace_id: candidate.trace_id.clone(),
                request_id: request_id.clone(),
                session_key: candidate.session_key.clone(),
                task_id: candidate.task_id.clone(),
                reward: candidate.reward,
                failed_reason: candidate.failed_reason.clone(),
                request_key: decision.request_key.clone(),
                ledger_key: decision.ledger_key.clone(),
                workflow_state: decision.workflow_state.clone(),
                static_tier: decision.static_tier.clone(),
                selected_tier: decision.selected_tier.clone(),
                tier_transition: transition_option(&decision.static_tier, &decision.selected_tier),
                static_model: decision.static_model.clone(),
                selected_model: decision.selected_model.clone(),
                model_transition: transition_option(
                    &decision.static_model,
                    &decision.selected_model,
                ),
                reason: decision.reason.clone(),
            })
        })
        .collect()
}

fn changed(left: &Option<String>, right: &Option<String>) -> bool {
    left.is_some() && right.is_some() && left != right
}

fn transition_option(left: &Option<String>, right: &Option<String>) -> Option<String> {
    match (left.as_deref(), right.as_deref()) {
        (Some(left), Some(right)) => Some(format!("{left} -> {right}")),
        _ => None,
    }
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
