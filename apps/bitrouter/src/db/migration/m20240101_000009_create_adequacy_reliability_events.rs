//! Persist ordered, content-free provider reliability observations.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(AdequacyReliabilityEvents::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(AdequacyReliabilityEvents::Sequence)
                            .big_integer()
                            .not_null()
                            .auto_increment()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(AdequacyReliabilityEvents::RequestId)
                            .string()
                            .not_null()
                            .unique_key(),
                    )
                    .col(
                        ColumnDef::new(AdequacyReliabilityEvents::RouteKey)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AdequacyReliabilityEvents::Provider)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AdequacyReliabilityEvents::Model)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AdequacyReliabilityEvents::CredentialClass)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AdequacyReliabilityEvents::EndpointScope)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AdequacyReliabilityEvents::Protocol)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AdequacyReliabilityEvents::Observation)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AdequacyReliabilityEvents::HalfOpenProbe)
                            .boolean()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AdequacyReliabilityEvents::ObservedAtUnix)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(AdequacyReliabilityEvents::CreatedAt)
                            .string()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(
                Table::drop()
                    .table(AdequacyReliabilityEvents::Table)
                    .to_owned(),
            )
            .await?;
        Ok(())
    }
}

#[derive(DeriveIden)]
enum AdequacyReliabilityEvents {
    Table,
    Sequence,
    RequestId,
    RouteKey,
    Provider,
    Model,
    CredentialClass,
    EndpointScope,
    Protocol,
    Observation,
    HalfOpenProbe,
    ObservedAtUnix,
    CreatedAt,
}
