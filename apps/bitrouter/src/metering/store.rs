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
    ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect, Set,
};
use serde::{Deserialize, Serialize};

use bitrouter_sdk::{BitrouterError, Result};

use crate::metering::db::RequestMetric;
use crate::metering::entities::requests;

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeteringUsageRecord {
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

#[derive(Debug, Clone, PartialEq)]
pub struct UsagePriceOverride {
    pub provider_id: String,
    pub model_id: String,
    pub input_micro_usd_per_token: f64,
    pub output_micro_usd_per_token: f64,
}

impl MeteringUsageRecord {
    pub fn apply_price_overrides(records: &mut [Self], prices: &[UsagePriceOverride]) {
        for record in records {
            if record.final_charge_micro_usd.unwrap_or(0) != 0 {
                continue;
            }
            let Some(price) = prices.iter().find(|price| {
                price.provider_id == record.provider_id && price.model_id == record.model_id
            }) else {
                continue;
            };
            let charge = record.prompt_tokens as f64 * price.input_micro_usd_per_token
                + record.completion_tokens as f64 * price.output_micro_usd_per_token;
            record.final_charge_micro_usd = Some(charge.round().max(0.0) as u64);
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
        let (route, prices) = value.split_once('=').ok_or_else(|| {
            BitrouterError::bad_request(format!(
                "invalid price override {value:?}; expected provider:model=input,output"
            ))
        })?;
        let (provider_id, model_id) = route.split_once(':').ok_or_else(|| {
            BitrouterError::bad_request(format!(
                "invalid price override {value:?}; expected provider:model=input,output"
            ))
        })?;
        let (input, output) = prices.split_once(',').ok_or_else(|| {
            BitrouterError::bad_request(format!(
                "invalid price override {value:?}; expected provider:model=input,output"
            ))
        })?;
        let input_micro_usd_per_token = input.trim().parse::<f64>().map_err(|e| {
            BitrouterError::bad_request(format!("invalid input price in override {value:?}: {e}"))
        })?;
        let output_micro_usd_per_token = output.trim().parse::<f64>().map_err(|e| {
            BitrouterError::bad_request(format!("invalid output price in override {value:?}: {e}"))
        })?;
        Ok(Self {
            provider_id: provider_id.trim().to_string(),
            model_id: model_id.trim().to_string(),
            input_micro_usd_per_token,
            output_micro_usd_per_token,
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

    /// Record one settled request. The single writer.
    pub async fn record_request(&self, record: RequestMetric) -> Result<()> {
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

impl From<requests::Model> for MeteringUsageRecord {
    fn from(row: requests::Model) -> Self {
        let status = if row.error.is_some() {
            "failed"
        } else {
            "completed"
        };
        Self {
            id: Some(row.request_id.clone()),
            request_id: Some(row.request_id),
            provider_id: row.provider_id,
            model_id: row.model_id,
            prompt_tokens: row.prompt_tokens.max(0) as u64,
            completion_tokens: row.completion_tokens.max(0) as u64,
            final_charge_micro_usd: Some(row.estimated_charge_micro_usd.max(0) as u64),
            status: Some(status.to_string()),
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
