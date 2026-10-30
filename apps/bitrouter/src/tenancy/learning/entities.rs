//! Private projections of033's existing assessment tables.
use crate::harness::trajectory::owned::io::Columns;

pub(in crate::tenancy) mod revisions {
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
        pub installation_id: Option<String>,
        pub tenant_id: Option<String>,
        pub creator_principal_id: Option<String>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}
pub(in crate::tenancy) mod heads {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "acp_effective_assessments")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub session_key: String,
        pub revision_id: Option<String>,
        pub installation_id: Option<String>,
        pub tenant_id: Option<String>,
        pub creator_principal_id: Option<String>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}
impl Columns for revisions::Model {
    const TEXT: &'static [&'static str] = &[
        "revision_id",
        "session_key",
        "checkpoint_id",
        "created_at",
        "revision_json",
        "installation_id",
        "tenant_id",
        "creator_principal_id",
    ];
}
impl Columns for heads::Model {
    const TEXT: &'static [&'static str] = &[
        "session_key",
        "revision_id",
        "installation_id",
        "tenant_id",
        "creator_principal_id",
    ];
}
