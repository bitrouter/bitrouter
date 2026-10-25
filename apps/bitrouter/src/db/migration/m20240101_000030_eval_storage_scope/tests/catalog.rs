use super::*;

async fn execute(db: &DatabaseConnection, sql: &str) -> Result<()> {
    db.execute(Statement::from_string(
        db.get_database_backend(),
        sql.to_owned(),
    ))
    .await?;
    Ok(())
}

// Every object the migration can add/drop is compared after each refusal.
// The incompatible object is on a late table/index, with earlier objects absent,
// so a failed migration that first tries to repair the schema cannot pass.
async fn layout(manager: &SchemaManager<'_>) -> Result<Vec<bool>> {
    let mut present = Vec::new();
    for table in TABLES {
        for column in COLUMNS {
            present.push(has_column(manager, table, column).await?);
        }
    }
    for index in &INDEXES {
        present.push(manager.has_index(index.table, index.name).await?);
    }
    Ok(present)
}

async fn assert_refused_without_ddl(
    db: &DatabaseConnection,
    historical: &fixture::Historical,
) -> Result<()> {
    let manager = SchemaManager::new(db);
    let before = layout(&manager).await?;
    ensure!(
        Migration.up(&manager).await.is_err(),
        "up accepted incompatible catalog metadata"
    );
    ensure!(
        layout(&manager).await? == before,
        "up performed DDL before refusing"
    );
    fixture::assert_historical(db, historical).await?;
    ensure!(
        Migration.down(&manager).await.is_err(),
        "down accepted incompatible catalog metadata"
    );
    ensure!(
        layout(&manager).await? == before,
        "down performed DDL before refusing"
    );
    fixture::assert_historical(db, historical).await?;
    fixture::assert_legacy_indexes(&manager).await
}

async fn drop_index(manager: &SchemaManager<'_>, table: &str, name: &str) -> Result<()> {
    manager
        .drop_index(Index::drop().name(name).table(Alias::new(table)).to_owned())
        .await?;
    Ok(())
}

#[tokio::test]
async fn sqlite_quoted_foreign_keys_are_resolved_before_any_ddl() -> Result<()> {
    for (foreign_column, dependent) in [
        ("'tenant_id'", true),
        ("'TENANT_ID'", true),
        ("'legacy_ref'", false),
    ] {
        let db = connect("sqlite::memory:").await?;
        let manager = SchemaManager::new(&db);
        crate::db::migration::m20240101_000010_create_eval_exchange::Migration
            .up(&manager)
            .await?;
        crate::db::migration::m20240101_000011_scope_eval_exchange::Migration
            .up(&manager)
            .await?;
        // Rebuild the empty fixture table before seeding historical bytes.
        // This late table also has a valid creator column: down must not drop
        // that column before detecting the incompatible tenant foreign key.
        let rows = query(
            &manager,
            "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = 'eval_snapshots'",
            vec![],
        )
        .await?;
        let original = rows
            .first()
            .context("snapshot fixture schema")?
            .try_get::<String>("", "sql")?;
        let columns = original
            .trim_end()
            .strip_suffix(')')
            .context("snapshot fixture table declaration")?;
        execute(&db, "DROP TABLE eval_snapshots").await?;
        execute(
            &db,
            &format!(
                "{columns}, tenant_id varchar(128) COLLATE BINARY, \
             creator_principal_id varchar(128) COLLATE BINARY, legacy_ref TEXT, \
             FOREIGN KEY ({foreign_column}) REFERENCES eval_subjects(eval_id))"
            ),
        )
        .await?;
        execute(
            &db,
            "CREATE INDEX idx_eval_snapshots_owner ON eval_snapshots (owner_user_id)",
        )
        .await?;
        fixture::seed(&db).await?;
        let historical = fixture::historical(&db).await?;
        let keys = query(
            &manager,
            "SELECT \"from\" AS source_column FROM pragma_foreign_key_list('eval_snapshots')",
            vec![],
        )
        .await?;
        ensure!(keys.len() == 1);
        if dependent {
            assert_refused_without_ddl(&db, &historical).await?;
        } else {
            Migration.up(&manager).await?;
            fixture::assert_complete(&manager).await?;
            fixture::assert_historical(&db, &historical).await?;
            Migration.down(&manager).await?;
            fixture::assert_absent(&manager).await?;
            fixture::assert_historical(&db, &historical).await?;
            fixture::assert_legacy_indexes(&manager).await?;
        }
        ensure!(
            query(
                &manager,
                "SELECT \"from\" AS source_column FROM pragma_foreign_key_list('eval_snapshots')",
                vec![]
            )
            .await?
            .first()
            .context("foreign key survived migration")?
            .try_get::<String>("", "source_column")?
            .eq_ignore_ascii_case(if dependent { "tenant_id" } else { "legacy_ref" })
        );
        db.close().await?;
    }
    Ok(())
}

pub(super) async fn unique_expression_dependencies(
    db: &DatabaseConnection,
    historical: &fixture::Historical,
) -> Result<()> {
    let manager = SchemaManager::new(db);
    // The index name matches a metadata identifier, and a literal inside its
    // expression matches another. Neither creates an ownership dependency.
    execute(db,
        "CREATE UNIQUE INDEX creator_principal_id ON eval_snapshots \
         ((CASE WHEN evidence_root = 'tenant_id' THEN evidence_root ELSE lower(evidence_root) END))"
    ).await?;
    execute(
        db,
        "CREATE INDEX migration030_fixture_unrelated ON eval_snapshots (owner_user_id)",
    )
    .await?;
    fixture::add_column(
        &manager,
        "eval_snapshots",
        identity(manager.get_database_backend(), "tenant_id", 128, true),
    )
    .await?;

    // All existing NULL values satisfy these UNIQUE indexes, but later tenant
    // writes would gain an unsupported uniqueness constraint. MySQL functional
    // parts and SQLite expression keys have no ordinary column-name entry.
    let expression = if db.get_database_backend() == DatabaseBackend::MySql {
        "lower(`tenant_id`)"
    } else {
        "lower(\"tenant_id\")"
    };
    let mut indexes = vec![format!(
        "CREATE UNIQUE INDEX migration030_fixture_dependency ON eval_snapshots (({expression}))"
    )];
    if db.get_database_backend() != DatabaseBackend::MySql {
        indexes.push(
            "CREATE UNIQUE INDEX migration030_fixture_dependency ON eval_snapshots \
             (evidence_root) WHERE tenant_id IS NOT NULL"
                .to_owned(),
        );
    }
    for sql in indexes {
        execute(db, &sql).await?;
        assert_refused_without_ddl(db, historical).await?;
        for index in [
            "migration030_fixture_dependency",
            "creator_principal_id",
            "migration030_fixture_unrelated",
        ] {
            ensure!(
                manager.has_index("eval_snapshots", index).await?,
                "refusal removed {index}"
            );
        }
        drop_index(
            &manager,
            "eval_snapshots",
            "migration030_fixture_dependency",
        )
        .await?;
    }

    // The same partially applied schema must now migrate successfully while
    // retaining unrelated expression and ordinary indexes in both directions.
    Migration.up(&manager).await?;
    fixture::assert_complete(&manager).await?;
    fixture::assert_all_null(db).await?;
    fixture::assert_historical(db, historical).await?;
    Migration.down(&manager).await?;
    fixture::assert_absent(&manager).await?;
    fixture::assert_historical(db, historical).await?;
    fixture::assert_legacy_indexes(&manager).await?;
    for index in ["creator_principal_id", "migration030_fixture_unrelated"] {
        ensure!(
            manager.has_index("eval_snapshots", index).await?,
            "migration removed unrelated {index}"
        );
        drop_index(&manager, "eval_snapshots", index).await?;
    }
    mysql_escaped_catalog_dependencies(db, historical).await?;
    Ok(())
}

async fn mysql_escaped_catalog_dependencies(
    db: &DatabaseConnection,
    historical: &fixture::Historical,
) -> Result<()> {
    if db.get_database_backend() != DatabaseBackend::MySql {
        return Ok(());
    }
    let manager = SchemaManager::new(db);
    // Keep earlier tables absent, but include creator_principal_id on this late
    // table. A downgrade must refuse before dropping that otherwise valid column.
    for column in COLUMNS {
        fixture::add_column(
            &manager,
            "eval_snapshots",
            identity(DatabaseBackend::MySql, column, 128, true),
        )
        .await?;
    }
    // Both literals contain apostrophes. MySQL stores escaped delimiters AND
    // escaped literal contents in its catalog; the middle identifier must not
    // disappear into a literal. These unrelated objects must survive up/down,
    // even though their suffix literal contains the metadata name tenant_id.
    execute(
        db,
        "CREATE UNIQUE INDEX migration030_fixture_escaped_unrelated ON eval_snapshots \
         ((CONCAT('a''', evidence_root, '''tenant_id')))",
    )
    .await?;
    execute(
        db,
        "ALTER TABLE eval_snapshots ADD CONSTRAINT migration030_fixture_escaped_unrelated_check \
         CHECK (CONCAT('a''', evidence_root, '''tenant_id') <> 'forbidden')",
    )
    .await?;

    execute(
        db,
        "CREATE UNIQUE INDEX migration030_fixture_escaped_dependency ON eval_snapshots \
         ((CONCAT('a''', tenant_id, '''b')))",
    )
    .await?;
    assert_refused_without_ddl(db, historical).await?;
    ensure!(
        manager
            .has_index("eval_snapshots", "migration030_fixture_escaped_dependency")
            .await?,
        "refusal removed the dependent expression index"
    );
    drop_index(
        &manager,
        "eval_snapshots",
        "migration030_fixture_escaped_dependency",
    )
    .await?;

    execute(
        db,
        "ALTER TABLE eval_snapshots ADD CONSTRAINT migration030_fixture_escaped_dependency_check \
         CHECK (CONCAT('a''', tenant_id, '''b') <> 'forbidden')",
    )
    .await?;
    assert_refused_without_ddl(db, historical).await?;
    ensure!(
        mysql_has_check(&manager, "migration030_fixture_escaped_dependency_check").await?,
        "refusal removed the dependent check"
    );
    execute(
        db,
        "ALTER TABLE eval_snapshots DROP CHECK migration030_fixture_escaped_dependency_check",
    )
    .await?;

    Migration.up(&manager).await?;
    fixture::assert_complete(&manager).await?;
    fixture::assert_all_null(db).await?;
    fixture::assert_historical(db, historical).await?;
    Migration.down(&manager).await?;
    fixture::assert_absent(&manager).await?;
    fixture::assert_historical(db, historical).await?;
    fixture::assert_legacy_indexes(&manager).await?;
    ensure!(
        manager
            .has_index("eval_snapshots", "migration030_fixture_escaped_unrelated")
            .await?,
        "migration removed the unrelated escaped expression index"
    );
    ensure!(
        mysql_has_check(&manager, "migration030_fixture_escaped_unrelated_check").await?,
        "migration removed the unrelated escaped check"
    );
    drop_index(
        &manager,
        "eval_snapshots",
        "migration030_fixture_escaped_unrelated",
    )
    .await?;
    execute(
        db,
        "ALTER TABLE eval_snapshots DROP CHECK migration030_fixture_escaped_unrelated_check",
    )
    .await
}

async fn mysql_has_check(manager: &SchemaManager<'_>, name: &str) -> Result<bool> {
    Ok(query(
        manager,
        "SELECT CONSTRAINT_NAME FROM information_schema.table_constraints \
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'eval_snapshots' \
         AND CONSTRAINT_TYPE = 'CHECK' AND CONSTRAINT_NAME = ?",
        vec![name.into()],
    )
    .await?
    .len()
        == 1)
}

#[test]
fn mysql_catalog_escapes_preserve_dependencies_and_hide_literal_contents() -> Result<()> {
    // Captured from actual MySQL 8.4 INFORMATION_SCHEMA, not the submitted DDL.
    let expression = r"concat(_utf8mb4\'a\\\'\',`tenant_id`,_utf8mb4\'\\\'b\')";
    let check = r"(concat(_latin1\'a\\\'\',`tenant_id`,_latin1\'\\\'b\') <> _latin1\'forbidden\')";
    let tenant = Token::Word("tenant_id".into());
    for catalog in [expression, check] {
        ensure!(mysql_catalog_tokens(catalog)?.contains(&tenant));
    }
    let unrelated = r"concat(_utf8mb4\'a\\\'\',`evidence_root`,_utf8mb4\'\\\'tenant_id\')";
    let tokens = mysql_catalog_tokens(unrelated)?;
    ensure!(tokens.contains(&Token::Word("evidence_root".into())));
    ensure!(!tokens.contains(&tenant));
    // SQLite does not give a backslash before a quote MySQL's escape meaning.
    ensure!(sqlite_tokens(r"concat('a\', tenant_id, '\b')")?.contains(&tenant));
    Ok(())
}

#[test]
fn mysql_catalog_parser_rejects_incomplete_or_unsupported_escapes() -> Result<()> {
    for catalog in [r"\q", "\\", r"concat(\'unfinished)", r"\'a\\"] {
        ensure!(mysql_catalog_tokens(catalog).is_err());
    }
    ensure!(mysql_catalog_tokens(&"x".repeat(65_537)).is_err());
    Ok(())
}

pub(super) async fn postgres_referencing_foreign_key(
    db: &DatabaseConnection,
    historical: &fixture::Historical,
) -> Result<()> {
    if db.get_database_backend() != DatabaseBackend::Postgres {
        return Ok(());
    }
    let manager = SchemaManager::new(db);
    fixture::add_column(
        &manager,
        "eval_snapshots",
        identity(DatabaseBackend::Postgres, "tenant_id", 128, true),
    )
    .await?;
    // Both tables are owned by this fixture. Existing NULLs satisfy the FK,
    // which is invisible on this side in constraint_column_usage.
    execute(
        db,
        "ALTER TABLE eval_snapshots ADD CONSTRAINT migration030_fixture_fk \
         FOREIGN KEY (tenant_id) REFERENCES eval_subjects (eval_id)",
    )
    .await?;
    assert_refused_without_ddl(db, historical).await?;
    ensure!(
        query(
            &manager,
            "SELECT conname FROM pg_constraint WHERE conrelid = 'eval_snapshots'::regclass \
             AND conname = 'migration030_fixture_fk'",
            vec![]
        )
        .await?
        .len()
            == 1,
        "refusal removed the foreign key"
    );
    execute(
        db,
        "ALTER TABLE eval_snapshots DROP CONSTRAINT migration030_fixture_fk",
    )
    .await?;
    fixture::drop_column(&manager, "eval_snapshots", "tenant_id").await?;
    fixture::assert_absent(&manager).await?;
    fixture::assert_historical(db, historical).await
}

pub(super) async fn postgres_index_options(
    db: &DatabaseConnection,
    historical: &fixture::Historical,
) -> Result<()> {
    if db.get_database_backend() != DatabaseBackend::Postgres {
        return Ok(());
    }
    let manager = SchemaManager::new(db);
    let index = &INDEXES[3];
    for column in COLUMNS {
        fixture::add_column(
            &manager,
            index.table,
            identity(DatabaseBackend::Postgres, column, 128, true),
        )
        .await?;
    }
    for keys in [
        "installation_id COLLATE \"default\", tenant_id, sequence",
        "installation_id, tenant_id COLLATE \"default\", sequence",
        "installation_id text_pattern_ops, tenant_id, sequence",
        "installation_id, tenant_id, sequence DESC",
        "installation_id, tenant_id, sequence NULLS FIRST",
    ] {
        execute(
            db,
            &format!("CREATE INDEX {} ON {} ({keys})", index.name, index.table),
        )
        .await?;
        assert_refused_without_ddl(db, historical).await?;
        manager.drop_index(index.drop()).await?;
    }
    // Establish that the exact same key columns with their normal options work.
    manager.create_index(index.create()).await?;
    Migration.up(&manager).await?;
    fixture::assert_complete(&manager).await?;
    fixture::assert_all_null(db).await?;
    fixture::assert_historical(db, historical).await?;
    Migration.down(&manager).await?;
    fixture::assert_absent(&manager).await?;
    fixture::assert_legacy_indexes(&manager).await?;
    fixture::assert_historical(db, historical).await
}
