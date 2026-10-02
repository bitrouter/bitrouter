//! Add nullable ACP/native request-session correlation to OSS metering.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for column in [
            ColumnDef::new(Requests::AgentHarness).string().to_owned(),
            ColumnDef::new(Requests::ControllerInstanceId)
                .string()
                .to_owned(),
            ColumnDef::new(Requests::AcpSessionId).string().to_owned(),
            ColumnDef::new(Requests::NativeRootSessionId)
                .string()
                .to_owned(),
            ColumnDef::new(Requests::NativeAgentThreadId)
                .string()
                .to_owned(),
            ColumnDef::new(Requests::NativeParentAgentThreadId)
                .string()
                .to_owned(),
            ColumnDef::new(Requests::NativeTurnId).string().to_owned(),
            ColumnDef::new(Requests::RouteLeaseId).string().to_owned(),
            ColumnDef::new(Requests::SessionIdentityJson)
                .text()
                .to_owned(),
        ] {
            manager
                .alter_table(
                    Table::alter()
                        .table(Requests::Table)
                        .add_column(column)
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for column in [
            Requests::SessionIdentityJson,
            Requests::RouteLeaseId,
            Requests::NativeTurnId,
            Requests::NativeParentAgentThreadId,
            Requests::NativeAgentThreadId,
            Requests::NativeRootSessionId,
            Requests::AcpSessionId,
            Requests::ControllerInstanceId,
            Requests::AgentHarness,
        ] {
            manager
                .alter_table(
                    Table::alter()
                        .table(Requests::Table)
                        .drop_column(column)
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }
}

#[derive(DeriveIden)]
enum Requests {
    Table,
    AgentHarness,
    ControllerInstanceId,
    AcpSessionId,
    NativeRootSessionId,
    NativeAgentThreadId,
    NativeParentAgentThreadId,
    NativeTurnId,
    RouteLeaseId,
    SessionIdentityJson,
}
