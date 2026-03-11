//! API key entity.

use chrono::NaiveDateTime;
use sea_orm::entity::prelude::*;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "api_keys")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub account_id: Uuid,
    /// A human-readable label for this key.
    pub name: String,
    /// Prefix of the key shown to the user (e.g. "sk-abc...").
    pub prefix: String,
    /// SHA-256 hash of the full key. The plaintext is never stored.
    #[sea_orm(unique)]
    pub key_hash: String,
    pub created_at: NaiveDateTime,
    pub expires_at: Option<NaiveDateTime>,
    pub revoked_at: Option<NaiveDateTime>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::account::Entity",
        from = "Column::AccountId",
        to = "super::account::Column::Id"
    )]
    Account,
}

impl Related<super::account::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Account.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
