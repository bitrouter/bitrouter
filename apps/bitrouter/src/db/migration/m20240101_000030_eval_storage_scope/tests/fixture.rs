use super::*;

pub(super) type Historical = Vec<Vec<Vec<String>>>;

const OLD_COLUMNS: [&[&str]; 4] = [
    &[
        "eval_id",
        "subject_id",
        "scope",
        "policy_digest",
        "holdout",
        "content_digest",
        "subject_json",
        "created_at",
        "owner_user_id",
    ],
    &[
        "result_id",
        "eval_id",
        "idempotency_key",
        "authority_id",
        "evaluator_kind",
        "content_digest",
        "result_json",
        "created_at",
        "owner_user_id",
    ],
    &[
        "sequence",
        "result_id",
        "status",
        "reason",
        "authority_id",
        "created_at",
    ],
    &[
        "evidence_root",
        "manifest_json",
        "result_count",
        "frozen_at",
        "owner_user_id",
    ],
];

pub(super) async fn insert(
    db: &DatabaseConnection,
    table: &str,
    columns: &[&str],
    values: Vec<Value>,
) -> Result<()> {
    let mut statement = Query::insert();
    statement
        .into_table(Alias::new(table))
        .columns(columns.iter().map(|name| Alias::new(*name)));
    statement.values(values.into_iter().map(Into::into))?;
    db.execute(db.get_database_backend().build(&statement))
        .await?;
    Ok(())
}

pub(super) async fn seed(db: &DatabaseConnection) -> Result<()> {
    for sequence in [1_i64, 2] {
        let eval = format!("legacy-eval-{sequence}");
        let result = format!("legacy-result-{sequence}");
        let owner = format!("Legacy owner / e\u{301} {sequence} ");
        let payload = format!("{{\n  \"z\":\"É / e\\u0301\", \"a\": {sequence}\n}}  ");
        insert(
            db,
            TABLES[0],
            OLD_COLUMNS[0],
            vec![
                eval.clone().into(),
                format!("native/{sequence}").into(),
                "task".into(),
                "PolicyDigest CASE-sensitive ".into(),
                (sequence == 1).into(),
                format!("digest:{sequence}:unaltered ").into(),
                payload.clone().into(),
                "2026-01-01T00:00:00+00:00".into(),
                owner.clone().into(),
            ],
        )
        .await?;
        insert(
            db,
            TABLES[1],
            OLD_COLUMNS[1],
            vec![
                result.clone().into(),
                eval.into(),
                format!("retry/{sequence}").into(),
                "legacy-evaluator".into(),
                "generic".into(),
                format!("result-hash-{sequence}").into(),
                payload.clone().into(),
                "2026-01-01T00:00:01Z".into(),
                owner.clone().into(),
            ],
        )
        .await?;
        insert(
            db,
            TABLES[2],
            OLD_COLUMNS[2],
            vec![
                sequence.into(),
                result.into(),
                "admitted".into(),
                "historical reason ".into(),
                "legacy-evaluator".into(),
                "2026-01-01T00:00:02Z".into(),
            ],
        )
        .await?;
        insert(
            db,
            TABLES[3],
            OLD_COLUMNS[3],
            vec![
                format!("legacy-root-{sequence}").into(),
                payload.into(),
                1_i32.into(),
                "2026-01-01T00:00:03Z".into(),
                owner.into(),
            ],
        )
        .await?;
    }
    Ok(())
}

pub(super) async fn historical(db: &DatabaseConnection) -> Result<Historical> {
    let mut tables = Vec::new();
    for (table, columns) in TABLES.into_iter().zip(OLD_COLUMNS) {
        let first = columns.first().context("fixture primary key absent")?;
        let statement = Query::select()
            .columns(columns.iter().map(|name| Alias::new(*name)))
            .from(Alias::new(table))
            .order_by(Alias::new(*first), Order::Asc)
            .to_owned();
        let rows = db
            .query_all(db.get_database_backend().build(&statement))
            .await?;
        let mut saved = Vec::new();
        for row in rows {
            let mut fields = Vec::new();
            for column in columns {
                fields.push(match *column {
                    "holdout" => row.try_get::<bool>("", column)?.to_string(),
                    "sequence" => row.try_get::<i64>("", column)?.to_string(),
                    "result_count" => row.try_get::<i32>("", column)?.to_string(),
                    _ => row.try_get::<String>("", column)?,
                });
            }
            saved.push(fields);
        }
        tables.push(saved);
    }
    Ok(tables)
}

pub(super) async fn assert_historical(
    db: &DatabaseConnection,
    expected: &Historical,
) -> Result<()> {
    ensure!(
        &historical(db).await? == expected,
        "migration changed historical IDs, owner, JSON, digest or event bytes"
    );
    Ok(())
}

pub(super) async fn assert_complete(manager: &SchemaManager<'_>) -> Result<()> {
    for table in TABLES {
        for column in COLUMNS {
            ensure!(
                manager.has_column(table, column).await?,
                "missing {table}.{column}"
            );
            validate_column(manager, table, column).await?;
        }
    }
    for index in &INDEXES {
        ensure!(
            manager.has_index(index.table, index.name).await?,
            "missing {}",
            index.name
        );
        validate_index(manager, index).await?;
    }
    assert_legacy_indexes(manager).await
}

pub(super) async fn assert_absent(manager: &SchemaManager<'_>) -> Result<()> {
    for table in TABLES {
        ensure!(manager.has_table(table).await?);
        for column in COLUMNS {
            ensure!(
                !manager.has_column(table, column).await?,
                "unexpected {table}.{column}"
            );
        }
    }
    for index in &INDEXES {
        ensure!(!manager.has_index(index.table, index.name).await?);
    }
    Ok(())
}

pub(super) async fn assert_legacy_indexes(manager: &SchemaManager<'_>) -> Result<()> {
    for (table, index) in [
        ("eval_results", "idx_eval_results_eval_id"),
        (
            "eval_admission_events",
            "idx_eval_admission_result_sequence",
        ),
        ("eval_subjects", "idx_eval_subjects_owner"),
        ("eval_results", "idx_eval_results_owner"),
        ("eval_results", "idx_eval_results_owner_idempotency"),
        ("eval_snapshots", "idx_eval_snapshots_owner"),
    ] {
        ensure!(
            manager.has_index(table, index).await?,
            "legacy index removed: {index}"
        );
    }
    Ok(())
}

pub(super) async fn assert_all_null(db: &DatabaseConnection) -> Result<()> {
    for table in TABLES {
        let mut populated = Condition::any();
        for column in COLUMNS {
            populated = populated.add(Expr::col(Alias::new(column)).is_not_null());
        }
        let statement = Query::select()
            .expr(Expr::val(1))
            .from(Alias::new(table))
            .cond_where(populated)
            .limit(1)
            .to_owned();
        ensure!(
            db.query_one(db.get_database_backend().build(&statement))
                .await?
                .is_none(),
            "legacy ownership was backfilled"
        );
    }
    Ok(())
}

pub(super) async fn set_metadata(
    db: &DatabaseConnection,
    table: &str,
    values: [Option<&str>; 3],
) -> Result<()> {
    let statement = Query::update()
        .table(Alias::new(table))
        .values(
            COLUMNS
                .into_iter()
                .zip(values)
                .map(|(name, value)| (Alias::new(name), value.into())),
        )
        .to_owned();
    db.execute(db.get_database_backend().build(&statement))
        .await?;
    Ok(())
}

pub(super) async fn metadata_count(
    db: &DatabaseConnection,
    table: &str,
    column: &str,
    value: &str,
) -> Result<usize> {
    let statement = Query::select()
        .column(Alias::new(column))
        .from(Alias::new(table))
        .and_where(Expr::col(Alias::new(column)).eq(value))
        .to_owned();
    Ok(db
        .query_all(db.get_database_backend().build(&statement))
        .await?
        .len())
}

pub(super) async fn add_column(
    manager: &SchemaManager<'_>,
    table: &str,
    column: ColumnDef,
) -> Result<()> {
    manager
        .alter_table(
            Table::alter()
                .table(Alias::new(table))
                .add_column(column)
                .to_owned(),
        )
        .await?;
    Ok(())
}

pub(super) async fn drop_column(
    manager: &SchemaManager<'_>,
    table: &str,
    column: &str,
) -> Result<()> {
    manager
        .alter_table(
            Table::alter()
                .table(Alias::new(table))
                .drop_column(Alias::new(column))
                .to_owned(),
        )
        .await?;
    Ok(())
}

pub(super) async fn cleanup(db: &DatabaseConnection) -> Result<()> {
    // Called only after all four names were proven absent before fixture setup.
    let manager = SchemaManager::new(db);
    for table in TABLES.into_iter().rev() {
        if manager.has_table(table).await? {
            manager
                .drop_table(Table::drop().table(Alias::new(table)).to_owned())
                .await?;
        }
    }
    Ok(())
}
