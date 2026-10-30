//! Exact portable catalog checks; conservative down rejects unresolved executable dependencies.
use super::*;
async fn rows(
    manager: &SchemaManager<'_>,
    sql: &str,
    values: Vec<Value>,
) -> Result<Vec<sea_orm::QueryResult>, DbErr> {
    manager
        .get_connection()
        .query_all(Statement::from_sql_and_values(
            manager.get_database_backend(),
            sql,
            values,
        ))
        .await
}
pub(super) async fn names(manager: &SchemaManager<'_>, spec: &Spec) -> Result<(), DbErr> {
    let sql = match manager.get_database_backend() {
        DatabaseBackend::Sqlite => {
            // SQLite resolves ASCII identifier aliases case-insensitively. Match
            // that collision set, but only adopt exact canonical owned names.
            // Temporary aliases may shadow otherwise compatible main objects.
            "SELECT name FROM sqlite_schema WHERE name COLLATE NOCASE=?1 AND (name COLLATE BINARY<>?1 OR type<>'table') UNION ALL SELECT name FROM temp.sqlite_schema WHERE name COLLATE NOCASE=?1 LIMIT 1"
        }
        DatabaseBackend::Postgres => {
            "SELECT c.relname AS name FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=current_schema() AND c.relname=$1 AND (c.relkind<>'r' OR c.relpersistence<>'p') LIMIT 1"
        }
        DatabaseBackend::MySql => {
            "SELECT TABLE_NAME AS name FROM information_schema.tables WHERE TABLE_SCHEMA=DATABASE() AND TABLE_NAME=? AND (TABLE_TYPE<>'BASE TABLE' OR ENGINE<>'InnoDB') LIMIT 1"
        }
    };
    if !rows(manager, sql, vec![spec.table.into()])
        .await?
        .is_empty()
    {
        return Err(incompatible());
    }
    if let Some((name, _)) = spec.index {
        let sql = match manager.get_database_backend() {
            DatabaseBackend::Sqlite => {
                "SELECT name FROM sqlite_schema WHERE name COLLATE NOCASE=?1 AND (name COLLATE BINARY<>?1 OR type<>'index' OR tbl_name COLLATE BINARY<>?2) UNION ALL SELECT name FROM temp.sqlite_schema WHERE name COLLATE NOCASE=?1 LIMIT 1"
            }
            DatabaseBackend::Postgres => {
                "SELECT c.relname AS name FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace LEFT JOIN pg_index i ON i.indexrelid=c.oid LEFT JOIN pg_class t ON t.oid=i.indrelid WHERE n.nspname=current_schema() AND c.relname=$1 AND (c.relkind<>'i' OR t.relname IS DISTINCT FROM $2) LIMIT 1"
            }
            DatabaseBackend::MySql => return Ok(()), // MySQL names are table scoped.
        };
        if !rows(manager, sql, vec![name.into(), spec.table.into()])
            .await?
            .is_empty()
        {
            return Err(incompatible());
        }
    }
    Ok(())
}
pub(super) async fn validate(
    manager: &SchemaManager<'_>,
    spec: &Spec,
    complete: bool,
) -> Result<(), DbErr> {
    let backend = manager.get_database_backend();
    if backend == DatabaseBackend::Sqlite {
        let found = rows(
            manager,
            "SELECT sql FROM sqlite_schema WHERE type='table' AND name=?",
            vec![spec.table.into()],
        )
        .await?;
        if found.len() != 1
            || found[0].try_get::<String>("", "sql")?
                != definition(spec, backend).to_string(SqliteQueryBuilder)
        {
            return Err(incompatible());
        }
    } else {
        let sql = if backend == DatabaseBackend::Postgres {
            "SELECT column_name AS name,data_type AS kind,character_maximum_length::text AS width,is_nullable AS nullable,column_default AS default_value,collation_name,collation_schema,is_generated AS generated_value,is_identity AS identity_column FROM information_schema.columns WHERE table_schema=current_schema() AND table_name=$1 ORDER BY ordinal_position LIMIT 32"
        } else {
            "SELECT COLUMN_NAME AS name,DATA_TYPE AS kind,COLUMN_TYPE AS full_kind,CAST(CHARACTER_MAXIMUM_LENGTH AS CHAR) AS width,IS_NULLABLE AS nullable,COLUMN_DEFAULT AS default_value,COLLATION_NAME AS collation_name,CHARACTER_SET_NAME AS charset_name,EXTRA AS extra,GENERATION_EXPRESSION AS generation_expression_value FROM information_schema.columns WHERE TABLE_SCHEMA=DATABASE() AND TABLE_NAME=? ORDER BY ORDINAL_POSITION LIMIT 32"
        };
        let actual = rows(manager, sql, vec![spec.table.into()]).await?;
        if actual.len() != spec.columns.len() {
            return Err(incompatible());
        }
        for (row, (name, kind)) in actual.iter().zip(spec.columns) {
            let expected = match (backend, kind) {
                (DatabaseBackend::Postgres, Kind::Id(_)) => "character varying",
                (_, Kind::Id(_)) => "varchar",
                (_, Kind::Integer) => "bigint",
                (DatabaseBackend::MySql, Kind::Boolean) => "tinyint",
                (_, Kind::Boolean) => "boolean",
                (DatabaseBackend::MySql, Kind::Payload) => "longtext",
                (_, Kind::Payload) => "text",
            };
            if row.try_get::<String>("", "name")? != *name
                || row.try_get::<String>("", "kind")? != expected
                || row.try_get::<String>("", "nullable")? != "NO"
                || row
                    .try_get::<Option<String>>("", "default_value")?
                    .is_some()
            {
                return Err(incompatible());
            }
            if let Kind::Id(width) = kind
                && row.try_get::<Option<String>>("", "width")?.as_deref()
                    != Some(width.to_string().as_str())
            {
                return Err(incompatible());
            }
            if backend == DatabaseBackend::Postgres {
                if row.try_get::<String>("", "generated_value")? != "NEVER"
                    || row.try_get::<String>("", "identity_column")? != "NO"
                {
                    return Err(incompatible());
                }
                if matches!(kind, Kind::Id(_))
                    && (row
                        .try_get::<Option<String>>("", "collation_name")?
                        .as_deref()
                        != Some("C")
                        || row
                            .try_get::<Option<String>>("", "collation_schema")?
                            .as_deref()
                            != Some("pg_catalog"))
                {
                    return Err(incompatible());
                }
            } else {
                if !row.try_get::<String>("", "extra")?.is_empty()
                    || !row
                        .try_get::<String>("", "generation_expression_value")?
                        .is_empty()
                {
                    return Err(incompatible());
                }
                if matches!(kind, Kind::Id(_) | Kind::Payload)
                    && (row
                        .try_get::<Option<String>>("", "collation_name")?
                        .as_deref()
                        != Some("utf8mb4_0900_bin")
                        || row
                            .try_get::<Option<String>>("", "charset_name")?
                            .as_deref()
                            != Some("utf8mb4"))
                {
                    return Err(incompatible());
                }
                if matches!(kind, Kind::Integer)
                    && row.try_get::<String>("", "full_kind")? != "bigint"
                {
                    return Err(incompatible());
                }
                if matches!(kind, Kind::Boolean)
                    && row.try_get::<String>("", "full_kind")? != "tinyint(1)"
                {
                    return Err(incompatible());
                }
            }
        }
    }
    indexes(manager, spec, complete).await
}
async fn indexes(manager: &SchemaManager<'_>, spec: &Spec, complete: bool) -> Result<(), DbErr> {
    let backend = manager.get_database_backend();
    let sql = match backend {
        DatabaseBackend::Sqlite => "SELECT name,origin FROM pragma_index_list(?)",
        DatabaseBackend::Postgres => {
            "SELECT x.relname AS name,CASE WHEN i.indisprimary THEN 'pk' ELSE 'c' END AS origin FROM pg_index i JOIN pg_class t ON t.oid=i.indrelid JOIN pg_namespace n ON n.oid=t.relnamespace JOIN pg_class x ON x.oid=i.indexrelid WHERE n.nspname=current_schema() AND t.relname=$1"
        }
        DatabaseBackend::MySql => {
            "SELECT DISTINCT INDEX_NAME AS name,CASE WHEN INDEX_NAME='PRIMARY' THEN 'pk' ELSE 'c' END AS origin FROM information_schema.statistics WHERE TABLE_SCHEMA=DATABASE() AND TABLE_NAME=?"
        }
    };
    let mut primary = false;
    let mut secondary = false;
    for row in rows(manager, sql, vec![spec.table.into()]).await? {
        let name: String = row.try_get("", "name")?;
        let is_primary = row.try_get::<String>("", "origin")? == "pk";
        let expected = if is_primary && !primary {
            primary = true;
            spec.primary
        } else if let Some((index, columns)) = spec.index
            && name == index
            && !secondary
        {
            secondary = true;
            columns
        } else {
            return Err(incompatible());
        };
        let sql = match backend {
            DatabaseBackend::Sqlite => {
                "SELECT name AS column_name,CASE WHEN key=1 AND cid>=0 AND coll='BINARY' AND \"desc\"=0 THEN 'yes' ELSE 'no' END AS compatible FROM pragma_index_xinfo(?) WHERE key=1 ORDER BY seqno"
            }
            DatabaseBackend::Postgres => {
                "SELECT a.attname AS column_name,CASE WHEN i.indisvalid AND i.indisready AND i.indimmediate AND i.indisunique=$2 AND i.indisprimary=$2 AND i.indpred IS NULL AND i.indexprs IS NULL AND i.indnkeyatts=i.indnatts AND am.amname='btree' AND i.indcollation[k.position-1]=a.attcollation AND i.indoption[k.position-1]=0 AND opc.opcmethod=x.relam AND opc.opcdefault AND opc.opcnamespace='pg_catalog'::regnamespace AND opc.opcname='text_ops' THEN 'yes' ELSE 'no' END AS compatible FROM pg_index i JOIN pg_class x ON x.oid=i.indexrelid JOIN pg_namespace n ON n.oid=x.relnamespace JOIN pg_class t ON t.oid=i.indrelid JOIN pg_am am ON am.oid=x.relam CROSS JOIN LATERAL generate_series(1,i.indnatts) k(position) JOIN pg_attribute a ON a.attrelid=t.oid AND a.attnum=i.indkey[k.position-1] JOIN pg_opclass opc ON opc.oid=i.indclass[k.position-1] WHERE n.nspname=current_schema() AND x.relname=$1 AND t.relname=$3 ORDER BY k.position"
            }
            DatabaseBackend::MySql => {
                "SELECT COLUMN_NAME AS column_name,CASE WHEN NON_UNIQUE=? AND SUB_PART IS NULL AND COLLATION='A' AND INDEX_TYPE='BTREE' AND IS_VISIBLE='YES' AND EXPRESSION IS NULL THEN 'yes' ELSE 'no' END AS compatible FROM information_schema.statistics WHERE TABLE_SCHEMA=DATABASE() AND TABLE_NAME=? AND INDEX_NAME=? ORDER BY SEQ_IN_INDEX"
            }
        };
        let values = match backend {
            DatabaseBackend::Sqlite => vec![name.clone().into()],
            DatabaseBackend::Postgres => {
                vec![name.clone().into(), is_primary.into(), spec.table.into()]
            }
            DatabaseBackend::MySql => vec![
                u32::from(!is_primary).into(),
                spec.table.into(),
                name.clone().into(),
            ],
        };
        let actual = rows(manager, sql, values).await?;
        if actual.len() != expected.len() {
            return Err(incompatible());
        }
        for (row, column) in actual.iter().zip(expected) {
            if row.try_get::<String>("", "column_name")? != *column
                || row.try_get::<String>("", "compatible")? != "yes"
            {
                return Err(incompatible());
            }
        }
        if backend == DatabaseBackend::Sqlite {
            let flags = rows(
                manager,
                "SELECT \"unique\" AS unique_value,partial FROM pragma_index_list(?) WHERE name=?",
                vec![spec.table.into(), name.into()],
            )
            .await?;
            let flag = flags.first().ok_or_else(incompatible)?;
            if flag.try_get::<i64>("", "unique_value")? != i64::from(is_primary)
                || flag.try_get::<i64>("", "partial")? != 0
            {
                return Err(incompatible());
            }
        }
    }
    if !primary || (complete && spec.index.is_some() && !secondary) {
        return Err(incompatible());
    }
    let sql = match backend {
        DatabaseBackend::Sqlite => {
            "SELECT 1 FROM sqlite_schema WHERE type='trigger' AND tbl_name=? UNION ALL SELECT 1 FROM temp.sqlite_schema WHERE type='trigger' AND tbl_name=? LIMIT 1"
        }
        DatabaseBackend::Postgres => {
            "SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname=current_schema() AND c.relname=$1 AND (c.relpersistence<>'p' OR c.relrowsecurity OR c.relforcerowsecurity OR EXISTS(SELECT 1 FROM pg_trigger g WHERE g.tgrelid=c.oid AND NOT g.tgisinternal) OR EXISTS(SELECT 1 FROM pg_constraint k WHERE k.conrelid=c.oid AND k.contype<>'p')) LIMIT 1"
        }
        DatabaseBackend::MySql => {
            "SELECT 1 FROM information_schema.table_constraints WHERE CONSTRAINT_SCHEMA=DATABASE() AND TABLE_NAME=? AND CONSTRAINT_TYPE<>'PRIMARY KEY' UNION ALL SELECT 1 FROM information_schema.triggers WHERE TRIGGER_SCHEMA=DATABASE() AND EVENT_OBJECT_TABLE=? LIMIT 1"
        }
    };
    let values = if backend == DatabaseBackend::Postgres {
        vec![spec.table.into()]
    } else {
        vec![spec.table.into(), spec.table.into()]
    };
    if !rows(manager, sql, values).await?.is_empty() {
        return Err(incompatible());
    }
    Ok(())
}
pub(super) async fn dependencies(manager: &SchemaManager<'_>, spec: &Spec) -> Result<(), DbErr> {
    // Conservative, read-only rejection is intentional: arbitrary trigger bodies
    // cannot be resolved by table metadata without executing user SQL.
    let sql = match manager.get_database_backend() {
        DatabaseBackend::Sqlite => {
            "SELECT 1 FROM sqlite_schema WHERE type IN ('view','trigger') UNION ALL SELECT 1 FROM temp.sqlite_schema WHERE type IN ('view','trigger') UNION ALL SELECT 1 FROM sqlite_schema t JOIN pragma_foreign_key_list(t.name) f WHERE t.type='table' AND lower(f.\"table\")=? LIMIT 1"
        }
        DatabaseBackend::Postgres => {
            "SELECT 1 FROM information_schema.view_table_usage WHERE table_schema=current_schema() AND table_name=$1 UNION ALL SELECT 1 FROM pg_constraint k JOIN pg_class t ON t.oid=k.confrelid JOIN pg_namespace n ON n.oid=t.relnamespace WHERE n.nspname=current_schema() AND t.relname=$1 UNION ALL SELECT 1 FROM pg_trigger g JOIN pg_class t ON t.oid=g.tgrelid JOIN pg_namespace n ON n.oid=t.relnamespace WHERE n.nspname=current_schema() AND NOT g.tgisinternal LIMIT 1"
        }
        DatabaseBackend::MySql => {
            "SELECT 1 FROM information_schema.view_table_usage WHERE TABLE_SCHEMA=DATABASE() AND TABLE_NAME=? UNION ALL SELECT 1 FROM information_schema.key_column_usage WHERE REFERENCED_TABLE_SCHEMA=DATABASE() AND REFERENCED_TABLE_NAME=? UNION ALL SELECT 1 FROM information_schema.triggers WHERE TRIGGER_SCHEMA=DATABASE() LIMIT 1"
        }
    };
    let values = if manager.get_database_backend() == DatabaseBackend::MySql {
        vec![spec.table.into(), spec.table.into()]
    } else {
        vec![spec.table.into()]
    };
    if !rows(manager, sql, values).await?.is_empty() {
        return Err(incompatible());
    }
    Ok(())
}
