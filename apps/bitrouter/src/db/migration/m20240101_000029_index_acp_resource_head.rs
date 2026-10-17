//! Keep latest resource revision lookup independent of retained history length.
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_index(
                Index::create()
                    .name("idx_acp_resources_checkpoint_revision")
                    .table(Alias::new("acp_resource_observations"))
                    .col(Alias::new("checkpoint_id"))
                    .col(Alias::new("revision"))
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_index(
                Index::drop()
                    .name("idx_acp_resources_checkpoint_revision")
                    .table(Alias::new("acp_resource_observations"))
                    .to_owned(),
            )
            .await
    }
}
