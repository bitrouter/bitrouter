//! Create the owner-scoped, append-only trajectory event ledger.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(TrajectoryEpisodes::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(TrajectoryEpisodes::EpisodeId)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(TrajectoryEpisodes::OwnerUserId)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(TrajectoryEpisodes::CorrelationDigest)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(TrajectoryEpisodes::CorrelationKeyId)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(TrajectoryEpisodes::CorrelationSource)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(TrajectoryEpisodes::HistoryCompleteness)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(TrajectoryEpisodes::NextSequence)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(TrajectoryEpisodes::FirstCapturedAt)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(TrajectoryEpisodes::LastCapturedAt)
                            .string()
                            .not_null(),
                    )
                    .col(ColumnDef::new(TrajectoryEpisodes::ClosedAt).string())
                    .col(ColumnDef::new(TrajectoryEpisodes::LatestRequestId).string())
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(TrajectoryEvents::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(TrajectoryEvents::EventId)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(TrajectoryEvents::OwnerUserId)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(TrajectoryEvents::EpisodeId)
                            .string()
                            .not_null(),
                    )
                    .col(ColumnDef::new(TrajectoryEvents::RequestId).string())
                    .col(
                        ColumnDef::new(TrajectoryEvents::Sequence)
                            .big_integer()
                            .not_null(),
                    )
                    .col(ColumnDef::new(TrajectoryEvents::Kind).string().not_null())
                    .col(
                        ColumnDef::new(TrajectoryEvents::EventJson)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(TrajectoryEvents::ContentDigest)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(TrajectoryEvents::CapturedAt)
                            .string()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(TrajectoryRequests::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(TrajectoryRequests::RequestId)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(TrajectoryRequests::OwnerUserId)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(TrajectoryRequests::EpisodeId)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(TrajectoryRequests::StartEventId)
                            .string()
                            .not_null(),
                    )
                    .col(ColumnDef::new(TrajectoryRequests::SettlementEventId).string())
                    .col(ColumnDef::new(TrajectoryRequests::SettlementOutboxId).string())
                    .col(
                        ColumnDef::new(TrajectoryRequests::FullInputDigest)
                            .string()
                            .not_null(),
                    )
                    .col(ColumnDef::new(TrajectoryRequests::NativeParentId).string())
                    .col(
                        ColumnDef::new(TrajectoryRequests::Protocol)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(TrajectoryRequests::Status)
                            .string()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_table(trajectory_prefix_index_table())
            .await?;
        manager
            .create_table(
                Table::create()
                    .table(TrajectoryOutbox::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(TrajectoryOutbox::OutboxId)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(TrajectoryOutbox::OwnerUserId)
                            .string()
                            .not_null(),
                    )
                    .col(ColumnDef::new(TrajectoryOutbox::Topic).string().not_null())
                    .col(
                        ColumnDef::new(TrajectoryOutbox::PayloadJson)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(TrajectoryOutbox::PayloadDigest)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(TrajectoryOutbox::Attempts)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(TrajectoryOutbox::CreatedAt)
                            .string()
                            .not_null(),
                    )
                    .col(ColumnDef::new(TrajectoryOutbox::DeliveredAt).string())
                    .to_owned(),
            )
            .await?;
        let mut pending = Index::create();
        pending
            .if_not_exists()
            .table(TrajectoryOutbox::Table)
            .col(TrajectoryOutbox::OwnerUserId);
        if manager.get_database_backend() == sea_orm::DatabaseBackend::MySql {
            // Four utf8mb4 VARCHAR(255) keys exceed InnoDB's 3072-byte limit.
            // Prefix only the timestamp keys in this nonunique index: stored
            // timestamps and full owner/outbox identities remain unchanged.
            // https://dev.mysql.com/doc/refman/8.4/en/innodb-limits.html
            // https://dev.mysql.com/doc/refman/8.4/en/column-indexes.html
            pending
                .col((TrajectoryOutbox::DeliveredAt, 64))
                .col(TrajectoryOutbox::Attempts)
                .col((TrajectoryOutbox::CreatedAt, 64));
        } else {
            pending
                .col(TrajectoryOutbox::DeliveredAt)
                .col(TrajectoryOutbox::Attempts)
                .col(TrajectoryOutbox::CreatedAt);
        }
        pending.col(TrajectoryOutbox::OutboxId);

        for (table, name, mut index) in [
            (
                "trajectory_episodes",
                "idx_trajectory_episodes_owner_correlation",
                Index::create()
                    .if_not_exists()
                    .table(TrajectoryEpisodes::Table)
                    .col(TrajectoryEpisodes::OwnerUserId)
                    .col(TrajectoryEpisodes::CorrelationDigest)
                    .to_owned(),
            ),
            (
                "trajectory_events",
                "idx_trajectory_events_episode_sequence",
                Index::create()
                    .if_not_exists()
                    .table(TrajectoryEvents::Table)
                    .col(TrajectoryEvents::EpisodeId)
                    .col(TrajectoryEvents::Sequence)
                    .unique()
                    .to_owned(),
            ),
            (
                "trajectory_requests",
                "idx_trajectory_requests_owner_full_input_digest",
                trajectory_requests_owner_full_input_digest_index(),
            ),
            (
                "trajectory_requests",
                "idx_trajectory_requests_owner_episode",
                Index::create()
                    .if_not_exists()
                    .table(TrajectoryRequests::Table)
                    .col(TrajectoryRequests::OwnerUserId)
                    .col(TrajectoryRequests::EpisodeId)
                    .to_owned(),
            ),
            (
                "trajectory_outbox",
                "idx_trajectory_outbox_pending",
                pending,
            ),
            (
                "trajectory_outbox",
                "idx_trajectory_outbox_delivery_order",
                trajectory_outbox_delivery_order_index(),
            ),
        ] {
            // MySQL commits DDL statements individually and lacks CREATE INDEX
            // IF NOT EXISTS. Resume an interrupted migration without dropping
            // its existing tables, rows or unique indexes.
            // https://dev.mysql.com/doc/refman/8.4/en/implicit-commit.html
            // https://dev.mysql.com/doc/refman/8.4/en/create-index.html
            if manager.get_database_backend() == sea_orm::DatabaseBackend::MySql
                && manager.has_index(table, name).await?
            {
                continue;
            }
            index.name(name);
            manager.create_index(index).await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(
                Table::drop()
                    .table(TrajectoryOutbox::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await?;
        manager
            .drop_table(
                Table::drop()
                    .table(TrajectoryPrefixIndex::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await?;
        manager
            .drop_table(
                Table::drop()
                    .table(TrajectoryRequests::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await?;
        manager
            .drop_table(
                Table::drop()
                    .table(TrajectoryEvents::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await?;
        manager
            .drop_table(
                Table::drop()
                    .table(TrajectoryEpisodes::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await?;
        Ok(())
    }
}

pub(crate) fn trajectory_requests_owner_full_input_digest_index() -> IndexCreateStatement {
    Index::create()
        .if_not_exists()
        .name("idx_trajectory_requests_owner_full_input_digest")
        .table(TrajectoryRequests::Table)
        .col(TrajectoryRequests::OwnerUserId)
        .col(TrajectoryRequests::FullInputDigest)
        .to_owned()
}

pub(crate) fn trajectory_prefix_index_table() -> TableCreateStatement {
    Table::create()
        .table(TrajectoryPrefixIndex::Table)
        .if_not_exists()
        .col(
            ColumnDef::new(TrajectoryPrefixIndex::OwnerUserId)
                .string()
                .not_null(),
        )
        .col(
            ColumnDef::new(TrajectoryPrefixIndex::FullInputDigest)
                .string()
                .not_null(),
        )
        .col(
            ColumnDef::new(TrajectoryPrefixIndex::EpisodeId)
                .string()
                .not_null(),
        )
        .col(
            ColumnDef::new(TrajectoryPrefixIndex::Ambiguous)
                .boolean()
                .not_null(),
        )
        .primary_key(
            Index::create()
                .col(TrajectoryPrefixIndex::OwnerUserId)
                .col(TrajectoryPrefixIndex::FullInputDigest),
        )
        .to_owned()
}

pub(crate) fn trajectory_outbox_delivery_order_index() -> IndexCreateStatement {
    Index::create()
        .if_not_exists()
        .name("idx_trajectory_outbox_delivery_order")
        .table(TrajectoryOutbox::Table)
        .col(TrajectoryOutbox::DeliveredAt)
        .col(TrajectoryOutbox::Attempts)
        .col(TrajectoryOutbox::CreatedAt)
        .col(TrajectoryOutbox::OutboxId)
        .to_owned()
}

#[derive(DeriveIden)]
enum TrajectoryEpisodes {
    Table,
    EpisodeId,
    OwnerUserId,
    CorrelationDigest,
    CorrelationKeyId,
    CorrelationSource,
    HistoryCompleteness,
    NextSequence,
    FirstCapturedAt,
    LastCapturedAt,
    ClosedAt,
    LatestRequestId,
}

#[derive(DeriveIden)]
enum TrajectoryEvents {
    Table,
    EventId,
    OwnerUserId,
    EpisodeId,
    RequestId,
    Sequence,
    Kind,
    EventJson,
    ContentDigest,
    CapturedAt,
}

#[derive(DeriveIden)]
enum TrajectoryRequests {
    Table,
    RequestId,
    OwnerUserId,
    EpisodeId,
    StartEventId,
    SettlementEventId,
    SettlementOutboxId,
    FullInputDigest,
    NativeParentId,
    Protocol,
    Status,
}

#[derive(DeriveIden)]
enum TrajectoryPrefixIndex {
    Table,
    OwnerUserId,
    FullInputDigest,
    EpisodeId,
    Ambiguous,
}

#[derive(DeriveIden)]
enum TrajectoryOutbox {
    Table,
    OutboxId,
    OwnerUserId,
    Topic,
    PayloadJson,
    PayloadDigest,
    Attempts,
    CreatedAt,
    DeliveredAt,
}
