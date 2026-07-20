//! `MeteringStore` — read+write access to the `requests` table.
//!
//! Single writer ([`super::MeteringRecorder`]), multiple readers (the
//! `policy` module's spend-cap enforcement; future analytics CLI).
//!
//! Every query goes through sea-orm, so the store works unchanged on
//! whichever backend `database.url` selects (SQLite / Postgres / MySQL).
//! The window rollups (`SUM` of charges / tokens) are folded in Rust
//! rather than as a SQL aggregate: `SUM` returns a different value type
//! per backend (`integer` / `numeric` / `decimal`), and folding the rows
//! sidesteps that entirely. The metering windows are bounded (at most one
//! month) so the row counts stay modest for a local-first deployment.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;

use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect, Set,
};
use serde::{Deserialize, Serialize};

use bitrouter_cloud_sdk::settlement::{SettlementReceipt, SettlementState};
use bitrouter_sdk::language_model::{Usage, UsageOrigin};
use bitrouter_sdk::{BitrouterError, Result};

use crate::metering::db::{ReconciliationStatus, RequestMetric};
use crate::metering::entities::requests;
use crate::metering::pricing::{
    ChargeEvidence, ChargeStatus, ModelPricing, PricingSource, calculate_charge_evidence,
    unavailable_charge_evidence,
};

/// A rolling time window for usage queries.
#[derive(Debug, Clone, Copy)]
pub enum TimeWindow {
    /// Trailing 60 seconds.
    LastMinute,
    /// Trailing 60 minutes.
    LastHour,
    /// Since UTC 00:00 today.
    Today,
    /// Since UTC 00:00 Monday this week.
    ThisWeek,
    /// Since UTC 00:00 on the 1st this month.
    ThisMonth,
    /// An explicit `[start, end)` range.
    Custom {
        /// Inclusive start.
        start: DateTime<Utc>,
        /// Exclusive end.
        end: DateTime<Utc>,
    },
}

/// Token usage rollup over a window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// Prompt (input) tokens.
    pub prompt_tokens: u64,
    /// Completion (output) tokens.
    pub completion_tokens: u64,
    /// Sum of prompt + completion.
    pub total_tokens: u64,
}

/// A settled request exported in the workflow bundle's usage-record shape.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MeteringUsageRecord {
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsagePriceOverride {
    pub provider_id: String,
    pub model_id: String,
    pub input_micro_usd_per_token: f64,
    pub cache_read_micro_usd_per_token: Option<f64>,
    pub cache_write_micro_usd_per_token: Option<f64>,
    pub output_micro_usd_per_token: f64,
}

/// Minimal state needed by the bounded reconciliation loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconciliationRecord {
    /// Stable request identity.
    pub request_id: String,
    /// Current reconciliation state.
    pub status: ReconciliationStatus,
    /// Receipt fetches already attempted.
    pub attempts: u32,
}

impl MeteringUsageRecord {
    pub fn apply_price_overrides(records: &mut [Self], prices: &[UsagePriceOverride]) {
        for record in records {
            if record.charge_status == ChargeStatus::Computed
                || record.reconciliation_status != ReconciliationStatus::NotApplicable
                || record.usage_origin == UsageOrigin::Unknown
            {
                continue;
            }
            let Some(price) = prices.iter().find(|price| {
                price.provider_id == record.provider_id && price.model_id == record.model_id
            }) else {
                continue;
            };
            if (record.cache_read_tokens > 0 || record.cache_write_tokens > 0)
                && (price.cache_read_micro_usd_per_token.is_none()
                    || price.cache_write_micro_usd_per_token.is_none())
            {
                continue;
            }
            let usage = Usage {
                prompt_tokens: record.prompt_tokens,
                completion_tokens: record.completion_tokens,
                reasoning_tokens: record.reasoning_tokens,
                cache_read_tokens: record.cache_read_tokens,
                cache_write_tokens: record.cache_write_tokens,
                origin: record.usage_origin,
                raw: record.raw_usage.clone().map(Box::new),
                ..Default::default()
            };
            let pricing = ModelPricing::cache_aware(
                Some(price.input_micro_usd_per_token),
                price.cache_read_micro_usd_per_token,
                price.cache_write_micro_usd_per_token,
                Some(price.output_micro_usd_per_token),
            );
            let evidence = calculate_charge_evidence(&usage, &pricing, PricingSource::Override);
            if evidence.status != ChargeStatus::Computed {
                record.charge_status = evidence.status;
                record.charge_evidence = Some(evidence);
                record.final_charge_micro_usd = None;
                continue;
            }
            record.uncached_input_tokens = evidence.normalized_usage.uncached_input_tokens;
            record.output_tokens = evidence.normalized_usage.output_tokens;
            record.final_charge_micro_usd =
                evidence.charge_micro_usd.map(|charge| charge.max(0) as u64);
            record.charge_status = evidence.status;
            record.charge_evidence = Some(evidence);
        }
    }

    pub fn write_jsonl(path: impl AsRef<Path>, records: &[Self]) -> Result<()> {
        if let Some(parent) = path.as_ref().parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|e| {
                BitrouterError::internal(format!(
                    "metering usage jsonl mkdir {}: {e}",
                    parent.display()
                ))
            })?;
        }
        let file = File::create(path.as_ref()).map_err(|e| {
            BitrouterError::internal(format!(
                "metering usage jsonl create {}: {e}",
                path.as_ref().display()
            ))
        })?;
        let mut writer = BufWriter::new(file);
        for record in records {
            serde_json::to_writer(&mut writer, record).map_err(|e| {
                BitrouterError::internal(format!("metering usage jsonl serialize: {e}"))
            })?;
            writer.write_all(b"\n").map_err(|e| {
                BitrouterError::internal(format!("metering usage jsonl write: {e}"))
            })?;
        }
        writer
            .flush()
            .map_err(|e| BitrouterError::internal(format!("metering usage jsonl flush: {e}")))
    }
}

impl UsagePriceOverride {
    pub fn parse(value: &str) -> Result<Self> {
        const EXPECTED: &str =
            "provider:model=uncached,cache_read,cache_write,output (or legacy input,output)";
        let (route, prices) = value.split_once('=').ok_or_else(|| {
            BitrouterError::bad_request(format!(
                "invalid price override {value:?}; expected {EXPECTED}"
            ))
        })?;
        let (provider_id, model_id) = route.split_once(':').ok_or_else(|| {
            BitrouterError::bad_request(format!(
                "invalid price override {value:?}; expected {EXPECTED}"
            ))
        })?;
        let rates = prices
            .split(',')
            .map(str::trim)
            .map(|rate| {
                rate.parse::<f64>().map_err(|error| {
                    BitrouterError::bad_request(format!(
                        "invalid price in override {value:?}: {error}"
                    ))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if rates.iter().any(|rate| !rate.is_finite() || *rate < 0.0) {
            return Err(BitrouterError::bad_request(format!(
                "invalid price override {value:?}; rates must be finite and non-negative"
            )));
        }
        let (input, cache_read, cache_write, output) = match rates.as_slice() {
            [input, output] => (*input, None, None, *output),
            [input, cache_read, cache_write, output] => {
                (*input, Some(*cache_read), Some(*cache_write), *output)
            }
            _ => {
                return Err(BitrouterError::bad_request(format!(
                    "invalid price override {value:?}; expected {EXPECTED}"
                )));
            }
        };
        Ok(Self {
            provider_id: provider_id.trim().to_string(),
            model_id: model_id.trim().to_string(),
            input_micro_usd_per_token: input,
            cache_read_micro_usd_per_token: cache_read,
            cache_write_micro_usd_per_token: cache_write,
            output_micro_usd_per_token: output,
        })
    }
}

/// Request / token rate over the trailing minute.
#[derive(Debug, Clone, Copy, Default)]
pub struct RateMetrics {
    /// Requests observed in the trailing minute.
    pub requests_per_minute: f64,
    /// Tokens observed in the trailing minute.
    pub tokens_per_minute: f64,
}

/// Spend / request rollup over a window, across **every** caller.
///
/// Read by the agent-facing CLI surfaces (`status --agent`,
/// the MCP cost footer, the `spawn` exit summary) — unlike the
/// per-key getters above, nothing here is
/// scoped to one `api_key_id`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpendSummary {
    /// Total estimated spend in micro-USD.
    pub spend_micro_usd: u64,
    /// Requests observed (success and failure alike).
    pub requests: u64,
}

/// sea-orm-backed metering store.
#[derive(Clone)]
pub struct MeteringStore {
    db: DatabaseConnection,
}

impl MeteringStore {
    /// Build a store over a database connection. The database must already
    /// carry the `requests` table (`crate::db::run_migrations`).
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Total estimated spend (micro-USD) for `api_key_id` within `window`.
    pub async fn get_spend(&self, api_key_id: &str, window: TimeWindow) -> Result<u64> {
        let start = window_start(window).to_rfc3339();
        let charges: Vec<i64> = requests::Entity::find()
            .select_only()
            .column(requests::Column::EstimatedChargeMicroUsd)
            .filter(requests::Column::ApiKeyId.eq(api_key_id))
            .filter(requests::Column::CreatedAt.gte(start))
            .into_tuple()
            .all(&self.db)
            .await
            .map_err(|e| BitrouterError::internal(format!("get_spend: {e}")))?;
        Ok(charges.into_iter().map(|c| c.max(0) as u64).sum())
    }

    /// Total request count for `api_key_id` within `window`.
    pub async fn get_request_count(&self, api_key_id: &str, window: TimeWindow) -> Result<u64> {
        let start = window_start(window).to_rfc3339();
        requests::Entity::find()
            .filter(requests::Column::ApiKeyId.eq(api_key_id))
            .filter(requests::Column::CreatedAt.gte(start))
            .count(&self.db)
            .await
            .map_err(|e| BitrouterError::internal(format!("get_request_count: {e}")))
    }

    /// Token usage for `api_key_id` on `model` within `window`.
    pub async fn get_token_usage(
        &self,
        api_key_id: &str,
        model: &str,
        window: TimeWindow,
    ) -> Result<TokenUsage> {
        let start = window_start(window).to_rfc3339();
        let rows: Vec<(i64, i64)> = requests::Entity::find()
            .select_only()
            .column(requests::Column::PromptTokens)
            .column(requests::Column::CompletionTokens)
            .filter(requests::Column::ApiKeyId.eq(api_key_id))
            .filter(requests::Column::ModelId.eq(model))
            .filter(requests::Column::CreatedAt.gte(start))
            .into_tuple()
            .all(&self.db)
            .await
            .map_err(|e| BitrouterError::internal(format!("get_token_usage: {e}")))?;
        let prompt: u64 = rows.iter().map(|(p, _)| (*p).max(0) as u64).sum();
        let completion: u64 = rows.iter().map(|(_, c)| (*c).max(0) as u64).sum();
        Ok(TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
        })
    }

    /// Current request/token rate for `api_key_id`.
    pub async fn get_rate(&self, api_key_id: &str) -> Result<RateMetrics> {
        let start = window_start(TimeWindow::LastMinute).to_rfc3339();
        let rows: Vec<(i64, i64)> = requests::Entity::find()
            .select_only()
            .column(requests::Column::PromptTokens)
            .column(requests::Column::CompletionTokens)
            .filter(requests::Column::ApiKeyId.eq(api_key_id))
            .filter(requests::Column::CreatedAt.gte(start))
            .into_tuple()
            .all(&self.db)
            .await
            .map_err(|e| BitrouterError::internal(format!("get_rate: {e}")))?;
        let tokens: u64 = rows
            .iter()
            .map(|(p, c)| (*p).max(0) as u64 + (*c).max(0) as u64)
            .sum();
        Ok(RateMetrics {
            requests_per_minute: rows.len() as f64,
            tokens_per_minute: tokens as f64,
        })
    }

    /// Export settled request rows in a JSONL-friendly shape compatible with
    /// `workflow-state bundle`'s cloud usage input.
    pub async fn export_usage(&self, window: TimeWindow) -> Result<Vec<MeteringUsageRecord>> {
        let start = window_start(window).to_rfc3339();
        let mut query = requests::Entity::find()
            .filter(requests::Column::CreatedAt.gte(start))
            .order_by_asc(requests::Column::CreatedAt)
            .order_by_asc(requests::Column::RequestId);
        if let TimeWindow::Custom { end, .. } = window {
            query = query.filter(requests::Column::CreatedAt.lt(end.to_rfc3339()));
        }
        let rows = query
            .all(&self.db)
            .await
            .map_err(|e| BitrouterError::internal(format!("export_usage: {e}")))?;

        Ok(rows.into_iter().map(MeteringUsageRecord::from).collect())
    }

    /// Total spend + request count within `window`, across every caller.
    pub async fn spend_summary(&self, window: TimeWindow) -> Result<SpendSummary> {
        let start = window_start(window).to_rfc3339();
        let charges: Vec<i64> = requests::Entity::find()
            .select_only()
            .column(requests::Column::EstimatedChargeMicroUsd)
            .filter(requests::Column::CreatedAt.gte(start))
            .into_tuple()
            .all(&self.db)
            .await
            .map_err(|e| BitrouterError::internal(format!("spend_summary: {e}")))?;
        Ok(SpendSummary {
            requests: charges.len() as u64,
            spend_micro_usd: charges.into_iter().map(|c| c.max(0) as u64).sum(),
        })
    }

    /// Load reconciliation state for exactly the supplied request ids.
    pub async fn reconciliation_records(
        &self,
        request_ids: &[String],
    ) -> Result<Vec<ReconciliationRecord>> {
        if request_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = requests::Entity::find()
            .filter(requests::Column::RequestId.is_in(request_ids.iter().cloned()))
            .order_by_asc(requests::Column::RequestId)
            .all(&self.db)
            .await
            .map_err(|error| {
                BitrouterError::internal(format!("load reconciliation records: {error}"))
            })?;
        Ok(rows
            .into_iter()
            .map(|row| ReconciliationRecord {
                request_id: row.request_id,
                status: ReconciliationStatus::from_persisted(&row.reconciliation_status),
                attempts: row.reconciliation_attempts.max(0) as u32,
            })
            .collect())
    }

    /// Increment the durable attempt counter before issuing a receipt request.
    pub async fn start_reconciliation_attempt(&self, request_id: &str) -> Result<u32> {
        let row = requests::Entity::find_by_id(request_id)
            .one(&self.db)
            .await
            .map_err(|error| {
                BitrouterError::internal(format!("load reconciliation attempt: {error}"))
            })?
            .ok_or_else(|| {
                BitrouterError::bad_request(format!(
                    "reconciliation request not found: {request_id}"
                ))
            })?;
        if ReconciliationStatus::from_persisted(&row.reconciliation_status)
            != ReconciliationStatus::Pending
        {
            return Err(BitrouterError::bad_request(format!(
                "reconciliation request {request_id} is not pending"
            )));
        }
        let attempts = row.reconciliation_attempts.max(0).saturating_add(1);
        let mut active: requests::ActiveModel = row.into();
        active.reconciliation_attempts = Set(attempts);
        active.reconciliation_last_attempt_at = Set(Some(Utc::now().to_rfc3339()));
        active.reconciliation_last_error = Set(None);
        active.update(&self.db).await.map_err(|error| {
            BitrouterError::internal(format!("persist reconciliation attempt: {error}"))
        })?;
        Ok(attempts as u32)
    }

    /// Reopen a previously unknown row for a later bounded reconciliation
    /// invocation while preserving its durable attempt budget.
    pub async fn reopen_unknown_reconciliation(
        &self,
        request_id: &str,
        max_attempts: u32,
    ) -> Result<bool> {
        let row = requests::Entity::find_by_id(request_id)
            .one(&self.db)
            .await
            .map_err(|error| {
                BitrouterError::internal(format!("load unknown reconciliation row: {error}"))
            })?
            .ok_or_else(|| {
                BitrouterError::bad_request(format!(
                    "reconciliation request not found: {request_id}"
                ))
            })?;
        if ReconciliationStatus::from_persisted(&row.reconciliation_status)
            != ReconciliationStatus::Unknown
            || row.reconciliation_attempts.max(0) as u32 >= max_attempts
        {
            return Ok(false);
        }
        let mut active: requests::ActiveModel = row.into();
        active.reconciliation_status = Set(ReconciliationStatus::Pending.as_str().to_string());
        active.authoritative_settled_at = Set(None);
        active.update(&self.db).await.map_err(|error| {
            BitrouterError::internal(format!("reopen unknown reconciliation: {error}"))
        })?;
        Ok(true)
    }

    /// Store a content-free receipt-fetch failure for operator diagnosis.
    pub async fn record_reconciliation_error(&self, request_id: &str, error: &str) -> Result<()> {
        let row = requests::Entity::find_by_id(request_id)
            .one(&self.db)
            .await
            .map_err(|load_error| {
                BitrouterError::internal(format!("load reconciliation error row: {load_error}"))
            })?
            .ok_or_else(|| {
                BitrouterError::bad_request(format!(
                    "reconciliation request not found: {request_id}"
                ))
            })?;
        let mut active: requests::ActiveModel = row.into();
        active.reconciliation_last_error = Set(Some(truncate_error(error)));
        active.update(&self.db).await.map_err(|update_error| {
            BitrouterError::internal(format!("persist reconciliation error: {update_error}"))
        })?;
        Ok(())
    }

    /// Terminally fail a pending row after the bounded retry budget expires.
    pub async fn exhaust_reconciliation(&self, request_id: &str, reason: &str) -> Result<()> {
        let row = requests::Entity::find_by_id(request_id)
            .one(&self.db)
            .await
            .map_err(|error| {
                BitrouterError::internal(format!("load exhausted reconciliation: {error}"))
            })?
            .ok_or_else(|| {
                BitrouterError::bad_request(format!(
                    "reconciliation request not found: {request_id}"
                ))
            })?;
        if ReconciliationStatus::from_persisted(&row.reconciliation_status)
            != ReconciliationStatus::Pending
        {
            return Ok(());
        }
        let mut active: requests::ActiveModel = row.into();
        active.reconciliation_status = Set(ReconciliationStatus::Unknown.as_str().to_string());
        active.charge_status = Set(ChargeStatus::Unknown.as_str().to_string());
        active.estimated_charge_micro_usd = Set(0);
        active.reconciliation_last_error = Set(Some(truncate_error(reason)));
        active.authoritative_settled_at = Set(Some(Utc::now().to_rfc3339()));
        active.update(&self.db).await.map_err(|error| {
            BitrouterError::internal(format!("persist exhausted reconciliation: {error}"))
        })?;
        Ok(())
    }

    /// Apply one content-free authoritative receipt to a pending local row.
    /// Computed receipts are accepted only when frozen pricing reconstructs
    /// the exact final micro-USD charge.
    pub async fn apply_authoritative_receipt(
        &self,
        receipt: &SettlementReceipt,
        prices: &[UsagePriceOverride],
    ) -> Result<ReconciliationStatus> {
        let row = requests::Entity::find_by_id(&receipt.request_id)
            .one(&self.db)
            .await
            .map_err(|error| BitrouterError::internal(format!("load reconciliation row: {error}")))?
            .ok_or_else(|| {
                BitrouterError::bad_request(format!(
                    "reconciliation request not found: {}",
                    receipt.request_id
                ))
            })?;
        let current = ReconciliationStatus::from_persisted(&row.reconciliation_status);
        if current != ReconciliationStatus::Pending {
            return Err(BitrouterError::bad_request(format!(
                "reconciliation request {} is already terminal ({})",
                receipt.request_id,
                current.as_str()
            )));
        }
        let receipt_json = serde_json::to_value(receipt).map_err(|error| {
            BitrouterError::internal(format!("serialize authoritative receipt: {error}"))
        })?;
        let usage = authoritative_usage(receipt, receipt_json.clone())?;
        let (status, charge_status, evidence, provider_id, model_id) = match receipt.state {
            SettlementState::Pending => return Ok(ReconciliationStatus::Pending),
            SettlementState::NotCharged => {
                if usage.prompt_tokens != 0 || usage.completion_tokens != 0 {
                    return self
                        .persist_unknown_reconciliation(
                            row,
                            &usage,
                            receipt_json,
                            "not_charged_receipt_has_usage",
                        )
                        .await;
                }
                let mut evidence = unavailable_charge_evidence(&usage, "authoritative_not_charged");
                evidence.status = ChargeStatus::NotCharged;
                (
                    ReconciliationStatus::NotCharged,
                    ChargeStatus::NotCharged,
                    evidence,
                    receipt
                        .provider_id
                        .clone()
                        .unwrap_or(row.provider_id.clone()),
                    receipt.model_id.clone().unwrap_or(row.model_id.clone()),
                )
            }
            SettlementState::Unknown => {
                return self
                    .persist_unknown_reconciliation(
                        row,
                        &usage,
                        receipt_json,
                        "authoritative_settlement_unknown",
                    )
                    .await;
            }
            SettlementState::Computed => {
                let Some(provider_id) = receipt.provider_id.as_deref() else {
                    return self
                        .persist_unknown_reconciliation(
                            row,
                            &usage,
                            receipt_json,
                            "authoritative_provider_missing",
                        )
                        .await;
                };
                let Some(model_id) = receipt.model_id.as_deref() else {
                    return self
                        .persist_unknown_reconciliation(
                            row,
                            &usage,
                            receipt_json,
                            "authoritative_model_missing",
                        )
                        .await;
                };
                let Some(price) = prices
                    .iter()
                    .find(|price| price.provider_id == provider_id && price.model_id == model_id)
                else {
                    return self
                        .persist_unknown_reconciliation(
                            row,
                            &usage,
                            receipt_json,
                            "authoritative_pricing_not_found",
                        )
                        .await;
                };
                let pricing = ModelPricing::cache_aware(
                    Some(price.input_micro_usd_per_token),
                    price.cache_read_micro_usd_per_token,
                    price.cache_write_micro_usd_per_token,
                    Some(price.output_micro_usd_per_token),
                );
                let evidence = calculate_charge_evidence(&usage, &pricing, PricingSource::Override);
                if evidence.status != ChargeStatus::Computed
                    || evidence.charge_micro_usd != receipt.final_charge_micro_usd
                {
                    return self
                        .persist_unknown_reconciliation(
                            row,
                            &usage,
                            receipt_json,
                            "authoritative_charge_mismatch",
                        )
                        .await;
                }
                (
                    ReconciliationStatus::Computed,
                    ChargeStatus::Computed,
                    evidence,
                    provider_id.to_string(),
                    model_id.to_string(),
                )
            }
        };
        let raw_json = serde_json::to_string(&receipt_json).map_err(|error| {
            BitrouterError::internal(format!("serialize authoritative usage: {error}"))
        })?;
        let evidence_json = serde_json::to_string(&evidence).map_err(|error| {
            BitrouterError::internal(format!("serialize reconciled charge evidence: {error}"))
        })?;
        let settled_at = Utc::now().to_rfc3339();
        let mut active: requests::ActiveModel = row.into();
        active.provider_id = Set(provider_id);
        active.model_id = Set(model_id);
        active.prompt_tokens = Set(usage.prompt_tokens as i64);
        active.completion_tokens = Set(usage.completion_tokens as i64);
        active.reasoning_tokens = Set(usage.reasoning_tokens as i64);
        active.cache_read_tokens = Set(usage.cache_read_tokens as i64);
        active.cache_write_tokens = Set(usage.cache_write_tokens as i64);
        let normalized = &evidence.normalized_usage;
        active.uncached_input_tokens = Set(normalized.uncached_input_tokens as i64);
        active.output_tokens = Set(normalized.output_tokens as i64);
        active.usage_origin = Set(UsageOrigin::AuthoritativeReceipt.as_str().to_string());
        active.raw_usage_json = Set(Some(raw_json.clone()));
        active.charge_status = Set(charge_status.as_str().to_string());
        active.charge_evidence_json = Set(Some(evidence_json));
        active.estimated_charge_micro_usd = Set(evidence.charge_micro_usd.unwrap_or(0));
        active.reconciliation_status = Set(status.as_str().to_string());
        active.reconciliation_last_error = Set(None);
        active.authoritative_settled_at = Set(Some(settled_at));
        active.authoritative_receipt_json = Set(Some(raw_json));
        active.update(&self.db).await.map_err(|error| {
            BitrouterError::internal(format!("persist authoritative receipt: {error}"))
        })?;
        Ok(status)
    }

    async fn persist_unknown_reconciliation(
        &self,
        row: requests::Model,
        usage: &Usage,
        receipt_json: serde_json::Value,
        reason: &str,
    ) -> Result<ReconciliationStatus> {
        let evidence = unavailable_charge_evidence(usage, reason);
        let evidence_json = serde_json::to_string(&evidence).map_err(|error| {
            BitrouterError::internal(format!("serialize unknown reconciliation: {error}"))
        })?;
        let receipt_json = serde_json::to_string(&receipt_json).map_err(|error| {
            BitrouterError::internal(format!("serialize authoritative receipt: {error}"))
        })?;
        let mut active: requests::ActiveModel = row.into();
        active.prompt_tokens = Set(usage.prompt_tokens as i64);
        active.completion_tokens = Set(usage.completion_tokens as i64);
        active.reasoning_tokens = Set(usage.reasoning_tokens as i64);
        active.cache_read_tokens = Set(usage.cache_read_tokens as i64);
        active.cache_write_tokens = Set(usage.cache_write_tokens as i64);
        active.uncached_input_tokens = Set(evidence.normalized_usage.uncached_input_tokens as i64);
        active.output_tokens = Set(evidence.normalized_usage.output_tokens as i64);
        active.usage_origin = Set(UsageOrigin::AuthoritativeReceipt.as_str().to_string());
        active.raw_usage_json = Set(Some(receipt_json.clone()));
        active.charge_status = Set(ChargeStatus::Unknown.as_str().to_string());
        active.charge_evidence_json = Set(Some(evidence_json));
        active.estimated_charge_micro_usd = Set(0);
        active.reconciliation_status = Set(ReconciliationStatus::Unknown.as_str().to_string());
        active.reconciliation_last_error = Set(Some(reason.to_string()));
        active.authoritative_settled_at = Set(Some(Utc::now().to_rfc3339()));
        active.authoritative_receipt_json = Set(Some(receipt_json));
        active.update(&self.db).await.map_err(|error| {
            BitrouterError::internal(format!("persist unknown reconciliation: {error}"))
        })?;
        Ok(ReconciliationStatus::Unknown)
    }

    /// Record one settled request. The single writer.
    pub async fn record_request(&self, record: RequestMetric) -> Result<()> {
        let raw_usage_json = record
            .raw_usage
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| BitrouterError::internal(format!("serialize raw usage: {error}")))?;
        let charge_evidence_json = serde_json::to_string(&record.charge_evidence)
            .map(Some)
            .map_err(|error| {
                BitrouterError::internal(format!("serialize charge evidence: {error}"))
            })?;
        let row = requests::ActiveModel {
            request_id: Set(record.request_id),
            user_id: Set(record.user_id),
            api_key_id: Set(record.api_key_id),
            model_id: Set(record.model_id),
            provider_id: Set(record.provider_id),
            prompt_tokens: Set(record.prompt_tokens as i64),
            completion_tokens: Set(record.completion_tokens as i64),
            reasoning_tokens: Set(record.reasoning_tokens as i64),
            cache_read_tokens: Set(record.cache_read_tokens as i64),
            cache_write_tokens: Set(record.cache_write_tokens as i64),
            uncached_input_tokens: Set(record.uncached_input_tokens as i64),
            output_tokens: Set(record.output_tokens as i64),
            usage_origin: Set(record.usage_origin.as_str().to_string()),
            raw_usage_json: Set(raw_usage_json),
            charge_status: Set(record.charge_status.as_str().to_string()),
            charge_evidence_json: Set(charge_evidence_json),
            reconciliation_status: Set(record.reconciliation_status.as_str().to_string()),
            reconciliation_attempts: Set(0),
            reconciliation_last_error: Set(None),
            reconciliation_last_attempt_at: Set(None),
            authoritative_settled_at: Set(None),
            authoritative_receipt_json: Set(None),
            estimated_charge_micro_usd: Set(record.estimated_charge_micro_usd),
            streamed: Set(record.streamed as i32),
            latency_ms: Set(record.latency_ms as i64),
            generation_time_ms: Set(record.generation_time_ms as i64),
            error: Set(record.error),
            created_at: Set(Utc::now().to_rfc3339()),
        };
        // `ON CONFLICT(request_id) DO UPDATE` so a retried / streamed-then-
        // finalised write doesn't reset `created_at`; only mutable columns
        // refresh.
        requests::Entity::insert(row)
            .on_conflict(
                OnConflict::column(requests::Column::RequestId)
                    .update_columns([
                        requests::Column::UserId,
                        requests::Column::ApiKeyId,
                        requests::Column::ModelId,
                        requests::Column::ProviderId,
                        requests::Column::PromptTokens,
                        requests::Column::CompletionTokens,
                        requests::Column::ReasoningTokens,
                        requests::Column::CacheReadTokens,
                        requests::Column::CacheWriteTokens,
                        requests::Column::UncachedInputTokens,
                        requests::Column::OutputTokens,
                        requests::Column::UsageOrigin,
                        requests::Column::RawUsageJson,
                        requests::Column::ChargeStatus,
                        requests::Column::ChargeEvidenceJson,
                        requests::Column::EstimatedChargeMicroUsd,
                        requests::Column::Streamed,
                        requests::Column::LatencyMs,
                        requests::Column::GenerationTimeMs,
                        requests::Column::Error,
                    ])
                    .to_owned(),
            )
            .exec(&self.db)
            .await
            .map_err(|e| BitrouterError::internal(format!("record_request: {e}")))?;
        Ok(())
    }
}

fn authoritative_usage(receipt: &SettlementReceipt, raw: serde_json::Value) -> Result<Usage> {
    fn tokens(value: i64, bucket: &str) -> Result<u64> {
        u64::try_from(value).map_err(|_| {
            BitrouterError::bad_request(format!("authoritative receipt has negative {bucket}"))
        })
    }
    let uncached = tokens(receipt.usage.uncached_input_tokens, "uncached_input_tokens")?;
    let cache_read = tokens(receipt.usage.cache_read_tokens, "cache_read_tokens")?;
    let cache_write = tokens(receipt.usage.cache_write_tokens, "cache_write_tokens")?;
    let output = tokens(receipt.usage.output_tokens, "output_tokens")?;
    let reasoning = tokens(receipt.usage.reasoning_tokens, "reasoning_tokens")?;
    let prompt_tokens = uncached
        .checked_add(cache_read)
        .and_then(|sum| sum.checked_add(cache_write))
        .ok_or_else(|| BitrouterError::bad_request("authoritative input token overflow"))?;
    let completion_tokens = output
        .checked_add(reasoning)
        .ok_or_else(|| BitrouterError::bad_request("authoritative output token overflow"))?;
    if prompt_tokens > i64::MAX as u64 || completion_tokens > i64::MAX as u64 {
        return Err(BitrouterError::bad_request(
            "authoritative token total exceeds local storage range",
        ));
    }
    Ok(Usage {
        prompt_tokens,
        completion_tokens,
        reasoning_tokens: reasoning,
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
        origin: UsageOrigin::AuthoritativeReceipt,
        raw: Some(Box::new(raw)),
        ..Default::default()
    })
}

fn truncate_error(error: &str) -> String {
    const MAX_CHARS: usize = 512;
    error.chars().take(MAX_CHARS).collect()
}

impl From<requests::Model> for MeteringUsageRecord {
    fn from(row: requests::Model) -> Self {
        let status = if row.error.is_some() {
            "failed"
        } else {
            "completed"
        };
        let charge_status = ChargeStatus::from_persisted(&row.charge_status);
        let charge_evidence = row
            .charge_evidence_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok());
        let final_charge_micro_usd = if charge_status == ChargeStatus::Computed {
            charge_evidence
                .as_ref()
                .and_then(|evidence: &ChargeEvidence| evidence.charge_micro_usd)
                .map(|charge| charge.max(0) as u64)
        } else {
            None
        };
        let mut usage_origin = match row.usage_origin.as_str() {
            "provider_reported" => UsageOrigin::ProviderReported,
            "authoritative_receipt" => UsageOrigin::AuthoritativeReceipt,
            "estimated" => UsageOrigin::Estimated,
            _ => UsageOrigin::Unknown,
        };
        let mut raw_usage = row
            .raw_usage_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok());
        let error_code = row.error.as_deref().map(|error| {
            if error == "upstream_policy_violation"
                || error.contains("upstream policy violation")
                || error.contains("upstream content policy violation")
            {
                "upstream_policy_violation".to_string()
            } else {
                "request_failed".to_string()
            }
        });
        let legacy_policy_rejection = error_code.as_deref() == Some("upstream_policy_violation")
            && usage_origin == UsageOrigin::Unknown
            && raw_usage.is_none()
            && row.prompt_tokens == 0
            && row.completion_tokens == 0
            && row.reasoning_tokens == 0
            && row.cache_read_tokens == 0
            && row.cache_write_tokens == 0;
        if legacy_policy_rejection {
            usage_origin = UsageOrigin::ProviderReported;
            raw_usage = Some(serde_json::json!({
                "error": { "code": "upstream_policy_violation" },
                "usage": null
            }));
        }
        let authoritative_receipt = row
            .authoritative_receipt_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok());
        Self {
            id: Some(row.request_id.clone()),
            request_id: Some(row.request_id),
            provider_id: row.provider_id,
            model_id: row.model_id,
            prompt_tokens: row.prompt_tokens.max(0) as u64,
            completion_tokens: row.completion_tokens.max(0) as u64,
            reasoning_tokens: row.reasoning_tokens.max(0) as u64,
            uncached_input_tokens: row.uncached_input_tokens.max(0) as u64,
            cache_read_tokens: row.cache_read_tokens.max(0) as u64,
            cache_write_tokens: row.cache_write_tokens.max(0) as u64,
            output_tokens: row.output_tokens.max(0) as u64,
            usage_origin,
            raw_usage,
            final_charge_micro_usd,
            charge_status,
            charge_evidence,
            reconciliation_status: ReconciliationStatus::from_persisted(&row.reconciliation_status),
            reconciliation_attempts: row.reconciliation_attempts.max(0) as u32,
            authoritative_receipt,
            status: Some(status.to_string()),
            error_code,
        }
    }
}

/// Resolve a [`TimeWindow`] into an inclusive lower-bound timestamp.
fn window_start(window: TimeWindow) -> DateTime<Utc> {
    let now = Utc::now();
    match window {
        TimeWindow::LastMinute => now - Duration::minutes(1),
        TimeWindow::LastHour => now - Duration::hours(1),
        TimeWindow::Today => Utc
            .with_ymd_and_hms(now.year(), now.month(), now.day(), 0, 0, 0)
            .single()
            .unwrap_or(now),
        TimeWindow::ThisWeek => {
            let days_from_monday = now.weekday().num_days_from_monday() as i64;
            let midnight = Utc
                .with_ymd_and_hms(now.year(), now.month(), now.day(), 0, 0, 0)
                .single()
                .unwrap_or(now);
            midnight - Duration::days(days_from_monday)
        }
        TimeWindow::ThisMonth => Utc
            .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
            .single()
            .unwrap_or(now),
        TimeWindow::Custom { start, .. } => start,
    }
}
