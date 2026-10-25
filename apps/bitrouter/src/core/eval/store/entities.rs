pub(super) mod subject_entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "eval_subjects")]
    pub struct Model {
        pub installation_id: Option<String>,
        pub tenant_id: Option<String>,
        pub creator_principal_id: Option<String>,
        #[sea_orm(primary_key, auto_increment = false)]
        pub eval_id: String,
        pub owner_user_id: String,
        pub subject_id: String,
        pub scope: String,
        pub policy_digest: String,
        pub holdout: bool,
        pub content_digest: String,
        pub subject_json: String,
        pub created_at: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

pub(super) mod result_entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "eval_results")]
    pub struct Model {
        pub installation_id: Option<String>,
        pub tenant_id: Option<String>,
        pub creator_principal_id: Option<String>,
        #[sea_orm(primary_key, auto_increment = false)]
        pub result_id: String,
        pub owner_user_id: String,
        pub eval_id: String,
        pub idempotency_key: String,
        pub authority_id: String,
        pub evaluator_kind: String,
        pub content_digest: String,
        pub result_json: String,
        pub created_at: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

pub(super) mod admission_entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "eval_admission_events")]
    pub struct Model {
        pub installation_id: Option<String>,
        pub tenant_id: Option<String>,
        pub creator_principal_id: Option<String>,
        #[sea_orm(primary_key)]
        pub sequence: i64,
        pub result_id: String,
        pub status: String,
        pub reason: String,
        pub authority_id: String,
        pub created_at: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

pub(super) mod snapshot_entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "eval_snapshots")]
    pub struct Model {
        pub installation_id: Option<String>,
        pub tenant_id: Option<String>,
        pub creator_principal_id: Option<String>,
        #[sea_orm(primary_key, auto_increment = false)]
        pub evidence_root: String,
        pub owner_user_id: String,
        pub manifest_json: String,
        pub result_count: i32,
        pub frozen_at: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}
