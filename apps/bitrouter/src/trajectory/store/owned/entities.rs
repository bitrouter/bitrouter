//! SQL mappings for owned rows; Legacy/shared projections stay unchanged.

pub(super) mod episode_entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, DeriveEntityModel)]
    #[sea_orm(table_name = "trajectory_episodes")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub episode_id: String,
        pub owner_user_id: String,
        pub correlation_digest: String,
        pub correlation_key_id: String,
        pub correlation_source: String,
        pub history_completeness: String,
        pub next_sequence: i64,
        pub first_captured_at: String,
        pub last_captured_at: String,
        pub closed_at: Option<String>,
        pub latest_request_id: Option<String>,
        pub installation_id: Option<String>,
        pub tenant_id: Option<String>,
        pub creator_principal_id: Option<String>,
        pub logical_identity_json: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

pub(super) mod event_entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, DeriveEntityModel)]
    #[sea_orm(table_name = "trajectory_events")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub event_id: String,
        pub owner_user_id: String,
        pub episode_id: String,
        pub request_id: Option<String>,
        pub sequence: i64,
        pub kind: String,
        pub event_json: String,
        pub content_digest: String,
        pub captured_at: String,
        pub installation_id: Option<String>,
        pub tenant_id: Option<String>,
        pub creator_principal_id: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

pub(super) mod request_entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, DeriveEntityModel)]
    #[sea_orm(table_name = "trajectory_requests")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub request_id: String,
        pub owner_user_id: String,
        pub episode_id: String,
        pub start_event_id: String,
        pub settlement_event_id: Option<String>,
        pub settlement_outbox_id: Option<String>,
        pub full_input_digest: String,
        pub native_parent_id: Option<String>,
        pub protocol: String,
        pub status: String,
        pub installation_id: Option<String>,
        pub tenant_id: Option<String>,
        pub creator_principal_id: Option<String>,
        pub logical_identity_json: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

pub(super) mod prefix_entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, DeriveEntityModel)]
    #[sea_orm(table_name = "trajectory_prefix_index")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub owner_user_id: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub full_input_digest: String,
        pub episode_id: String,
        pub ambiguous: bool,
        pub installation_id: Option<String>,
        pub tenant_id: Option<String>,
        pub creator_principal_id: Option<String>,
        pub logical_identity_json: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

pub(super) mod outbox_entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, DeriveEntityModel)]
    #[sea_orm(table_name = "trajectory_outbox")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub outbox_id: String,
        pub owner_user_id: String,
        pub topic: String,
        pub payload_json: String,
        pub payload_digest: String,
        pub attempts: i64,
        pub created_at: String,
        pub delivered_at: Option<String>,
        pub installation_id: Option<String>,
        pub tenant_id: Option<String>,
        pub creator_principal_id: Option<String>,
        pub logical_identity_json: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}
