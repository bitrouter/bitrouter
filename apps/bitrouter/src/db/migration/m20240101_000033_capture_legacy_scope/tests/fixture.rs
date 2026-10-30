use super::*;

// Only predecessor columns: neither the snapshot nor its expected values derive
// from migration033's additions. Values retain exact JSON/text byte spelling.
const OLD_COLUMNS: [&[&str]; 9] = [
    &[
        "connection_id",
        "owner",
        "source",
        "controller_instance_id",
        "route_scope_id",
        "state",
        "head",
        "started_at",
        "metadata_json",
    ],
    &[
        "session_key",
        "owner",
        "source",
        "native_session_id",
        "head",
        "history_origin",
        "parent_key",
        "parent_watermark",
        "deleted",
    ],
    &[
        "connection_id",
        "sequence",
        "session_key",
        "session_sequence",
        "replay",
        "event_json",
        "captured_at",
    ],
    &[
        "record_id",
        "scope_id",
        "kind",
        "session_key",
        "revision",
        "body",
    ],
    &[
        "checkpoint_id",
        "session_key",
        "manifest_json",
        "watermark",
        "deleted",
    ],
    &["checkpoint_id", "session_key"],
    &[
        "observation_id",
        "checkpoint_id",
        "observed_at",
        "observation_json",
        "revision",
    ],
    &[
        "revision_id",
        "session_key",
        "checkpoint_id",
        "created_at",
        "revision_json",
    ],
    &["session_key", "revision_id"],
];

pub(super) async fn seed(db: &DatabaseConnection) -> Result<()> {
    for (table, columns) in TABLES.into_iter().zip(OLD_COLUMNS) {
        for number in [1_i64, 2] {
            let values: Vec<Value> = columns
                .iter()
                .map(|column| match *column {
                    "head" | "sequence" | "session_sequence" | "revision" | "watermark" => {
                        number.into()
                    }
                    "parent_watermark" => Option::<i64>::None.into(),
                    "parent_key" => Option::<String>::None.into(),
                    "deleted" | "replay" => false.into(),
                    "body" | "manifest_json" | "metadata_json" | "event_json"
                    | "observation_json" | "revision_json" => {
                        format!("{{\n \"z\":\"É / e\\u0301\", \"a\":{number}\n}}  ").into()
                    }
                    "owner" => format!("Historical owner e\u{301} {number} ").into(),
                    "kind" => "fixture".into(),
                    "state" => "complete".into(),
                    "history_origin" => "new".into(),
                    "started_at" | "captured_at" | "observed_at" | "created_at" => {
                        "2026-10-25T08:02:00.000+08:00".into()
                    }
                    _ => format!("{column}/{number}").into(),
                })
                .collect();
            let mut insert = Query::insert();
            insert
                .into_table(Alias::new(table))
                .columns(columns.iter().map(|column| Alias::new(*column)));
            insert.values(values.into_iter().map(Into::into))?;
            db.execute(db.get_database_backend().build(&insert)).await?;
        }
    }
    Ok(())
}

pub(super) async fn historical(db: &DatabaseConnection) -> Result<Vec<Vec<Vec<Option<String>>>>> {
    let mut tables = Vec::new();
    for (table, columns) in TABLES.into_iter().zip(OLD_COLUMNS) {
        let mut select = Query::select();
        select.from(Alias::new(table));
        let cast = if db.get_database_backend() == DatabaseBackend::MySql {
            "CHAR"
        } else {
            "TEXT"
        };
        for column in columns {
            select.expr_as(
                Expr::col(Alias::new(*column)).cast_as(Alias::new(cast)),
                Alias::new(*column),
            );
        }
        select.order_by(Alias::new(columns[0]), Order::Asc);
        let mut rows = Vec::new();
        for row in db
            .query_all(db.get_database_backend().build(&select))
            .await?
        {
            rows.push(
                columns
                    .iter()
                    .map(|column| row.try_get::<Option<String>>("", column))
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            );
        }
        ensure!(rows.len() == 2, "missing predecessor rows in {table}");
        tables.push(rows);
    }
    Ok(tables)
}

pub(super) async fn legacy_indexes(manager: &SchemaManager<'_>) -> Result<()> {
    for (table, name) in [
        ("acp_capture_events", "idx_acp_session_sequence"),
        ("acp_checkpoints", "idx_acp_checkpoints_session"),
        ("acp_checkpoint_members", "idx_acp_members_session"),
        ("acp_resource_observations", "idx_acp_resources_checkpoint"),
        ("acp_assessment_revisions", "idx_acp_revisions_session"),
        ("acp_assessment_revisions", "idx_acp_revisions_checkpoint"),
        (
            "checkpoint_evolution",
            "idx_checkpoint_evolution_scope_kind",
        ),
        ("checkpoint_evolution", "idx_checkpoint_evolution_session"),
    ] {
        ensure!(
            manager.has_index(table, name).await?,
            "removed predecessor index {name}"
        );
    }
    Ok(())
}

pub(super) async fn complete(manager: &SchemaManager<'_>) -> Result<()> {
    for table in TABLES {
        for column in COLUMNS {
            ensure!(has_column(manager, table, column).await?);
            validate_column(manager, table, column).await?;
        }
    }
    for index in &INDEXES {
        ensure!(manager.has_index(index.table, index.name).await?);
        validate_index(manager, index).await?;
    }
    legacy_indexes(manager).await
}

pub(super) async fn absent(manager: &SchemaManager<'_>) -> Result<()> {
    for table in TABLES {
        ensure!(manager.has_table(table).await?);
        for column in COLUMNS {
            ensure!(!has_column(manager, table, column).await?);
        }
    }
    for index in &INDEXES {
        ensure!(!manager.has_index(index.table, index.name).await?);
    }
    legacy_indexes(manager).await
}

pub(super) async fn all_null(db: &DatabaseConnection) -> Result<()> {
    for table in TABLES {
        let condition = COLUMNS.into_iter().fold(Condition::any(), |c, column| {
            c.add(Expr::col(Alias::new(column)).is_not_null())
        });
        let select = Query::select()
            .expr(Expr::val(1))
            .from(Alias::new(table))
            .cond_where(condition)
            .limit(1)
            .to_owned();
        ensure!(
            db.query_one(db.get_database_backend().build(&select))
                .await?
                .is_none(),
            "backfilled {table}"
        );
    }
    Ok(())
}

pub(super) async fn metadata(
    db: &DatabaseConnection,
    table: &str,
    column: &str,
    value: Option<&str>,
) -> Result<()> {
    let update = Query::update()
        .table(Alias::new(table))
        .value(Alias::new(column), value)
        .to_owned();
    ensure!(
        db.execute(db.get_database_backend().build(&update))
            .await?
            .rows_affected()
            == 2
    );
    Ok(())
}

pub(super) async fn add(manager: &SchemaManager<'_>, table: &str, column: ColumnDef) -> Result<()> {
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

pub(super) async fn drop(manager: &SchemaManager<'_>, table: &str, column: &str) -> Result<()> {
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
