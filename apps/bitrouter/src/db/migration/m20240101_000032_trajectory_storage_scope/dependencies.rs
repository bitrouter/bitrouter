//! Downgrade must detect dependencies across ALL tables before the first DDL.
use super::*;

pub(super) async fn down(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let backend = manager.get_database_backend();
    if backend == DatabaseBackend::Sqlite {
        sqlite_objects(manager).await?;
    }
    for table in TABLES {
        // User triggers may hide dependencies in procedural code. None belong
        // to the predecessor schema, so refuse rather than partially drop DDL.
        let trigger_sql = match backend {
            DatabaseBackend::Sqlite => {
                "SELECT name FROM sqlite_schema WHERE type = 'trigger' AND tbl_name = ? LIMIT 1"
            }
            DatabaseBackend::MySql => {
                "SELECT TRIGGER_NAME AS name FROM information_schema.triggers WHERE TRIGGER_SCHEMA = DATABASE() AND EVENT_OBJECT_TABLE = ? LIMIT 1"
            }
            DatabaseBackend::Postgres => {
                "SELECT g.tgname AS name FROM pg_trigger g JOIN pg_class t ON t.oid = g.tgrelid JOIN pg_namespace n ON n.oid = t.relnamespace WHERE n.nspname = current_schema() AND t.relname = $1 AND NOT g.tgisinternal LIMIT 1"
            }
        };
        if !query(manager, trigger_sql, vec![table.into()])
            .await?
            .is_empty()
        {
            return Err(incompatible(&format!("user trigger on {table}")));
        }
        for &column in columns(table) {
            if !has_column(manager, table, column).await? {
                continue;
            }
            let inbound = match backend {
                DatabaseBackend::Sqlite => {
                    "SELECT t.name FROM sqlite_schema t JOIN pragma_foreign_key_list(t.name) f WHERE t.type = 'table' AND lower(f.\"table\") = ? AND lower(f.\"to\") = ? LIMIT 1"
                }
                DatabaseBackend::MySql => {
                    "SELECT CONSTRAINT_NAME AS name FROM information_schema.key_column_usage WHERE REFERENCED_TABLE_SCHEMA = DATABASE() AND REFERENCED_TABLE_NAME = ? AND REFERENCED_COLUMN_NAME = ? LIMIT 1"
                }
                DatabaseBackend::Postgres => {
                    "SELECT constraint_name AS name FROM information_schema.constraint_column_usage WHERE table_schema = current_schema() AND table_name = $1 AND column_name = $2 LIMIT 1"
                }
            };
            if !query(manager, inbound, vec![table.into(), column.into()])
                .await?
                .is_empty()
            {
                return Err(incompatible(&format!(
                    "referencing constraint on {table}.{column}"
                )));
            }
            let sql = match backend {
                DatabaseBackend::Sqlite => {
                    "SELECT s.name, s.sql AS expression FROM sqlite_schema s WHERE s.type = 'index' AND s.tbl_name = ? AND s.sql IS NOT NULL"
                }
                DatabaseBackend::MySql => {
                    "SELECT INDEX_NAME AS name, COLUMN_NAME AS column_name, EXPRESSION AS expression FROM information_schema.statistics WHERE table_schema = DATABASE() AND table_name = ?"
                }
                DatabaseBackend::Postgres => {
                    "SELECT DISTINCT x.relname AS name, a.attname AS column_name FROM pg_index i JOIN pg_class t ON t.oid = i.indrelid JOIN pg_namespace s ON s.oid = t.relnamespace JOIN pg_class x ON x.oid = i.indexrelid JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum > 0 WHERE s.nspname = current_schema() AND t.relname = $1 AND (a.attnum = ANY(i.indkey) OR EXISTS (SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_catalog.pg_class'::regclass AND d.objid = i.indexrelid AND d.refclassid = 'pg_catalog.pg_class'::regclass AND d.refobjid = t.oid AND d.refobjsubid = a.attnum))"
                }
            };
            for row in query(manager, sql, vec![table.into()]).await? {
                let name = row.try_get::<String>("", "name")?;
                if INDEXES.iter().any(|i| i.table == table && i.name == name) {
                    continue;
                }
                if backend != DatabaseBackend::Sqlite
                    && row.try_get::<Option<String>>("", "column_name")?.as_deref() == Some(column)
                {
                    return Err(incompatible(&format!("dependent index {name}")));
                }
                if backend == DatabaseBackend::Postgres {
                    continue;
                }
                if let Some(expression) = row.try_get::<Option<String>>("", "expression")? {
                    let tokens = if backend == DatabaseBackend::MySql {
                        mysql_catalog_tokens(&expression)?
                    } else {
                        sqlite_tokens(&expression)?
                    };
                    let start = if backend == DatabaseBackend::Sqlite {
                        tokens
                            .iter()
                            .position(|t| *t == Token::Symbol('('))
                            .ok_or_else(|| incompatible(&name))?
                    } else {
                        0
                    };
                    if tokens[start..].contains(&Token::Word(column.into())) {
                        return Err(incompatible(&format!("dependent expression index {name}")));
                    }
                }
            }
        }
    }
    Ok(())
}

// SQLite has no column-dependency catalog for views or trigger bodies. DROP
// COLUMN reparses the entire schema and can fail on either, including a trigger
// attached to another table. Do not guess dependencies from SQL tokens: aliases,
// wildcards and SQLite's quoted-name rules require actual name resolution.
// https://www.sqlite.org/lang_altertable.html#altertabdropcol
// Conservatively require ALL persistent and connection-local views/triggers to
// be removed before dropping remaining scope columns. This read-only check runs
// before any DDL; the server-backend dependency checks above are unchanged.
async fn sqlite_objects(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    if query(
        manager,
        "SELECT 1 AS present FROM main.sqlite_schema WHERE type IN ('view', 'trigger') \
         UNION ALL SELECT 1 AS present FROM temp.sqlite_schema WHERE type IN ('view', 'trigger') \
         LIMIT 1",
        Vec::new(),
    )
    .await?
    .is_empty()
    {
        return Ok(());
    }
    for table in TABLES {
        for &column in columns(table) {
            if has_column(manager, table, column).await? {
                return Err(incompatible(
                    "SQLite downgrade cannot resolve view/trigger dependencies; remove all persistent and temporary views or triggers before dropping scope columns",
                ));
            }
        }
    }
    Ok(())
}
