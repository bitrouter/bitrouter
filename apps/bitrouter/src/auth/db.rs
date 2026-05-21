//! Database access for the OSS auth module. This module owns the `users`
//! and `api_keys` tables; the rest of the binary coordinates via the
//! `Authenticated` event instead of reading `api_keys` directly.
//!
//! The tables themselves are created by the sea-orm migrations in
//! `crate::db::migration`; this module only reads and writes rows.

use chrono::{DateTime, Utc};
use sea_orm::sea_query::OnConflict;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter, Set,
};

use bitrouter_sdk::{BitrouterError, Result};

use crate::auth::entities::{api_keys, users};

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

/// Look up an api key by its SHA-256 hash. Only the **hash** is stored —
/// the plaintext secret is never persisted.
pub async fn find_key_by_hash(
    db: &DatabaseConnection,
    key_hash: &str,
) -> Result<Option<ApiKeyRecord>> {
    let row = api_keys::Entity::find()
        .filter(api_keys::Column::KeyHash.eq(key_hash))
        .one(db)
        .await
        .map_err(|e| BitrouterError::internal(format!("api_keys lookup: {e}")))?;

    let Some(row) = row else {
        return Ok(None);
    };
    let expires_at = row.expires_at.as_deref().and_then(|s| {
        DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.with_timezone(&Utc))
    });
    Ok(Some(ApiKeyRecord {
        id: row.id,
        user_id: row.user_id,
        spend_limit_micro_usd: row.spend_limit_micro_usd,
        rpm_limit: row.rpm_limit,
        policy_id: row.policy_id,
        expires_at,
        active: row.active != 0,
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
pub async fn upsert_user(db: &DatabaseConnection, user_id: &str) -> Result<()> {
    if is_reserved_user_id(user_id) {
        return Err(BitrouterError::bad_request(format!(
            "user id '{user_id}' is reserved for synthesised callers"
        )));
    }
    let row = users::ActiveModel {
        id: Set(user_id.to_string()),
        created_at: Set(Utc::now().to_rfc3339()),
    };
    // `do_nothing` makes this idempotent; on a conflict sea-orm reports
    // `RecordNotInserted`, which here is the success path, not an error.
    match users::Entity::insert(row)
        .on_conflict(
            OnConflict::column(users::Column::Id)
                .do_nothing()
                .to_owned(),
        )
        .exec(db)
        .await
    {
        Ok(_) | Err(DbErr::RecordNotInserted) => Ok(()),
        Err(e) => Err(BitrouterError::internal(format!("upsert user: {e}"))),
    }
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
pub async fn insert_api_key(db: &DatabaseConnection, key: &NewApiKey) -> Result<()> {
    let row = api_keys::ActiveModel {
        id: Set(key.id.clone()),
        key_hash: Set(key.key_hash.clone()),
        user_id: Set(key.user_id.clone()),
        spend_limit_micro_usd: Set(key.spend_limit_micro_usd),
        rpm_limit: Set(key.rpm_limit),
        policy_id: Set(key.policy_id.clone()),
        expires_at: Set(None),
        active: Set(1),
        created_at: Set(Utc::now().to_rfc3339()),
    };
    row.insert(db)
        .await
        .map_err(|e| BitrouterError::internal(format!("insert api key: {e}")))?;
    Ok(())
}
