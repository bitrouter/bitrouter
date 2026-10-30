//! Additive owned OAuth slots and optional private receipt evidence. No Legacy backfill.
use super::m20240101_000028_resource_tenancy::{identity, payload};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement, Value};
use sea_orm_migration::prelude::*;
mod catalog;
#[cfg(test)]
mod tests;

#[derive(DeriveMigrationName)]
pub struct Migration;
#[derive(Clone, Copy)]
enum Kind {
    Id(u32),
    Payload,
    Integer,
    Boolean,
}
struct Spec {
    table: &'static str,
    columns: &'static [(&'static str, Kind)],
    primary: &'static [&'static str],
    index: Option<(&'static str, &'static [&'static str])>,
}
const SPECS: &[Spec] = &[
    Spec {
        table: "tenancy_oauth_slots",
        columns: &[
            ("slot_key", Kind::Id(64)),
            ("installation_id", Kind::Id(128)),
            ("owner_kind", Kind::Id(32)),
            ("owner_id", Kind::Id(128)),
            ("credential_resource_id", Kind::Id(128)),
            ("authority_revision", Kind::Id(128)),
            ("generation", Kind::Integer),
            ("state", Kind::Id(32)),
            ("metadata_json", Kind::Payload),
            ("payload_json", Kind::Payload),
            ("content_digest", Kind::Id(64)),
        ],
        primary: &["slot_key"],
        index: Some((
            "idx_tenancy_oauth_pending",
            &["installation_id", "state", "slot_key"],
        )),
    },
    Spec {
        table: "tenancy_receipt_auxiliary",
        columns: &[
            ("installation_id", Kind::Id(128)),
            ("tenant_id", Kind::Id(128)),
            ("execution_id", Kind::Id(128)),
            ("receipt_digest", Kind::Id(64)),
            ("evidence_json", Kind::Payload),
            ("evidence_digest", Kind::Id(64)),
            ("unknown_outcome", Kind::Boolean),
        ],
        primary: &["installation_id", "tenant_id", "execution_id"],
        index: None,
    },
];
fn incompatible() -> DbErr {
    DbErr::Custom("migration037 incompatible owned OAuth storage".into())
}
fn definition(spec: &Spec, backend: DatabaseBackend) -> TableCreateStatement {
    let mut table = Table::create();
    table.table(Alias::new(spec.table));
    if backend == DatabaseBackend::MySql {
        table.engine("InnoDB");
    }
    for (name, kind) in spec.columns {
        table.col(match kind {
            Kind::Id(width) => identity(backend, name, *width, false),
            Kind::Payload => payload(backend, name),
            Kind::Integer => ColumnDef::new(Alias::new(*name))
                .big_integer()
                .not_null()
                .to_owned(),
            Kind::Boolean => ColumnDef::new(Alias::new(*name))
                .boolean()
                .not_null()
                .to_owned(),
        });
    }
    let mut primary = Index::create();
    for name in spec.primary {
        primary.col(Alias::new(*name));
    }
    table.primary_key(&mut primary).to_owned()
}
fn index(spec: &Spec) -> Option<IndexCreateStatement> {
    let (name, columns) = spec.index?;
    let mut index = Index::create();
    index.name(name).table(Alias::new(spec.table));
    for column in columns {
        index.col(Alias::new(*column));
    }
    Some(index.to_owned())
}
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Validate every pre-existing definition before the first CREATE.
        for spec in SPECS {
            catalog::names(manager, spec).await?;
            // Additive CREATE needs collision/definition validation, not the
            // destructive down path's refusal of unresolved external SQL.
            if manager.has_table(spec.table).await? {
                catalog::validate(manager, spec, false).await?;
            }
        }
        for spec in SPECS {
            if !manager.has_table(spec.table).await? {
                manager
                    .create_table(definition(spec, manager.get_database_backend()))
                    .await?;
            }
            if let Some((name, _)) = spec.index
                && !manager.has_index(spec.table, name).await?
                && let Some(index) = index(spec)
            {
                manager.create_index(index).await?;
            }
        }
        for spec in SPECS {
            catalog::validate(manager, spec, true).await?;
        }
        Ok(())
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for spec in SPECS {
            catalog::names(manager, spec).await?;
            if manager.has_table(spec.table).await? {
                catalog::validate(manager, spec, false).await?;
                catalog::dependencies(manager, spec).await?;
                if manager
                    .get_connection()
                    .query_one(Statement::from_string(
                        manager.get_database_backend(),
                        format!("SELECT 1 FROM {} LIMIT 1", spec.table),
                    ))
                    .await?
                    .is_some()
                {
                    return Err(incompatible());
                }
            }
        }
        for spec in SPECS.iter().rev() {
            if manager.has_table(spec.table).await? {
                manager
                    .drop_table(Table::drop().table(Alias::new(spec.table)).to_owned())
                    .await?;
            }
        }
        Ok(())
    }
}
