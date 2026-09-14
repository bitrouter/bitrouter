//! Persistence entities for the opt-in ACP content store.

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
