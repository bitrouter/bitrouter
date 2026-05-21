//! The `users` table.

use sea_orm::entity::prelude::*;

/// One row of the `users` table.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "users")]
pub struct Model {
    /// The user id.
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// RFC3339 creation timestamp.
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
