//! Revoked API key entity.
//!
//! Each row records a revoked JWT key `id` claim. The `key_id` is the
//! base64url-encoded 32-byte identifier embedded in the JWT at issuance
//! time. Once a key ID appears in this table, any JWT carrying that `id`
//! claim is rejected during authentication.

use chrono::NaiveDateTime;
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "revoked_keys")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub key_id: String,
    pub revoked_at: NaiveDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
