//! Downgrade must detect dependencies across ALL tables before the first DDL.
use super::*;

pub(super) async fn down(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let backend = manager.get_database_backend();
    external_objects(manager).await?;
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

// SQLite permits views and triggers on OTHER tables to mention a dropped
// column. Check them before dropping any index. Conservatively refuse any
// reference to this table (including SELECT * and single-quoted identifiers).
// SQL text is catalog data; a substring is deliberately a refusal criterion,
// never an authorization or an instruction to execute that text.
async fn external_objects(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    let sql = match manager.get_database_backend() {
        DatabaseBackend::Sqlite => {
            "SELECT name FROM sqlite_schema WHERE type IN ('view','trigger') AND instr(lower(sql),'provider_continuations')>0 UNION ALL SELECT name FROM sqlite_temp_schema WHERE type IN ('view','trigger') AND instr(lower(sql),'provider_continuations')>0 LIMIT 1"
        }
        DatabaseBackend::Postgres => {
            "SELECT v.relname AS name FROM pg_depend d JOIN pg_rewrite r ON d.classid='pg_catalog.pg_rewrite'::regclass AND r.oid=d.objid JOIN pg_class v ON v.oid=r.ev_class JOIN pg_class t ON t.oid=d.refobjid JOIN pg_namespace n ON n.oid=t.relnamespace WHERE d.refclassid='pg_catalog.pg_class'::regclass AND n.nspname=current_schema() AND t.relname='provider_continuations' AND v.oid<>t.oid LIMIT 1"
        }
        DatabaseBackend::MySql => {
            "SELECT VIEW_NAME AS name FROM information_schema.view_table_usage WHERE TABLE_SCHEMA=DATABASE() AND TABLE_NAME='provider_continuations' LIMIT 1"
        }
    };
    if !query(manager, sql, vec![]).await?.is_empty() {
        return Err(incompatible("dependent view or external trigger"));
    }
    // Procedural trigger bodies are not fully represented in dependency
    // catalogs. Reject textual references as well, including external tables.
    let trigger_sql = match manager.get_database_backend() {
        DatabaseBackend::Sqlite => return Ok(()),
        DatabaseBackend::MySql => {
            "SELECT TRIGGER_NAME AS name FROM information_schema.triggers WHERE LOWER(ACTION_STATEMENT) LIKE '%provider_continuations%' LIMIT 1"
        }
        DatabaseBackend::Postgres => {
            "SELECT g.tgname AS name FROM pg_trigger g JOIN pg_proc p ON p.oid=g.tgfoid WHERE NOT g.tgisinternal AND strpos(lower(p.prosrc),'provider_continuations')>0 LIMIT 1"
        }
    };
    if !query(manager, trigger_sql, vec![]).await?.is_empty() {
        return Err(incompatible("dependent external trigger body"));
    }
    Ok(())
}
