//! Session file (blob) index entity.
//!
//! Each row tracks one blob stored via [`BlobStore`](bitrouter_core::blob::BlobStore)
//! that belongs to a session. The `blob_key` is the logical key in the blob
//! store; the `message_id` links the blob to the specific message content part
//! that references it (if known at insertion time).

use chrono::NaiveDateTime;
use sea_orm::entity::prelude::*;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "session_files")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub session_id: Uuid,
    /// The message this blob belongs to, if known.
    pub message_id: Option<Uuid>,
    /// Logical key in the blob store (e.g. `"sessions/{sid}/{uuid}.png"`).
    #[sea_orm(unique)]
    pub blob_key: String,
    /// Original filename, if available.
    pub filename: Option<String>,
    /// IANA media type (e.g. `"image/png"`).
    pub media_type: String,
    /// Size in bytes.
    pub size_bytes: i64,
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
    #[sea_orm(
        belongs_to = "super::message::Entity",
        from = "Column::MessageId",
        to = "super::message::Column::Id"
    )]
    Message,
}

impl Related<super::session::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Session.def()
    }
}

impl Related<super::message::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Message.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
