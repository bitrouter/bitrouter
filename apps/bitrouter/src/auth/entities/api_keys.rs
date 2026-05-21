//! The `api_keys` table.

use sea_orm::entity::prelude::*;

/// One row of the `api_keys` table. Only the SHA-256 **hash** of a virtual
/// key is stored — never the plaintext secret.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "api_keys")]
pub struct Model {
    /// The key id (e.g. `brvk_id_…`, distinct from the secret).
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// Hex-encoded SHA-256 of the plaintext secret.
    pub key_hash: String,
    /// Owning user id.
    pub user_id: String,
    /// Monthly spend ceiling in micro-USD, if any.
    pub spend_limit_micro_usd: Option<i64>,
    /// Requests-per-minute ceiling, if any.
    pub rpm_limit: Option<i64>,
    /// The policy id this key is bound to, if any.
    pub policy_id: Option<String>,
    /// RFC3339 expiry timestamp, if any.
    pub expires_at: Option<String>,
    /// Whether the key is active (`1`) or revoked (`0`).
    pub active: i32,
    /// RFC3339 creation timestamp.
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
