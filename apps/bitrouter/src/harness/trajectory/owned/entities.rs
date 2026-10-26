//! Private owned projections of the existing capture/checkpoint graph.

pub(crate) mod connections {
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
        pub installation_id: Option<String>,
        pub tenant_id: Option<String>,
        pub creator_principal_id: Option<String>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

pub(crate) mod sessions {
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
        pub installation_id: Option<String>,
        pub tenant_id: Option<String>,
        pub creator_principal_id: Option<String>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

pub(crate) mod events {
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
        pub installation_id: Option<String>,
        pub tenant_id: Option<String>,
        pub creator_principal_id: Option<String>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

pub(crate) mod checkpoints {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "acp_checkpoints")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub checkpoint_id: String,
        pub session_key: String,
        pub watermark: i64,
        pub manifest_json: String,
        pub deleted: bool,
        pub installation_id: Option<String>,
        pub tenant_id: Option<String>,
        pub creator_principal_id: Option<String>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

pub(crate) mod members {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "acp_checkpoint_members")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub checkpoint_id: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub session_key: String,
        pub installation_id: Option<String>,
        pub tenant_id: Option<String>,
        pub creator_principal_id: Option<String>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

pub(crate) mod records {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "checkpoint_evolution")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub record_id: String,
        pub scope_id: String,
        pub kind: String,
        pub session_key: Option<String>,
        pub revision: i64,
        pub body: String,
        pub installation_id: Option<String>,
        pub tenant_id: Option<String>,
        pub creator_principal_id: Option<String>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}
