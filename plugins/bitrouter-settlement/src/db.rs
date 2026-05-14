//! Database schema for `bitrouter-settlement`.
//!
//! This plugin owns five tables — `requests` (receipts + usage metrics source),
//! `credit_accounts`, `credit_ledger_entries`, `byok_provider_keys`,
//! `mpp_sessions`. Each is touched only by its dedicated hook module (plugin DB
//! isolation, 004 §7.2):
//!
//! | table                   | owner module          |
//! |-------------------------|-----------------------|
//! | `requests`              | [`crate::metrics_store`] |
//! | `credit_accounts`       | [`crate::charge`] (`CreditCharge`) |
//! | `credit_ledger_entries` | [`crate::charge`] (`CreditCharge`) |
//! | `byok_provider_keys`    | [`crate::byok`]       |
//! | `mpp_sessions`          | [`crate::charge`] (`MppCharge`) / [`crate::balance`] |

use sqlx::SqlitePool;

use bitrouter_sdk::{BitrouterError, MigrationItem, Result};

/// SQL that creates every table this plugin owns.
pub const MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS requests (
    request_id             TEXT PRIMARY KEY,
    user_id                TEXT NOT NULL,
    api_key_id             TEXT NOT NULL,
    model_id               TEXT NOT NULL,
    provider_id            TEXT NOT NULL,
    prompt_tokens          INTEGER NOT NULL DEFAULT 0,
    completion_tokens      INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens       INTEGER NOT NULL DEFAULT 0,
    final_charge_micro_usd INTEGER NOT NULL DEFAULT 0,
    funding_source         TEXT NOT NULL,
    byok_used              INTEGER NOT NULL DEFAULT 0,
    streamed               INTEGER NOT NULL DEFAULT 0,
    latency_ms             INTEGER NOT NULL DEFAULT 0,
    generation_time_ms     INTEGER NOT NULL DEFAULT 0,
    error                  TEXT,
    created_at             TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_requests_api_key ON requests(api_key_id, created_at);
CREATE INDEX IF NOT EXISTS idx_requests_user ON requests(user_id, created_at);

CREATE TABLE IF NOT EXISTS credit_accounts (
    user_id           TEXT PRIMARY KEY,
    balance_micro_usd INTEGER NOT NULL DEFAULT 0,
    updated_at        TEXT NOT NULL
);

-- Append-only ledger of every balance change (004 §7.5). `idempotency_key` is
-- UNIQUE so a retried charge cannot be applied twice; NULL keys (manual
-- top-ups) are exempt — sqlite permits multiple NULLs in a UNIQUE column.
CREATE TABLE IF NOT EXISTS credit_ledger_entries (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id          TEXT NOT NULL,
    delta_micro_usd  INTEGER NOT NULL,
    request_id       TEXT,
    idempotency_key  TEXT UNIQUE,
    created_at       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_credit_ledger_user
    ON credit_ledger_entries(user_id, created_at);

CREATE TABLE IF NOT EXISTS byok_provider_keys (
    id          TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL,
    provider    TEXT NOT NULL,
    api_key     TEXT NOT NULL,
    api_base    TEXT,
    active      INTEGER NOT NULL DEFAULT 1,
    created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_byok_user_provider
    ON byok_provider_keys(user_id, provider);

CREATE TABLE IF NOT EXISTS mpp_sessions (
    session_id        TEXT PRIMARY KEY,
    user_id           TEXT NOT NULL,
    channel           TEXT NOT NULL,
    balance_micro_usd INTEGER NOT NULL DEFAULT 0,
    last_checkpoint_micro_usd INTEGER NOT NULL DEFAULT 0,
    updated_at        TEXT NOT NULL
);
"#;

/// This plugin's migration set, for `Plugin::migrations()`.
pub fn migrations() -> Vec<MigrationItem> {
    vec![MigrationItem::sql(
        2_000,
        vec![
            "requests".to_string(),
            "credit_accounts".to_string(),
            "credit_ledger_entries".to_string(),
            "byok_provider_keys".to_string(),
            "mpp_sessions".to_string(),
        ],
        MIGRATION_SQL,
    )]
}

/// Create this plugin's tables on `pool`. Idempotent.
pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    // Strip `--` line comments *before* splitting on `;` — a comment may
    // legitimately contain a semicolon, which would otherwise cut a statement
    // in half.
    let sql: String = MIGRATION_SQL
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    for stmt in sql.split(';') {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        sqlx::query(stmt)
            .execute(pool)
            .await
            .map_err(|e| BitrouterError::internal(format!("settlement migration: {e}")))?;
    }
    Ok(())
}
