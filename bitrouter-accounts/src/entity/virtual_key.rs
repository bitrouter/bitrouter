//! Virtual key entity.
//!
//! A virtual key is a short opaque credential that maps 1-to-1 to a stored
//! JWT. Only the SHA-256 hash of the virtual key is persisted; the raw
//! virtual key is shown once to the caller at creation time.

use chrono::NaiveDateTime;
use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "virtual_keys")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub key_hash: String,
    pub jwt: String,
    pub created_at: NaiveDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
