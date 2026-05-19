//! `MeteringStore` — SQLite read+write for the `requests` table.
//!
//! Single writer ([`super::MeteringRecorder`]), multiple readers (the
//! `policy` module's spend-cap enforcement; future analytics CLI).

use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use sqlx::{Row, SqlitePool};

use bitrouter_sdk::{BitrouterError, Result};

use crate::metering::db::RequestMetric;

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

/// Request / token rate over the trailing minute.
#[derive(Debug, Clone, Copy, Default)]
pub struct RateMetrics {
    /// Requests observed in the trailing minute.
    pub requests_per_minute: f64,
    /// Tokens observed in the trailing minute.
    pub tokens_per_minute: f64,
}

/// SQLite-backed metering store.
#[derive(Clone)]
pub struct MeteringStore {
    pool: SqlitePool,
}

impl MeteringStore {
    /// Build a store over a sqlite pool. The pool must already carry the
    /// `requests` table (`crate::metering::db::migrate`).
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Shared access to the underlying pool (used by tests and by sibling
    /// OSS business modules that want to share the same DB).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Total estimated spend (micro-USD) for `api_key_id` within `window`.
    pub async fn get_spend(&self, api_key_id: &str, window: TimeWindow) -> Result<u64> {
        let start = window_start(window).to_rfc3339();
        let row = sqlx::query(
            "SELECT COALESCE(SUM(estimated_charge_micro_usd), 0) AS total \
             FROM requests WHERE api_key_id = ? AND created_at >= ?",
        )
        .bind(api_key_id)
        .bind(&start)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| BitrouterError::internal(format!("get_spend: {e}")))?;
        Ok(row.get::<i64, _>("total").max(0) as u64)
    }

    /// Total request count for `api_key_id` within `window`.
    pub async fn get_request_count(&self, api_key_id: &str, window: TimeWindow) -> Result<u64> {
        let start = window_start(window).to_rfc3339();
        let row = sqlx::query(
            "SELECT COUNT(*) AS n FROM requests WHERE api_key_id = ? AND created_at >= ?",
        )
        .bind(api_key_id)
        .bind(&start)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| BitrouterError::internal(format!("get_request_count: {e}")))?;
        Ok(row.get::<i64, _>("n").max(0) as u64)
    }

    /// Token usage for `api_key_id` on `model` within `window`.
    pub async fn get_token_usage(
        &self,
        api_key_id: &str,
        model: &str,
        window: TimeWindow,
    ) -> Result<TokenUsage> {
        let start = window_start(window).to_rfc3339();
        let row = sqlx::query(
            "SELECT COALESCE(SUM(prompt_tokens), 0) AS pt, \
             COALESCE(SUM(completion_tokens), 0) AS ct \
             FROM requests WHERE api_key_id = ? AND model_id = ? AND created_at >= ?",
        )
        .bind(api_key_id)
        .bind(model)
        .bind(&start)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| BitrouterError::internal(format!("get_token_usage: {e}")))?;
        let prompt = row.get::<i64, _>("pt").max(0) as u64;
        let completion = row.get::<i64, _>("ct").max(0) as u64;
        Ok(TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
        })
    }

    /// Current request/token rate for `api_key_id`.
    pub async fn get_rate(&self, api_key_id: &str) -> Result<RateMetrics> {
        let start = window_start(TimeWindow::LastMinute).to_rfc3339();
        let row = sqlx::query(
            "SELECT COUNT(*) AS n, \
             COALESCE(SUM(prompt_tokens + completion_tokens), 0) AS tok \
             FROM requests WHERE api_key_id = ? AND created_at >= ?",
        )
        .bind(api_key_id)
        .bind(&start)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| BitrouterError::internal(format!("get_rate: {e}")))?;
        Ok(RateMetrics {
            requests_per_minute: row.get::<i64, _>("n").max(0) as f64,
            tokens_per_minute: row.get::<i64, _>("tok").max(0) as f64,
        })
    }

    /// Record one settled request. The single writer.
    pub async fn record_request(&self, record: RequestMetric) -> Result<()> {
        // `ON CONFLICT(request_id) DO UPDATE` so a retried / streamed-then-
        // finalised write doesn't reset `created_at`; only mutable columns
        // refresh.
        sqlx::query(
            "INSERT INTO requests \
             (request_id, user_id, api_key_id, model_id, provider_id, \
              prompt_tokens, completion_tokens, reasoning_tokens, \
              cache_read_tokens, cache_write_tokens, \
              estimated_charge_micro_usd, streamed, \
              latency_ms, generation_time_ms, error, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(request_id) DO UPDATE SET \
                user_id = excluded.user_id, \
                api_key_id = excluded.api_key_id, \
                model_id = excluded.model_id, \
                provider_id = excluded.provider_id, \
                prompt_tokens = excluded.prompt_tokens, \
                completion_tokens = excluded.completion_tokens, \
                reasoning_tokens = excluded.reasoning_tokens, \
                cache_read_tokens = excluded.cache_read_tokens, \
                cache_write_tokens = excluded.cache_write_tokens, \
                estimated_charge_micro_usd = excluded.estimated_charge_micro_usd, \
                streamed = excluded.streamed, \
                latency_ms = excluded.latency_ms, \
                generation_time_ms = excluded.generation_time_ms, \
                error = excluded.error",
        )
        .bind(&record.request_id)
        .bind(&record.user_id)
        .bind(&record.api_key_id)
        .bind(&record.model_id)
        .bind(&record.provider_id)
        .bind(record.prompt_tokens as i64)
        .bind(record.completion_tokens as i64)
        .bind(record.reasoning_tokens as i64)
        .bind(record.cache_read_tokens as i64)
        .bind(record.cache_write_tokens as i64)
        .bind(record.estimated_charge_micro_usd)
        .bind(record.streamed as i64)
        .bind(record.latency_ms as i64)
        .bind(record.generation_time_ms as i64)
        .bind(&record.error)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await
        .map_err(|e| BitrouterError::internal(format!("record_request: {e}")))?;
        Ok(())
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
