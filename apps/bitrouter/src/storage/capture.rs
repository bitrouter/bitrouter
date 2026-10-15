//! Persistence entities for the opt-in ACP content store.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Which side originated a captured message, or a controller lifecycle fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureDirection {
    /// A manager-originated request or notification.
    Client,
    /// A harness-originated request or notification.
    Agent,
    /// A fact observed by the controller itself.
    Controller,
}

/// Requests and responses share a connection-local `call_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureKind {
    /// Capture began for the controller connection.
    Connected,
    /// An outbound request, before forwarding.
    Request,
    /// A one-way protocol update, before forwarding.
    Notification,
    /// A response, before delivery to the requester.
    Response,
    /// The controller connection ended.
    Disconnected,
}

/// Observable protocol data, before any display projection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureEvent {
    /// Source side of this message.
    pub direction: CaptureDirection,
    /// Wire message kind or lifecycle fact.
    pub kind: CaptureKind,
    /// A correlation identifier, not a model request or native session ID.
    pub call_id: Option<u64>,
    /// ACP method; responses retain the originating request method.
    pub method: String,
    /// Request/notification parameters, or a response's `result` / `error`.
    pub payload: Value,
}

pub mod connections {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "acp_capture_connections")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub connection_id: String,
        pub owner: String,
        pub source: String,
        pub controller_instance_id: Option<String>,
        pub route_scope_id: Option<String>,
        pub state: String,
        pub head: i64,
        pub started_at: String,
        pub metadata_json: String,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

pub mod sessions {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "acp_canonical_sessions")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub session_key: String,
        pub owner: String,
        pub source: String,
        pub native_session_id: String,
        pub head: i64,
        pub history_origin: String,
        pub deleted: bool,
        pub parent_key: Option<String>,
        pub parent_watermark: Option<i64>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

pub mod events {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, serde::Serialize)]
    #[sea_orm(table_name = "acp_capture_events")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub connection_id: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub sequence: i64,
        pub session_key: Option<String>,
        pub session_sequence: Option<i64>,
        pub replay: bool,
        pub event_json: String,
        pub captured_at: String,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}
