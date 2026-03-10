//! Message entity.
//!
//! Each row stores one message in a session as a JSON blob matching
//! the [`LanguageModelMessage`](bitrouter_core::models::language::prompt::LanguageModelMessage)
//! schema. Keeping the payload as JSON avoids a rigid relational mapping
//! of every content variant while still allowing SQL-level queries on
//! the `role` and `position` columns.

use chrono::NaiveDateTime;
use sea_orm::entity::prelude::*;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "messages")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub session_id: Uuid,
    /// Ordering index within the session (0-based).
    pub position: i32,
    /// Denormalized role for indexing: "system", "user", "assistant", "tool".
    pub role: String,
    /// Full message payload as JSON ([`LanguageModelMessage`] serialization).
    #[sea_orm(column_type = "Text")]
    pub payload: String,
    pub created_at: NaiveDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::session::Entity",
        from = "Column::SessionId",
        to = "super::session::Column::Id"
    )]
    Session,
}

impl Related<super::session::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Session.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
