//! Account entity.

use chrono::NaiveDateTime;
use sea_orm::entity::prelude::*;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "accounts")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub name: String,
    /// CAIP-10 identity string (e.g. `solana:5eykt...:BASE58_KEY`), used for
    /// JWT authentication.
    #[sea_orm(unique)]
    pub caip10_identity: Option<String>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::rotated_pubkey::Entity")]
    RotatedPubkeys,
    #[sea_orm(has_many = "super::session::Entity")]
    Sessions,
}

impl Related<super::rotated_pubkey::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::RotatedPubkeys.def()
    }
}

impl Related<super::session::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sessions.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
