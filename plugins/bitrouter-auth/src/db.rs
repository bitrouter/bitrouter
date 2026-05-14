//! Database access for `bitrouter-auth`. This plugin **owns** the `users` and
//! `api_keys` tables; no other plugin reads or writes them (plugin DB
//! isolation, 003 §2.3). All access goes through the typed API here.

use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};

use bitrouter_sdk::caller::PaymentMethod;
use bitrouter_sdk::{BitrouterError, MigrationItem, Result};

/// The SQL that creates this plugin's tables. Run once at startup.
pub const MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id          TEXT PRIMARY KEY,
    created_at  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS api_keys (
    id                     TEXT PRIMARY KEY,
    key_hash               TEXT NOT NULL UNIQUE,
    user_id                TEXT NOT NULL,
    payment_method         TEXT NOT NULL,
    spend_limit_micro_usd  INTEGER,
    rpm_limit              INTEGER,
    policy_id              TEXT,
    expires_at             TEXT,
    active                 INTEGER NOT NULL DEFAULT 1,
    created_at             TEXT NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users(id)
);
CREATE INDEX IF NOT EXISTS idx_api_keys_hash ON api_keys(key_hash);
"#;

/// This plugin's migration set, for `Plugin::migrations()`.
pub fn migrations() -> Vec<MigrationItem> {
    vec![MigrationItem::sql(
        1_000,
        vec!["users".to_string(), "api_keys".to_string()],
        MIGRATION_SQL,
    )]
}

/// Create this plugin's tables on `pool`. Idempotent.
pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    for stmt in MIGRATION_SQL.split(';') {
        let stmt = stmt.trim();
        if stmt.is_empty() {
            continue;
        }
        sqlx::query(stmt)
            .execute(pool)
            .await
            .map_err(|e| BitrouterError::internal(format!("auth migration: {e}")))?;
    }
    Ok(())
}

/// One row of the `api_keys` table, in typed form.
#[derive(Debug, Clone)]
pub struct ApiKeyRecord {
    /// The key id (e.g. `brvk_…` prefix-stable id, distinct from the secret).
    pub id: String,
    /// Owning user id.
    pub user_id: String,
    /// How this key pays.
    pub payment_method: PaymentMethod,
    /// Monthly spend ceiling in micro-USD, if any.
    pub spend_limit_micro_usd: Option<i64>,
    /// Requests-per-minute ceiling, if any.
    pub rpm_limit: Option<i64>,
    /// The policy id this key is bound to, if any.
    pub policy_id: Option<String>,
    /// Expiry timestamp, if any.
    pub expires_at: Option<DateTime<Utc>>,
    /// Whether the key is active.
    pub active: bool,
}

fn payment_method_from_str(s: &str) -> PaymentMethod {
    match s {
        "credits" => PaymentMethod::Credits,
        "mpp" => PaymentMethod::Mpp,
        "byok" => PaymentMethod::Byok,
        _ => PaymentMethod::None,
    }
}

fn payment_method_to_str(m: PaymentMethod) -> &'static str {
    match m {
        PaymentMethod::Credits => "credits",
        PaymentMethod::Mpp => "mpp",
        PaymentMethod::Byok => "byok",
        PaymentMethod::None => "none",
    }
}

/// Look up an active api key by its SHA-256 hash. Only the **hash** is stored —
/// the plaintext secret is never persisted.
pub async fn find_key_by_hash(pool: &SqlitePool, key_hash: &str) -> Result<Option<ApiKeyRecord>> {
    let row = sqlx::query(
        "SELECT id, user_id, payment_method, spend_limit_micro_usd, rpm_limit, \
         policy_id, expires_at, active FROM api_keys WHERE key_hash = ?",
    )
    .bind(key_hash)
    .fetch_optional(pool)
    .await
    .map_err(|e| BitrouterError::internal(format!("api_keys lookup: {e}")))?;

    let Some(row) = row else {
        return Ok(None);
    };
    let expires_at = row.get::<Option<String>, _>("expires_at").and_then(|s| {
        DateTime::parse_from_rfc3339(&s)
            .ok()
            .map(|d| d.with_timezone(&Utc))
    });
    Ok(Some(ApiKeyRecord {
        id: row.get("id"),
        user_id: row.get("user_id"),
        payment_method: payment_method_from_str(&row.get::<String, _>("payment_method")),
        spend_limit_micro_usd: row.get("spend_limit_micro_usd"),
        rpm_limit: row.get("rpm_limit"),
        policy_id: row.get("policy_id"),
        expires_at,
        active: row.get::<i64, _>("active") != 0,
    }))
}

/// Insert a user row (idempotent on conflict).
pub async fn upsert_user(pool: &SqlitePool, user_id: &str) -> Result<()> {
    sqlx::query("INSERT OR IGNORE INTO users (id, created_at) VALUES (?, ?)")
        .bind(user_id)
        .bind(Utc::now().to_rfc3339())
        .execute(pool)
        .await
        .map_err(|e| BitrouterError::internal(format!("upsert user: {e}")))?;
    Ok(())
}

/// Parameters for inserting a new api key.
#[derive(Debug, Clone)]
pub struct NewApiKey {
    /// The key id.
    pub id: String,
    /// SHA-256 hash of the plaintext secret.
    pub key_hash: String,
    /// Owning user id.
    pub user_id: String,
    /// How the key pays.
    pub payment_method: PaymentMethod,
    /// Monthly spend ceiling, if any.
    pub spend_limit_micro_usd: Option<i64>,
    /// RPM ceiling, if any.
    pub rpm_limit: Option<i64>,
    /// Bound policy id, if any.
    pub policy_id: Option<String>,
}

/// Insert a new api key row. The caller is responsible for having created the
/// `users` row first (or call [`upsert_user`]).
pub async fn insert_api_key(pool: &SqlitePool, key: &NewApiKey) -> Result<()> {
    sqlx::query(
        "INSERT INTO api_keys \
         (id, key_hash, user_id, payment_method, spend_limit_micro_usd, rpm_limit, \
          policy_id, expires_at, active, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, NULL, 1, ?)",
    )
    .bind(&key.id)
    .bind(&key.key_hash)
    .bind(&key.user_id)
    .bind(payment_method_to_str(key.payment_method))
    .bind(key.spend_limit_micro_usd)
    .bind(key.rpm_limit)
    .bind(&key.policy_id)
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await
    .map_err(|e| BitrouterError::internal(format!("insert api key: {e}")))?;
    Ok(())
}
