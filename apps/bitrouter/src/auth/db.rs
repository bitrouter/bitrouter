//! Database access for the OSS auth module. This module owns the `users`
//! and `api_keys` tables; the rest of the binary coordinates via the
//! `Authenticated` event instead of reading `api_keys` directly.

use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};

use bitrouter_sdk::{BitrouterError, Result};

/// The SQL that creates this module's tables. Run once at startup.
pub const MIGRATION_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id          TEXT PRIMARY KEY,
    created_at  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS api_keys (
    id                     TEXT PRIMARY KEY,
    key_hash               TEXT NOT NULL UNIQUE,
    user_id                TEXT NOT NULL,
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

/// Create the auth tables on `pool`. Idempotent.
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
    /// Monthly spend ceiling in micro-USD, if any. Surfaced for the
    /// `policy` module via the per-policy `max_spend_micro_usd`; the auth
    /// module doesn't enforce it itself.
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

/// Look up an active api key by its SHA-256 hash. Only the **hash** is stored —
/// the plaintext secret is never persisted.
pub async fn find_key_by_hash(pool: &SqlitePool, key_hash: &str) -> Result<Option<ApiKeyRecord>> {
    let row = sqlx::query(
        "SELECT id, user_id, spend_limit_micro_usd, rpm_limit, \
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
        spend_limit_micro_usd: row.get("spend_limit_micro_usd"),
        rpm_limit: row.get("rpm_limit"),
        policy_id: row.get("policy_id"),
        expires_at,
        active: row.get::<i64, _>("active") != 0,
    }))
}

/// Reserved user ids — `CallerContext::local()` / `::anonymous()` synthesise
/// these and downstream code looks them up. A real user row with one of these
/// ids would silently merge with the synthesised caller, allowing a
/// credential-less request under `skip_auth: true` to spend a real user's
/// quota. Refuse to mint them.
const RESERVED_USER_IDS: &[&str] = &["local", "anonymous"];

/// Whether `user_id` collides with a reserved synthetic caller.
pub fn is_reserved_user_id(user_id: &str) -> bool {
    RESERVED_USER_IDS.contains(&user_id)
}

/// Insert a user row (idempotent on conflict). Rejects [`is_reserved_user_id`]
/// values — those names are owned by synthesised callers.
pub async fn upsert_user(pool: &SqlitePool, user_id: &str) -> Result<()> {
    if is_reserved_user_id(user_id) {
        return Err(BitrouterError::bad_request(format!(
            "user id '{user_id}' is reserved for synthesised callers"
        )));
    }
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
         (id, key_hash, user_id, spend_limit_micro_usd, rpm_limit, \
          policy_id, expires_at, active, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, NULL, 1, ?)",
    )
    .bind(&key.id)
    .bind(&key.key_hash)
    .bind(&key.user_id)
    .bind(key.spend_limit_micro_usd)
    .bind(key.rpm_limit)
    .bind(&key.policy_id)
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await
    .map_err(|e| BitrouterError::internal(format!("insert api key: {e}")))?;
    Ok(())
}
