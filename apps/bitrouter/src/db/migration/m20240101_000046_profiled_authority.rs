//! Fence ordered-only authority writers before enabling profiled resources.

use sea_orm::ConnectionTrait;
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let db = manager.get_connection();
        let backend = manager.get_database_backend();
        super::m20240101_000045_ordered_receipts::validate(db).await?;

        let invalid = Query::select()
            .from(Alias::new("tenancy_installation"))
            .expr(Expr::val(1))
            .and_where(Expr::col(Alias::new("schema_version")).is_not_in([1, 2]))
            .limit(1)
            .to_owned();
        if db.query_one(backend.build(&invalid)).await?.is_some() {
            return Err(DbErr::Custom("incompatible profiled authority".into()));
        }

        // Format 1 still requires main's canonical validation and saturation
        // handling. Leave it untouched until the owned startup/offline upgrade.
        let update = Query::update()
            .table(Alias::new("tenancy_installation"))
            .value(Alias::new("schema_version"), 3)
            .and_where(Expr::col(Alias::new("singleton")).eq(1))
            .and_where(Expr::col(Alias::new("schema_version")).eq(2))
            .to_owned();
        db.execute(backend.build(&update)).await?;
        Ok(())
    }

    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "profiled authority requires verified-backup rollback".into(),
        ))
    }
}
