use super::*;

pub(super) async fn incompatible_columns(
    db: &DatabaseConnection,
    historical: &fixture::Historical,
) -> Result<()> {
    let manager = SchemaManager::new(db);
    let backend = manager.get_database_backend();
    let mut wrong_collation = ColumnDef::new(Alias::new("creator_principal_id"));
    match backend {
        DatabaseBackend::Sqlite => {
            wrong_collation.string_len(128).extra("COLLATE NOCASE");
        }
        DatabaseBackend::Postgres => {
            wrong_collation.custom(Alias::new("VARCHAR(128) COLLATE \"default\""));
        }
        DatabaseBackend::MySql => {
            wrong_collation.custom(Alias::new(
                "VARCHAR(128) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin",
            ));
        }
    }
    for column in [
        ColumnDef::new(Alias::new("creator_principal_id"))
            .integer()
            .to_owned(),
        identity(backend, "creator_principal_id", 127, true),
        identity(backend, "creator_principal_id", 128, false)
            .default("unassigned")
            .to_owned(),
        identity(backend, "creator_principal_id", 128, true)
            .default("unassigned")
            .to_owned(),
        identity(backend, "creator_principal_id", 128, true)
            .generated(Expr::val("derived"), backend != DatabaseBackend::Sqlite)
            .to_owned(),
        identity(backend, "creator_principal_id", 128, true)
            .check(Expr::col(Alias::new("creator_principal_id")).is_null())
            .to_owned(),
        wrong_collation,
    ] {
        // Put the bad definition on the LAST table, proving whole-schema
        // preflight refuses before altering any earlier table.
        fixture::add_column(&manager, "eval_snapshots", column).await?;
        ensure!(
            Migration.up(&manager).await.is_err(),
            "accepted incompatible ownership column"
        );
        ensure!(
            Migration.down(&manager).await.is_err(),
            "down accepted incompatible ownership column"
        );
        for table in ["eval_subjects", "eval_results", "eval_admission_events"] {
            for name in COLUMNS {
                ensure!(
                    !manager.has_column(table, name).await?,
                    "failed preflight changed another table"
                );
            }
        }
        ensure!(has_column(&manager, "eval_snapshots", "creator_principal_id").await?);
        fixture::drop_column(&manager, "eval_snapshots", "creator_principal_id").await?;
        fixture::assert_absent(&manager).await?;
        fixture::assert_historical(db, historical).await?;
    }
    Ok(())
}

pub(super) async fn mysql_oversize_parent(
    db: &DatabaseConnection,
    historical: &fixture::Historical,
) -> Result<()> {
    if db.get_database_backend() != DatabaseBackend::MySql {
        return Ok(());
    }
    let manager = SchemaManager::new(db);
    // The old index fits 513*4 + 255*4 = 3072 bytes even including its primary-key
    // suffix. The new explicit key alone needs 513*4 + 128*4*2 = 3076 bytes.
    // Pin this fixture's charset so the check also works with a non-UTF8 default.
    // Refusal must precede even the first metadata column on MySQL.
    manager
        .alter_table(
            Table::alter()
                .table(Alias::new("eval_results"))
                .modify_column(
                    ColumnDef::new(Alias::new("eval_id"))
                        .custom(Alias::new("VARCHAR(513) CHARACTER SET utf8mb4"))
                        .not_null(),
                )
                .to_owned(),
        )
        .await?;
    let refused = Migration.up(&manager).await;
    fixture::assert_absent(&manager).await?;
    manager
        .alter_table(
            Table::alter()
                .table(Alias::new("eval_results"))
                .modify_column(
                    ColumnDef::new(Alias::new("eval_id"))
                        .string_len(255)
                        .not_null(),
                )
                .to_owned(),
        )
        .await?;
    ensure!(refused.is_err(), "oversize MySQL scoped index was accepted");
    fixture::assert_historical(db, historical).await
}

pub(super) async fn unexpected_unique_constraint(
    db: &DatabaseConnection,
    historical: &fixture::Historical,
) -> Result<()> {
    let manager = SchemaManager::new(db);
    fixture::add_column(
        &manager,
        "eval_snapshots",
        identity(manager.get_database_backend(), "tenant_id", 128, true),
    )
    .await?;
    manager
        .create_index(
            Index::create()
                .name("migration030_fixture_unique")
                .table(Alias::new("eval_snapshots"))
                .col(Alias::new("tenant_id"))
                .unique()
                .to_owned(),
        )
        .await?;
    ensure!(
        Migration.up(&manager).await.is_err(),
        "UNIQUE changed identity admissibility"
    );
    ensure!(Migration.down(&manager).await.is_err());
    ensure!(
        !manager
            .has_column("eval_subjects", "installation_id")
            .await?
    );
    manager
        .drop_index(
            Index::drop()
                .name("migration030_fixture_unique")
                .table(Alias::new("eval_snapshots"))
                .to_owned(),
        )
        .await?;
    fixture::drop_column(&manager, "eval_snapshots", "tenant_id").await?;
    fixture::assert_absent(&manager).await?;
    fixture::assert_historical(db, historical).await
}

pub(super) async fn partial_up(
    db: &DatabaseConnection,
    historical: &fixture::Historical,
) -> Result<()> {
    let manager = SchemaManager::new(db);
    for column in COLUMNS {
        fixture::add_column(
            &manager,
            "eval_subjects",
            identity(manager.get_database_backend(), column, 128, true),
        )
        .await?;
    }
    manager.create_index(INDEXES[0].create()).await?;
    fixture::add_column(
        &manager,
        "eval_results",
        identity(manager.get_database_backend(), "installation_id", 128, true),
    )
    .await?;
    // Simulate a completed DDL statement separated from the migration ledger.
    Migration.up(&manager).await?;
    fixture::assert_complete(&manager).await?;
    fixture::assert_all_null(db).await?;
    fixture::assert_historical(db, historical).await?;
    Migration.up(&manager).await?;
    fixture::assert_complete(&manager).await?;
    fixture::assert_historical(db, historical).await
}

pub(super) async fn index_name_collision(
    db: &DatabaseConnection,
    historical: &fixture::Historical,
) -> Result<()> {
    if db.get_database_backend() == DatabaseBackend::MySql {
        return Ok(());
    }
    let manager = SchemaManager::new(db);
    manager
        .create_index(
            Index::create()
                .name(INDEXES[0].name)
                .table(Alias::new("eval_snapshots"))
                .col(Alias::new("evidence_root"))
                .to_owned(),
        )
        .await?;
    ensure!(
        Migration.up(&manager).await.is_err(),
        "index on another table hid a schema-name collision"
    );
    for table in TABLES {
        for column in COLUMNS {
            ensure!(!manager.has_column(table, column).await?);
        }
    }
    manager
        .drop_index(
            Index::drop()
                .name(INDEXES[0].name)
                .table(Alias::new("eval_snapshots"))
                .to_owned(),
        )
        .await?;
    fixture::assert_historical(db, historical).await
}

pub(super) async fn incompatible_indexes(
    db: &DatabaseConnection,
    historical: &fixture::Historical,
) -> Result<()> {
    let manager = SchemaManager::new(db);
    let bad = &INDEXES[3];
    for unique in [false, true] {
        manager.drop_index(bad.drop()).await?;
        manager.drop_index(INDEXES[0].drop()).await?;
        let mut statement = Index::create();
        statement.name(bad.name).table(Alias::new(bad.table));
        if unique {
            statement.unique();
            for column in bad.columns {
                statement.col(Alias::new(*column));
            }
        } else {
            for column in bad.columns.iter().rev() {
                statement.col(Alias::new(*column));
            }
        }
        manager.create_index(statement.to_owned()).await?;
        ensure!(
            Migration.up(&manager).await.is_err(),
            "same name hid an incompatible index"
        );
        ensure!(
            !manager.has_index(INDEXES[0].table, INDEXES[0].name).await?,
            "preflight repaired an earlier index before failing"
        );
        ensure!(Migration.down(&manager).await.is_err());
        for column in COLUMNS {
            ensure!(manager.has_column("eval_snapshots", column).await?);
        }
        manager.drop_index(bad.drop()).await?;
        Migration.up(&manager).await?;
        fixture::assert_complete(&manager).await?;
        fixture::assert_historical(db, historical).await?;
    }
    Ok(())
}

pub(super) async fn exact_comparison(db: &DatabaseConnection) -> Result<()> {
    for table in TABLES {
        for (index, column) in COLUMNS.into_iter().enumerate() {
            for (stored, other) in [
                ("Tenant", "tenant"),
                ("tenant", "tenant "),
                ("é", "e\u{301}"),
            ] {
                let mut metadata = [None; 3];
                metadata[index] = Some(stored);
                fixture::set_metadata(db, table, metadata).await?;
                ensure!(fixture::metadata_count(db, table, column, stored).await? == 2);
                ensure!(
                    fixture::metadata_count(db, table, column, other).await? == 0,
                    "ownership comparison folded case/space/Unicode: {table}.{column}"
                );
            }
        }
        let longest = "x".repeat(128);
        fixture::set_metadata(db, table, [Some(&longest), Some(&longest), Some(&longest)]).await?;
        for column in COLUMNS {
            ensure!(fixture::metadata_count(db, table, column, &longest).await? == 2);
        }
        fixture::set_metadata(db, table, [None, None, None]).await?;
    }
    fixture::assert_all_null(db).await
}

pub(super) async fn down_refuses_every_ownership_field(
    db: &DatabaseConnection,
    historical: &fixture::Historical,
) -> Result<()> {
    let manager = SchemaManager::new(db);
    for table in TABLES {
        // Every nonempty NULL/present combination, including creator-only rows,
        // must block down; the last table verifies no earlier destructive DDL.
        for mask in 1_u8..8 {
            let metadata =
                std::array::from_fn(|index| (mask & (1 << index) != 0).then_some("retained-owner"));
            fixture::set_metadata(db, table, metadata).await?;
            ensure!(
                Migration.down(&manager).await.is_err(),
                "down erased {table} mask {mask}"
            );
            fixture::assert_complete(&manager).await?;
            Migration.up(&manager).await?;
            for (column, expected) in COLUMNS.into_iter().zip(metadata) {
                let count = fixture::metadata_count(db, table, column, "retained-owner").await?;
                ensure!(
                    count == if expected.is_some() { 2 } else { 0 },
                    "resume rewrote mixed metadata"
                );
            }
            fixture::assert_historical(db, historical).await?;
            fixture::set_metadata(db, table, [None, None, None]).await?;
        }
    }
    Ok(())
}

pub(super) async fn sqlite_query_plans(db: &DatabaseConnection) -> Result<()> {
    if db.get_database_backend() != DatabaseBackend::Sqlite {
        return Ok(());
    }
    for (index, sql) in [
        (
            &INDEXES[0],
            "SELECT eval_id FROM eval_subjects WHERE installation_id='i' AND tenant_id='t' ORDER BY eval_id",
        ),
        (
            &INDEXES[1],
            "SELECT result_id FROM eval_results WHERE installation_id='i' AND tenant_id='t' AND eval_id='e'",
        ),
        (
            &INDEXES[2],
            "SELECT sequence FROM eval_admission_events WHERE installation_id='i' AND tenant_id='t' AND result_id='r' ORDER BY sequence DESC LIMIT 1",
        ),
        (
            &INDEXES[3],
            "SELECT sequence FROM eval_admission_events WHERE installation_id='i' AND tenant_id='t' ORDER BY sequence LIMIT 16",
        ),
    ] {
        let rows = db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!("EXPLAIN QUERY PLAN {sql}"),
            ))
            .await?;
        let details = rows
            .iter()
            .map(|row| row.try_get::<String>("", "detail"))
            .collect::<std::result::Result<Vec<_>, _>>()?
            .join("\n");
        ensure!(
            details.contains(index.name),
            "scoped query missed {}: {details}",
            index.name
        );
        ensure!(
            !details.contains("USE TEMP B-TREE"),
            "scoped scan required an unbounded sort"
        );
    }
    Ok(())
}
