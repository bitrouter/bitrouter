//! Add local resource-tenancy authority without assigning any legacy owner.

use sea_orm::DatabaseBackend;
use sea_orm_migration::prelude::*;

const TABLES: [&str; 6] = [
    "tenancy_installation",
    "tenancy_tenants",
    "tenancy_principals",
    "tenancy_bindings",
    "tenancy_credentials",
    "tenancy_operations",
];

pub(super) fn identity(
    backend: DatabaseBackend,
    name: &str,
    length: u32,
    nullable: bool,
) -> ColumnDef {
    let mut column = ColumnDef::new(Alias::new(name));
    // IDs and issuer/subject pairs are byte-exact, including case and trailing
    // spaces. MySQL's NO PAD binary Unicode collation avoids both default
    // case-folding and the older *_bin collations' PAD SPACE equality.
    // https://dev.mysql.com/doc/refman/8.4/en/charset-binary-collations.html
    match backend {
        DatabaseBackend::MySql => {
            column.custom(Alias::new(format!(
                "VARCHAR({length}) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin"
            )));
        }
        DatabaseBackend::Postgres => {
            column.custom(Alias::new(format!("VARCHAR({length}) COLLATE \"C\"")));
        }
        DatabaseBackend::Sqlite => {
            column.string_len(length).extra("COLLATE BINARY");
        }
    }
    if !nullable {
        column.not_null();
    }
    column
}

pub(super) fn payload(backend: DatabaseBackend, name: &str) -> ColumnDef {
    let mut column = ColumnDef::new(Alias::new(name));
    // Valid 256-key scopes exceed MySQL TEXT's 64 KiB ceiling. Application
    // validation supplies the tighter semantic/byte bounds on these payloads.
    // https://dev.mysql.com/doc/refman/8.4/en/blob.html
    if backend == DatabaseBackend::MySql {
        column.custom(Alias::new(
            "LONGTEXT CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin",
        ));
    } else {
        column.text();
    }
    column.not_null().to_owned()
}

fn table(name: &str) -> TableCreateStatement {
    Table::create()
        .table(Alias::new(name))
        .if_not_exists()
        .to_owned()
}

fn primary(columns: &[&str]) -> IndexCreateStatement {
    let mut key = Index::create();
    for column in columns {
        key.col(Alias::new(*column));
    }
    key.to_owned()
}

pub(super) fn statements(backend: DatabaseBackend) -> Vec<TableCreateStatement> {
    let id = |name| identity(backend, name, 128, false);
    let optional_id = |name| identity(backend, name, 128, true);
    let text = |name| payload(backend, name);
    vec![
        table(TABLES[0])
            .col(
                ColumnDef::new(Alias::new("singleton"))
                    .integer()
                    .not_null()
                    .primary_key(),
            )
            .col(id("installation_id").unique_key())
            .col(
                ColumnDef::new(Alias::new("schema_version"))
                    .integer()
                    .not_null(),
            )
            .col(
                ColumnDef::new(Alias::new("authority_revision"))
                    .big_integer()
                    .not_null(),
            )
            .col(
                ColumnDef::new(Alias::new("operation_ids"))
                    .integer()
                    .not_null(),
            )
            .col(
                ColumnDef::new(Alias::new("safety_operation_ids"))
                    .integer()
                    .not_null(),
            )
            .col(ColumnDef::new(Alias::new("saturated")).boolean().not_null())
            .col(text("created_at"))
            .to_owned(),
        table(TABLES[1])
            .col(id("id").primary_key())
            .col(id("revision"))
            .col(id("state"))
            .col(text("created_at"))
            .to_owned(),
        table(TABLES[2])
            .col(id("id").primary_key())
            .col(id("revision"))
            .col(identity(backend, "issuer", 256, false))
            .col(identity(backend, "subject", 256, false))
            .col(text("created_at"))
            .index(
                Index::create()
                    .name("uq_tenancy_principal_subject")
                    .unique()
                    .col(Alias::new("issuer"))
                    .col(Alias::new("subject")),
            )
            .to_owned(),
        table(TABLES[3])
            .col(id("tenant_id"))
            .col(id("id"))
            .col(id("principal_id"))
            .col(id("revision"))
            .col(id("role"))
            .col(text("scope_json"))
            .col(text("expires_at"))
            .col(optional_id("management_owner"))
            .col(identity(backend, "management_class", 32, true))
            .primary_key(&mut primary(&["tenant_id", "id"]))
            .foreign_key(
                ForeignKey::create()
                    .name("fk_tenancy_binding_tenant")
                    .from(Alias::new(TABLES[3]), Alias::new("tenant_id"))
                    .to(Alias::new(TABLES[1]), Alias::new("id")),
            )
            .foreign_key(
                ForeignKey::create()
                    .name("fk_tenancy_binding_principal")
                    .from(Alias::new(TABLES[3]), Alias::new("principal_id"))
                    .to(Alias::new(TABLES[2]), Alias::new("id")),
            )
            .to_owned(),
        table(TABLES[4])
            .col(id("id").primary_key())
            .col(id("tenant_id"))
            .col(id("principal_id"))
            .col(optional_id("binding_id"))
            .col(id("revision"))
            .col(id("key_hash").unique_key())
            .col(id("state"))
            .col(
                ColumnDef::new(Alias::new("audit_reader"))
                    .boolean()
                    .not_null(),
            )
            .col(text("permissions_json"))
            .col(text("scope_json"))
            .col(text("expires_at"))
            .col(text("created_at"))
            .foreign_key(
                ForeignKey::create()
                    .name("fk_tenancy_credential_tenant")
                    .from(Alias::new(TABLES[4]), Alias::new("tenant_id"))
                    .to(Alias::new(TABLES[1]), Alias::new("id")),
            )
            .foreign_key(
                ForeignKey::create()
                    .name("fk_tenancy_credential_principal")
                    .from(Alias::new(TABLES[4]), Alias::new("principal_id"))
                    .to(Alias::new(TABLES[2]), Alias::new("id")),
            )
            .foreign_key(
                ForeignKey::create()
                    .name("fk_tenancy_credential_binding")
                    .from(
                        Alias::new(TABLES[4]),
                        (Alias::new("tenant_id"), Alias::new("binding_id")),
                    )
                    .to(
                        Alias::new(TABLES[3]),
                        (Alias::new("tenant_id"), Alias::new("id")),
                    ),
            )
            .to_owned(),
        table(TABLES[5])
            .col(id("installation_id"))
            .col(id("principal_id"))
            .col(id("operation_id"))
            .col(optional_id("tenant_id"))
            .col(id("authority_class"))
            .col(text("read_context_json"))
            .col(
                ColumnDef::new(Alias::new("hash_version"))
                    .integer()
                    .not_null(),
            )
            .col(id("command_digest"))
            .col(text("result_json"))
            .col(text("response_json"))
            .col(text("created_at"))
            .col(ColumnDef::new(Alias::new("safety")).boolean().not_null())
            .primary_key(&mut primary(&[
                "installation_id",
                "principal_id",
                "operation_id",
            ]))
            .to_owned(),
    ]
}

/// DDL-only component of the fresh current baseline; never accepts an old dataset.
pub(super) async fn initialize_current(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    for statement in statements(manager.get_database_backend()) {
        manager.create_table(statement).await?;
    }
    // MySQL DDL commits before the migration ledger is written. An
    // interrupted run must resume without recreating an existing index.
    // https://dev.mysql.com/doc/refman/8.4/en/implicit-commit.html
    if manager.get_database_backend() == DatabaseBackend::MySql
        && manager
            .has_index(TABLES[4], "idx_tenancy_credentials_state")
            .await?
    {
        return Ok(());
    }
    manager
        .create_index(
            Index::create()
                .if_not_exists()
                .name("idx_tenancy_credentials_state")
                .table(Alias::new(TABLES[4]))
                .col(Alias::new("state"))
                .col(Alias::new("tenant_id"))
                .to_owned(),
        )
        .await?;
    Ok(())
}
