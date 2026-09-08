//! Application-owned checkpoint, revision and resource observation tables.

pub mod checkpoints {
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
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

pub mod members {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "acp_checkpoint_members")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub checkpoint_id: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub session_key: String,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

pub mod resources {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "acp_resource_observations")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub observation_id: String,
        pub checkpoint_id: String,
        pub revision: i64,
        pub observed_at: String,
        pub observation_json: String,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

pub mod revisions {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "acp_assessment_revisions")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub revision_id: String,
        pub session_key: String,
        pub checkpoint_id: String,
        pub created_at: String,
        pub revision_json: String,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

pub mod heads {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "acp_effective_assessments")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub session_key: String,
        pub revision_id: Option<String>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}
