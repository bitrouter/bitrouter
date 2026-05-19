//! Metering schema + the `RequestMetric` row.
//!
//! v1 schema: a single `requests` table that records pipeline-observed
//! usage + the estimated micro-USD computed from the pricing table. No
//! charge state, no funding source — those are billing concepts, and
//! this OSS module measures, not bills.

use bitrouter_sdk::{BitrouterError, Result};
use sqlx::{Row, SqlitePool};

/// The SQL that creates this module's tables. Run once at startup.
pub const MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS requests (
    request_id                      TEXT PRIMARY KEY,
    user_id                         TEXT NOT NULL,
    api_key_id                      TEXT NOT NULL,
    model_id                        TEXT NOT NULL,
    provider_id                     TEXT NOT NULL,
    prompt_tokens                   INTEGER NOT NULL,
    completion_tokens               INTEGER NOT NULL,
    reasoning_tokens                INTEGER NOT NULL,
    cache_read_tokens               INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens              INTEGER NOT NULL DEFAULT 0,
    estimated_charge_micro_usd      INTEGER NOT NULL DEFAULT 0,
    streamed                        INTEGER NOT NULL DEFAULT 0,
    latency_ms                      INTEGER NOT NULL DEFAULT 0,
    generation_time_ms              INTEGER NOT NULL DEFAULT 0,
    error                           TEXT,
    created_at                      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_requests_api_key_created
    ON requests(api_key_id, created_at);
CREATE INDEX IF NOT EXISTS idx_requests_user_created
    ON requests(user_id, created_at);
"#;

/// Create the metering tables on `pool`. Idempotent.
///
/// Also handles the schema rename from the pre-OSS-refactor column name
/// `final_charge_micro_usd` to the current `estimated_charge_micro_usd`.
/// A pre-refactor `.db` file would otherwise have the old column and
/// every recorder insert would fail with `no such column:
/// estimated_charge_micro_usd`. The rename is detected by inspecting
/// `pragma_table_info(requests)` and applied with `ALTER TABLE … RENAME
/// COLUMN`, which SQLite supports natively since 3.25.0.
pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    for stmt in MIGRATION_SQL.split(';') {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        sqlx::query(stmt)
            .execute(pool)
            .await
            .map_err(|e| BitrouterError::internal(format!("metering migration: {e}")))?;
    }
    rename_legacy_charge_column(pool).await?;
    Ok(())
}

/// Pre-OSS-refactor schema named the column `final_charge_micro_usd`. The
/// OSS metering module renamed it to `estimated_charge_micro_usd` for
/// semantic clarity (we measure; cloud bills). This helper looks for the
/// legacy column on existing databases and renames it in place.
async fn rename_legacy_charge_column(pool: &SqlitePool) -> Result<()> {
    let columns: Vec<String> = sqlx::query("PRAGMA table_info(requests)")
        .fetch_all(pool)
        .await
        .map_err(|e| BitrouterError::internal(format!("metering migration probe: {e}")))?
        .into_iter()
        .map(|row| row.get::<String, _>("name"))
        .collect();
    let has_legacy = columns.iter().any(|c| c == "final_charge_micro_usd");
    let has_current = columns.iter().any(|c| c == "estimated_charge_micro_usd");
    if has_legacy && !has_current {
        sqlx::query(
            "ALTER TABLE requests RENAME COLUMN final_charge_micro_usd \
             TO estimated_charge_micro_usd",
        )
        .execute(pool)
        .await
        .map_err(|e| BitrouterError::internal(format!("metering migration rename: {e}")))?;
        tracing::info!(
            "metering: renamed legacy `requests.final_charge_micro_usd` → \
             `estimated_charge_micro_usd`"
        );
    }
    Ok(())
}

/// One settled request, as recorded by [`super::MeteringRecorder`].
#[derive(Debug, Clone)]
pub struct RequestMetric {
    /// Unique request id.
    pub request_id: String,
    /// Owning user id.
    pub user_id: String,
    /// API key id that made the request.
    pub api_key_id: String,
    /// Resolved model id.
    pub model_id: String,
    /// Resolved provider id.
    pub provider_id: String,
    /// Prompt tokens consumed.
    pub prompt_tokens: u64,
    /// Completion tokens consumed.
    pub completion_tokens: u64,
    /// Reasoning tokens consumed (subset of completion on most providers).
    pub reasoning_tokens: u64,
    /// Cache-read prompt tokens.
    pub cache_read_tokens: u64,
    /// Cache-write prompt tokens.
    pub cache_write_tokens: u64,
    /// Estimated charge in micro-USD computed from pricing × tokens. `0`
    /// when pricing was unavailable (the recorder skips the math in that
    /// case and writes 0, plus emits a `PricingUnavailable` event).
    pub estimated_charge_micro_usd: i64,
    /// End-to-end latency in milliseconds.
    pub latency_ms: u64,
    /// Upstream generation time in milliseconds.
    pub generation_time_ms: u64,
    /// Whether the request was streamed.
    pub streamed: bool,
    /// Error string if the request failed, else `None`.
    pub error: Option<String>,
}
