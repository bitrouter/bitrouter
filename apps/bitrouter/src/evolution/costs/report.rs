//! Owner-scoped, request-union cost views over durable jobs and current metering.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use bitrouter_sdk::language_model::UsageOrigin;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};

use super::{JudgeAttempt, JudgeCosts, KIND, LOOKUP_KIND, RequestOwner, VERSION};
use crate::acp_trajectory::SessionIdentity;
use crate::evolution::jobs::JudgeJob;
use crate::evolution::runtime::ExecutionOutcome;
use crate::evolution::store::records;
use crate::metering::db::ReconciliationStatus;
use crate::metering::entities::requests;
use crate::metering::pricing::{ChargeStatus, PricingSource};
use crate::metering::store::MeteringUsageRecord;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostSummary {
    pub requests: usize,
    pub incomplete_requests: usize,
    /// Sum of available metering estimates. This may be revised by receipts;
    /// it is not an invoice, a confidence bound, or a complete price.
    pub known_cost_micro_usd: u64,
    /// Present only when every reserved request has complete cost evidence.
    pub total_cost_micro_usd: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptCost {
    pub request_id: String,
    pub retry: bool,
    pub reserved_at: Option<String>,
    pub started_at: Option<String>,
    pub settled_at: Option<String>,
    pub outcome: Option<ExecutionOutcome>,
    pub upstream_attempts: Option<usize>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub charge_status: Option<ChargeStatus>,
    pub usage_origin: Option<UsageOrigin>,
    pub observed_usage_origin: Option<UsageOrigin>,
    pub pricing_source: Option<PricingSource>,
    pub reconciliation_status: Option<ReconciliationStatus>,
    pub known_cost_micro_usd: Option<u64>,
    pub total_cost_micro_usd: Option<u64>,
    pub incomplete_reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobCosts {
    pub identity: SessionIdentity,
    pub checkpoint_id: String,
    pub model: String,
    pub job_details_available: bool,
    pub summary: CostSummary,
    /// Subset of summary, never an additional charge to add to it.
    pub retries: CostSummary,
    pub attempts: Vec<AttemptCost>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCosts {
    pub identity: SessionIdentity,
    pub summary: CostSummary,
    pub retries: CostSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeCostReport {
    pub observed_at: String,
    pub summary: CostSummary,
    pub retries: CostSummary,
    pub without_job_details: CostSummary,
    pub sessions: Vec<SessionCosts>,
    pub jobs: BTreeMap<String, JobCosts>,
}

fn summarize<'a>(attempts: impl IntoIterator<Item = &'a AttemptCost>) -> Result<CostSummary> {
    let mut summary = CostSummary {
        requests: 0,
        incomplete_requests: 0,
        known_cost_micro_usd: 0,
        total_cost_micro_usd: Some(0),
    };
    let mut seen = BTreeSet::new();
    for attempt in attempts {
        if !seen.insert(&attempt.request_id) {
            continue;
        }
        summary.requests += 1;
        summary.known_cost_micro_usd = summary
            .known_cost_micro_usd
            .checked_add(attempt.known_cost_micro_usd.unwrap_or(0))
            .context("judge cost sum overflow")?;
        if attempt.total_cost_micro_usd.is_none() {
            summary.incomplete_requests += 1;
        }
        summary.total_cost_micro_usd =
            match (summary.total_cost_micro_usd, attempt.total_cost_micro_usd) {
                (Some(total), Some(cost)) => {
                    Some(total.checked_add(cost).context("judge cost sum overflow")?)
                }
                _ => None,
            };
    }
    Ok(summary)
}

fn metered_cost(row: &MeteringUsageRecord) -> Option<u64> {
    let evidence = row.charge_evidence.as_ref()?;
    if evidence.status != row.charge_status {
        return None;
    }
    match row.charge_status {
        ChargeStatus::Computed => u64::try_from(evidence.charge_micro_usd?).ok(),
        ChargeStatus::NotCharged
            if row.reconciliation_status == ReconciliationStatus::NotCharged
                && row.usage_origin == UsageOrigin::AuthoritativeReceipt =>
        {
            Some(0)
        }
        _ => None,
    }
}

fn attempt_cost(
    request_id: &str,
    retry: bool,
    attempt: Option<&JudgeAttempt>,
    metered: Option<&MeteringUsageRecord>,
) -> AttemptCost {
    let mut reasons = Vec::new();
    let known = metered.and_then(metered_cost);
    let authoritative = metered.is_some_and(|row| {
        row.usage_origin == UsageOrigin::AuthoritativeReceipt
            && matches!(
                row.reconciliation_status,
                ReconciliationStatus::Computed | ReconciliationStatus::NotCharged
            )
            && row.authoritative_receipt.is_some()
    });
    let no_upstream = attempt.is_some_and(|a| {
        a.started_at.is_some()
            && a.settled_at.is_some()
            && a.outcome == Some(ExecutionOutcome::Failed)
            && a.hops.is_empty()
    });
    let total = if no_upstream && known.is_none_or(|amount| amount == 0) {
        // A durable, terminal pre-dispatch rejection proves that no model ran.
        Some(0)
    } else {
        match attempt {
            None => reasons.push("dispatch_inventory_missing".into()),
            Some(attempt) => {
                if attempt.started_at.is_none() {
                    reasons.push("dispatch_not_confirmed".into());
                }
                if attempt.settled_at.is_none() || attempt.outcome.is_none() {
                    reasons.push("terminal_settlement_missing".into());
                }
                if !authoritative
                    && attempt.observed_usage_origin != Some(UsageOrigin::ProviderReported)
                {
                    reasons.push("pipeline_usage_not_observed".into());
                }
                match attempt.hops.as_slice() {
                    [hop] => {
                        if !matches!(hop.status.as_str(), "completed" | "failed") {
                            reasons.push("upstream_attempt_unfinished".into());
                        }
                        if let Some(row) = metered
                            && !authoritative
                            && (row.provider_id != hop.provider || row.model_id != hop.model)
                        {
                            reasons.push("metered_target_mismatch".into());
                        }
                    }
                    [] => reasons.push("upstream_attempt_coverage_unknown".into()),
                    _ => reasons.push("earlier_upstream_attempt_costs_missing".into()),
                }
            }
        }
        match metered {
            None => reasons.push("metering_record_missing".into()),
            Some(row) => {
                if known.is_none() {
                    reasons.push("charge_evidence_unknown".into());
                }
                if !matches!(
                    row.usage_origin,
                    UsageOrigin::ProviderReported | UsageOrigin::AuthoritativeReceipt
                ) {
                    reasons.push("usage_not_observed".into());
                }
                match row.reconciliation_status {
                    ReconciliationStatus::Pending => reasons.push("reconciliation_pending".into()),
                    ReconciliationStatus::Unknown => reasons.push("reconciliation_unknown".into()),
                    _ => {}
                }
            }
        }
        reasons.is_empty().then_some(known).flatten()
    };
    AttemptCost {
        request_id: request_id.into(),
        retry,
        reserved_at: attempt.map(|a| a.reserved_at.clone()),
        started_at: attempt.and_then(|a| a.started_at.clone()),
        settled_at: attempt.and_then(|a| a.settled_at.clone()),
        outcome: attempt.and_then(|a| a.outcome),
        upstream_attempts: attempt
            .filter(|a| a.settled_at.is_some())
            .map(|a| a.hops.len()),
        provider: metered.map(|r| r.provider_id.clone()),
        model: metered.map(|r| r.model_id.clone()),
        charge_status: metered.map(|r| r.charge_status),
        usage_origin: metered.map(|r| r.usage_origin),
        observed_usage_origin: attempt.and_then(|a| a.observed_usage_origin),
        pricing_source: metered.and_then(|r| r.charge_evidence.as_ref().map(|e| e.pricing_source)),
        reconciliation_status: metered.map(|r| r.reconciliation_status),
        known_cost_micro_usd: if no_upstream { total.or(known) } else { known },
        total_cost_micro_usd: total,
        incomplete_reasons: reasons,
    }
}

struct ReportInput {
    identity: SessionIdentity,
    checkpoint_id: String,
    model: String,
    job_details_available: bool,
}

impl JudgeCosts {
    /// Unions retained cost reservations with current jobs, including legacy
    /// attempts without an inventory. Source deletion removes judge content,
    /// but content-free cost metadata remains owned and visible.
    pub async fn report(&self, owner: &str, jobs: &[JudgeJob]) -> Result<JudgeCostReport> {
        let store = self.store(owner)?;
        let lookup = self.lookup()?;
        let mut inputs = BTreeMap::new();
        let mut owned = BTreeMap::new();
        for job in jobs {
            ensure!(job.identity.owner == owner, "judge cost owner mismatch");
            ensure!(
                inputs
                    .insert(
                        job.job_id.clone(),
                        ReportInput {
                            identity: job.identity.clone(),
                            checkpoint_id: job.checkpoint_id.clone(),
                            model: job.model.clone(),
                            job_details_available: true,
                        }
                    )
                    .is_none(),
                "duplicate judge job identity"
            );
            let mut seen = BTreeSet::new();
            for (index, id) in job.request_ids.iter().enumerate() {
                if !seen.insert(id) {
                    continue;
                }
                ensure!(
                    owned
                        .insert(id.clone(), (job.job_id.clone(), index != 0))
                        .is_none(),
                    "judge request belongs to conflicting jobs"
                );
            }
        }
        let mut inventory = BTreeMap::new();
        for (record_id, _, attempt) in store.list::<JudgeAttempt>(KIND).await? {
            ensure!(
                attempt.version == VERSION
                    && attempt.identity.owner == owner
                    && attempt.attempt_number > 0
                    && record_id == store.id(KIND, &attempt.request_id)?,
                "judge cost reservation mismatch"
            );
            let input = inputs
                .entry(attempt.job_id.clone())
                .or_insert_with(|| ReportInput {
                    identity: attempt.identity.clone(),
                    checkpoint_id: attempt.checkpoint_id.clone(),
                    model: attempt.model.clone(),
                    job_details_available: false,
                });
            ensure!(
                input.identity == attempt.identity
                    && input.checkpoint_id == attempt.checkpoint_id
                    && input.model == attempt.model,
                "judge cost job metadata mismatch"
            );
            let membership = (attempt.job_id.clone(), attempt.attempt_number > 1);
            if let Some(previous) = owned.insert(attempt.request_id.clone(), membership.clone()) {
                ensure!(
                    previous == membership,
                    "judge request attempt membership mismatch"
                );
            }
            inventory.insert(attempt.request_id.clone(), attempt);
        }
        let ids: Vec<_> = owned.keys().cloned().collect();
        let mut metering = BTreeMap::new();
        // Bounded SQL parameter lists work on every configured database backend.
        for chunk in ids.chunks(128) {
            let record_ids = chunk
                .iter()
                .map(|id| lookup.id(LOOKUP_KIND, id))
                .collect::<Result<Vec<_>>>()?;
            for row in records::Entity::find()
                .filter(records::Column::ScopeId.eq(&lookup.scope_id))
                .filter(records::Column::Kind.eq(LOOKUP_KIND))
                .filter(records::Column::RecordId.is_in(record_ids))
                .all(&self.db)
                .await?
            {
                let binding: RequestOwner = serde_json::from_str(&row.body)?;
                ensure!(
                    binding.owner == owner
                        && owned.contains_key(&binding.request_id)
                        && row.record_id == lookup.id(LOOKUP_KIND, &binding.request_id)?,
                    "judge request owner binding mismatch"
                );
            }
            for row in requests::Entity::find()
                .filter(requests::Column::RequestId.is_in(chunk.iter().cloned()))
                .all(&self.db)
                .await?
            {
                ensure!(
                    row.api_key_id == "local" && row.user_id == "local",
                    "judge metering caller mismatch"
                );
                metering.insert(row.request_id.clone(), MeteringUsageRecord::from(row));
            }
        }
        let mut attempts_by_job: BTreeMap<String, Vec<AttemptCost>> = BTreeMap::new();
        for (id, (job_id, retry)) in owned {
            attempts_by_job
                .entry(job_id)
                .or_default()
                .push(attempt_cost(
                    &id,
                    retry,
                    inventory.get(&id),
                    metering.get(&id),
                ));
        }
        let mut report_jobs = BTreeMap::new();
        let mut sessions: BTreeMap<String, (SessionIdentity, Vec<AttemptCost>)> = BTreeMap::new();
        for (job_id, input) in inputs {
            let mut attempts = attempts_by_job.remove(&job_id).unwrap_or_default();
            attempts.sort_by(|a, b| {
                (&a.reserved_at, &a.request_id).cmp(&(&b.reserved_at, &b.request_id))
            });
            sessions
                .entry(input.identity.key()?)
                .or_insert_with(|| (input.identity.clone(), vec![]))
                .1
                .extend(attempts.iter().cloned());
            report_jobs.insert(
                job_id,
                JobCosts {
                    identity: input.identity,
                    checkpoint_id: input.checkpoint_id,
                    model: input.model,
                    job_details_available: input.job_details_available,
                    summary: summarize(&attempts)?,
                    retries: summarize(attempts.iter().filter(|a| a.retry))?,
                    attempts,
                },
            );
        }
        let all_attempts = || report_jobs.values().flat_map(|job| &job.attempts);
        Ok(JudgeCostReport {
            observed_at: chrono::Utc::now().to_rfc3339(),
            summary: summarize(all_attempts())?,
            retries: summarize(all_attempts().filter(|a| a.retry))?,
            without_job_details: summarize(
                report_jobs
                    .values()
                    .filter(|job| !job.job_details_available)
                    .flat_map(|job| &job.attempts),
            )?,
            sessions: sessions
                .into_values()
                .map(|(identity, attempts)| {
                    Ok(SessionCosts {
                        identity,
                        summary: summarize(&attempts)?,
                        retries: summarize(attempts.iter().filter(|a| a.retry))?,
                    })
                })
                .collect::<Result<_>>()?,
            jobs: report_jobs,
        })
    }
}
