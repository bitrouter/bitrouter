//! Fence Legacy capture/checkpoint/evolution namespaces without assigning any legacy row.
//!
//! Migration execution requires exclusive schema ownership. Each DDL statement
//! can resume independently after MySQL's implicit commit; existing definitions
//! must match before any change. Downgrade never erases populated provenance.

use sea_orm::{ConnectionTrait, DatabaseBackend, QueryResult, Statement, Value};
use sea_orm_migration::prelude::*;

use super::m20240101_000028_resource_tenancy::identity;

mod dependencies;

#[derive(DeriveMigrationName)]
pub struct Migration;

const TABLES: [&str; 9] = [
    "acp_capture_connections",
    "acp_canonical_sessions",
    "acp_capture_events",
    "checkpoint_evolution",
    "acp_checkpoints",
    "acp_checkpoint_members",
    "acp_resource_observations",
    "acp_assessment_revisions",
    "acp_effective_assessments",
];
const COLUMNS: [&str; 3] = ["installation_id", "tenant_id", "creator_principal_id"];

struct ScopedIndex {
    table: &'static str,
    name: &'static str,
    columns: &'static [&'static str],
}

const INDEXES: [ScopedIndex; 9] = [
    ScopedIndex {
        table: "acp_capture_connections",
        name: "idx_legacy33_0_scope",
        columns: &["installation_id", "tenant_id"],
    },
    ScopedIndex {
        table: "acp_canonical_sessions",
        name: "idx_legacy33_1_scope",
        columns: &["installation_id", "tenant_id"],
    },
    ScopedIndex {
        table: "acp_capture_events",
        name: "idx_legacy33_2_scope",
        columns: &["installation_id", "tenant_id"],
    },
    ScopedIndex {
        table: "checkpoint_evolution",
        name: "idx_legacy33_3_scope",
        columns: &["installation_id", "tenant_id"],
    },
    ScopedIndex {
        table: "acp_checkpoints",
        name: "idx_legacy33_4_scope",
        columns: &["installation_id", "tenant_id"],
    },
    ScopedIndex {
        table: "acp_checkpoint_members",
        name: "idx_legacy33_5_scope",
        columns: &["installation_id", "tenant_id"],
    },
    ScopedIndex {
        table: "acp_resource_observations",
        name: "idx_legacy33_6_scope",
        columns: &["installation_id", "tenant_id"],
    },
    ScopedIndex {
        table: "acp_assessment_revisions",
        name: "idx_legacy33_7_scope",
        columns: &["installation_id", "tenant_id"],
    },
    ScopedIndex {
        table: "acp_effective_assessments",
        name: "idx_legacy33_8_scope",
        columns: &["installation_id", "tenant_id"],
    },
];

impl ScopedIndex {
    fn create(&self) -> IndexCreateStatement {
        let mut statement = Index::create();
        statement.name(self.name).table(Alias::new(self.table));
        for column in self.columns {
            statement.col(Alias::new(*column));
        }
        statement.to_owned()
    }

    fn drop(&self) -> IndexDropStatement {
        Index::drop()
            .name(self.name)
            .table(Alias::new(self.table))
            .to_owned()
    }
}

fn incompatible(object: &str) -> DbErr {
    DbErr::Custom(format!(
        "migration033 incompatible existing definition: {object}"
    ))
}

async fn query(
    manager: &SchemaManager<'_>,
    sql: &str,
    values: Vec<Value>,
) -> Result<Vec<QueryResult>, DbErr> {
    manager
        .get_connection()
        .query_all(Statement::from_sql_and_values(
            manager.get_database_backend(),
            sql,
            values,
        ))
        .await
}

async fn has_column(manager: &SchemaManager<'_>, table: &str, column: &str) -> Result<bool, DbErr> {
    if manager.get_database_backend() == DatabaseBackend::Sqlite {
        // SchemaManager uses table_info, which hides generated columns. Such a
        // preexisting name is incompatible, not an absent column safe to add.
        return Ok(!query(
            manager,
            "SELECT name FROM pragma_table_xinfo(?) WHERE lower(name) = ?",
            vec![table.into(), column.into()],
        )
        .await?
        .is_empty());
    }
    manager.has_column(table, column).await
}

async fn server_column(
    manager: &SchemaManager<'_>,
    table: &str,
    column: &str,
) -> Result<QueryResult, DbErr> {
    let sql = match manager.get_database_backend() {
        DatabaseBackend::MySql => {
            "SELECT DATA_TYPE AS kind, CAST(CHARACTER_MAXIMUM_LENGTH AS CHAR) AS width, \
             CAST(CHARACTER_OCTET_LENGTH AS CHAR) AS octets, COLLATION_NAME AS collation_name, \
             CHARACTER_SET_NAME AS character_set, IS_NULLABLE AS nullable, \
             COLUMN_DEFAULT AS default_value, EXTRA AS extra, GENERATION_EXPRESSION AS generation_value \
             FROM information_schema.columns WHERE table_schema = DATABASE() \
             AND table_name = ? AND column_name = ?"
        }
        DatabaseBackend::Postgres => {
            "SELECT data_type AS kind, character_maximum_length::text AS width, \
             collation_name, collation_schema, is_nullable AS nullable, column_default AS default_value, \
             is_generated AS generation_value, is_identity, domain_name \
             FROM information_schema.columns WHERE table_schema = current_schema() \
             AND table_name = $1 AND column_name = $2"
        }
        DatabaseBackend::Sqlite => return Err(incompatible("unexpected SQLite catalog request")),
    };
    let mut rows = query(manager, sql, vec![table.into(), column.into()]).await?;
    if rows.len() != 1 {
        return Err(incompatible(&format!("{table}.{column}")));
    }
    rows.pop().ok_or_else(|| incompatible(column))
}

async fn validate_column(
    manager: &SchemaManager<'_>,
    table: &str,
    column: &str,
) -> Result<(), DbErr> {
    if manager.get_database_backend() == DatabaseBackend::Sqlite {
        validate_sqlite_column(manager, table, column).await?;
        return validate_constraints(manager, table, column).await;
    }
    let row = server_column(manager, table, column).await?;
    let common = row.try_get::<Option<String>>("", "width")?.as_deref() == Some("128")
        && row.try_get::<String>("", "nullable")? == "YES"
        && row
            .try_get::<Option<String>>("", "default_value")?
            .is_none();
    let valid = match manager.get_database_backend() {
        DatabaseBackend::MySql => {
            row.try_get::<String>("", "kind")? == "varchar"
                && row
                    .try_get::<Option<String>>("", "character_set")?
                    .as_deref()
                    == Some("utf8mb4")
                && row
                    .try_get::<Option<String>>("", "collation_name")?
                    .as_deref()
                    == Some("utf8mb4_0900_bin")
                && row.try_get::<String>("", "extra")?.is_empty()
                && row.try_get::<String>("", "generation_value")?.is_empty()
        }
        DatabaseBackend::Postgres => {
            row.try_get::<String>("", "kind")? == "character varying"
                && row
                    .try_get::<Option<String>>("", "collation_name")?
                    .as_deref()
                    == Some("C")
                && row
                    .try_get::<Option<String>>("", "collation_schema")?
                    .as_deref()
                    == Some("pg_catalog")
                && row.try_get::<String>("", "generation_value")? == "NEVER"
                && row.try_get::<String>("", "is_identity")? == "NO"
                && row.try_get::<Option<String>>("", "domain_name")?.is_none()
        }
        DatabaseBackend::Sqlite => false,
    };
    if !common || !valid {
        return Err(incompatible(&format!("{table}.{column}")));
    }
    validate_constraints(manager, table, column).await
}

async fn validate_constraints(
    manager: &SchemaManager<'_>,
    table: &str,
    column: &str,
) -> Result<(), DbErr> {
    let sql = match manager.get_database_backend() {
        DatabaseBackend::Sqlite => {
            "SELECT l.name FROM pragma_index_list(?) l JOIN pragma_index_xinfo(l.name) x \
             WHERE l.\"unique\" <> 0 AND x.\"key\" = 1 AND lower(x.name) = ? LIMIT 1"
        }
        DatabaseBackend::MySql => {
            "SELECT constraint_name FROM information_schema.key_column_usage \
             WHERE table_schema = DATABASE() AND table_name = ? AND column_name = ? LIMIT 1"
        }
        DatabaseBackend::Postgres => {
            // constraint_column_usage reports the referenced side of an FK;
            // conkey also covers the referencing columns. Expression and
            // predicate dependencies are absent from indkey's column numbers.
            // https://www.postgresql.org/docs/17/catalog-pg-constraint.html
            // https://www.postgresql.org/docs/17/catalog-pg-depend.html
            "SELECT constraint_name FROM information_schema.constraint_column_usage \
             WHERE table_schema = current_schema() AND table_name = $1 AND column_name = $2 \
             UNION ALL SELECT c.conname FROM pg_constraint c \
             JOIN pg_class t ON t.oid = c.conrelid \
             JOIN pg_namespace s ON s.oid = t.relnamespace \
             JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = ANY(c.conkey) \
             WHERE s.nspname = current_schema() AND t.relname = $1 AND a.attname = $2 \
             UNION ALL SELECT x.relname FROM pg_index i JOIN pg_class t ON t.oid = i.indrelid \
             JOIN pg_namespace s ON s.oid = t.relnamespace JOIN pg_class x ON x.oid = i.indexrelid \
             JOIN pg_attribute a ON a.attrelid = t.oid \
             WHERE s.nspname = current_schema() AND t.relname = $1 AND a.attname = $2 \
             AND i.indisunique AND (a.attnum = ANY(i.indkey) OR EXISTS (\
                 SELECT 1 FROM pg_depend d WHERE d.classid = 'pg_catalog.pg_class'::regclass \
                 AND d.objid = i.indexrelid AND d.refclassid = 'pg_catalog.pg_class'::regclass \
                 AND d.refobjid = t.oid AND d.refobjsubid = a.attnum)) LIMIT 1"
        }
    };
    if !query(manager, sql, vec![table.into(), column.into()])
        .await?
        .is_empty()
    {
        return Err(incompatible(&format!("constraint on {table}.{column}")));
    }
    validate_unique_expressions(manager, table, column).await?;
    if manager.get_database_backend() == DatabaseBackend::MySql {
        // CHECK constraints do not appear in MySQL key_column_usage. Catalog
        // clauses are tokenized so a quoted literal cannot masquerade as an ID.
        let rows = query(manager,
            "SELECT c.CHECK_CLAUSE AS clause FROM information_schema.check_constraints c \
             JOIN information_schema.table_constraints t ON t.CONSTRAINT_SCHEMA = c.CONSTRAINT_SCHEMA \
             AND t.CONSTRAINT_NAME = c.CONSTRAINT_NAME WHERE t.TABLE_SCHEMA = DATABASE() \
             AND t.TABLE_NAME = ?", vec![table.into()]).await?;
        for row in rows {
            if mysql_catalog_tokens(&row.try_get::<String>("", "clause")?)?
                .contains(&Token::Word(column.to_owned()))
            {
                return Err(incompatible(&format!("check on {table}.{column}")));
            }
        }
    }
    Ok(())
}

async fn validate_unique_expressions(
    manager: &SchemaManager<'_>,
    table: &str,
    column: &str,
) -> Result<(), DbErr> {
    let backend = manager.get_database_backend();
    let sql = match backend {
        // Expression keys have NULL names in index_xinfo. Read the stored SQL
        // as well, including a partial index predicate, without treating the
        // index name or string literals as column dependencies.
        // https://www.sqlite.org/pragma.html#pragma_index_xinfo
        DatabaseBackend::Sqlite => {
            "SELECT s.sql AS expression FROM pragma_index_list(?) l \
             JOIN sqlite_schema s ON s.type = 'index' AND s.name = l.name \
             WHERE l.\"unique\" <> 0 AND s.sql IS NOT NULL"
        }
        // Functional key parts are not ordinary key_column_usage columns.
        // https://dev.mysql.com/doc/refman/8.4/en/information-schema-statistics-table.html
        DatabaseBackend::MySql => {
            "SELECT EXPRESSION AS expression FROM information_schema.statistics \
             WHERE table_schema = DATABASE() AND table_name = ? \
             AND NON_UNIQUE = 0 AND EXPRESSION IS NOT NULL"
        }
        DatabaseBackend::Postgres => return Ok(()), // pg_depend was checked above.
    };
    for row in query(manager, sql, vec![table.into()]).await? {
        let expression = row.try_get::<String>("", "expression")?;
        let tokens = if backend == DatabaseBackend::MySql {
            mysql_catalog_tokens(&expression)?
        } else {
            sqlite_tokens(&expression)?
        };
        let expression = if backend == DatabaseBackend::Sqlite {
            let start = tokens
                .iter()
                .position(|token| token == &Token::Symbol('('))
                .ok_or_else(|| incompatible("incomplete SQLite index definition"))?;
            &tokens[start..]
        } else {
            &tokens[..]
        };
        if expression.contains(&Token::Word(column.to_owned())) {
            return Err(incompatible(&format!(
                "unique expression on {table}.{column}"
            )));
        }
    }
    Ok(())
}

// SQLite's table_info omits collation and CHECK clauses. Inspect the column's
// complete tokenized declaration as well; never infer BINARY from sample rows.
#[derive(Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    Symbol(char),
    Literal,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SchemaDialect {
    Sqlite,
    MySql,
}

fn mysql_catalog_tokens(expression: &str) -> Result<Vec<Token>, DbErr> {
    if expression.len() > 65_536 {
        return Err(incompatible("oversize MySQL catalog expression"));
    }
    // MySQL 8.4 stores both generation_expression_utf8 (STATISTICS.EXPRESSION)
    // and check_clause_utf8 via convert_and_print, which String::print escapes
    // once around the already normalized SQL expression. Decode that catalog
    // layer first, then recognize SQL literal escapes. The normalized SQL uses
    // Sql_mode_parse_guard, independent of the connection's SQL mode.
    // https://github.com/mysql/mysql-server/blob/8.4/sql/dd/dd_table.cc
    // https://github.com/mysql/mysql-server/blob/8.4/sql-common/sql_string.cc
    // https://github.com/mysql/mysql-server/blob/8.4/sql/thd_raii.h
    let mut sql = String::with_capacity(expression.len());
    let mut chars = expression.chars();
    while let Some(ch) = chars.next() {
        sql.push(if ch == '\\' {
            match chars.next() {
                Some('\\') => '\\',
                Some('\'') => '\'',
                Some('0') => '\0',
                Some('n') => '\n',
                Some('r') => '\r',
                Some('Z') => '\u{001a}',
                _ => return Err(incompatible("invalid MySQL catalog escape")),
            }
        } else {
            ch
        });
    }
    schema_tokens(&sql, SchemaDialect::MySql)
}

fn sqlite_tokens(sql: &str) -> Result<Vec<Token>, DbErr> {
    schema_tokens(sql, SchemaDialect::Sqlite)
}

fn schema_tokens(sql: &str, dialect: SchemaDialect) -> Result<Vec<Token>, DbErr> {
    if sql.len() > 65_536 {
        return Err(incompatible("oversize schema definition"));
    }
    let mut tokens = Vec::new();
    let mut chars = sql.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch.is_whitespace() {
            continue;
        }
        if ch == '-' && chars.peek() == Some(&'-') {
            for next in chars.by_ref() {
                if next == '\n' {
                    break;
                }
            }
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut closed = false;
            while let Some(next) = chars.next() {
                if next == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    closed = true;
                    break;
                }
            }
            if !closed {
                return Err(incompatible("unterminated schema comment"));
            }
            continue;
        }
        if matches!(ch, '\'' | '"' | '`' | '[') {
            let close = if ch == '[' { ']' } else { ch };
            let literal = ch == '\'' || (dialect == SchemaDialect::MySql && ch == '"');
            let mut word = String::new();
            let mut closed = false;
            while let Some(next) = chars.next() {
                if dialect == SchemaDialect::MySql && literal && next == '\\' {
                    // An escaped quote cannot terminate a MySQL literal. No
                    // literal contents participate in identifier matching.
                    chars
                        .next()
                        .ok_or_else(|| incompatible("unterminated MySQL literal escape"))?;
                } else if next == close {
                    if ch != '[' && chars.peek() == Some(&close) {
                        chars.next();
                        word.push(close);
                    } else {
                        closed = true;
                        break;
                    }
                } else {
                    word.push(next);
                }
            }
            if !closed {
                return Err(incompatible("unterminated schema quote"));
            }
            tokens.push(if literal {
                Token::Literal
            } else {
                Token::Word(word.to_ascii_lowercase())
            });
        } else if ch.is_alphanumeric() || ch == '_' {
            let mut word = String::from(ch);
            while let Some(next) = chars.peek().copied() {
                if !next.is_alphanumeric() && next != '_' {
                    break;
                }
                chars.next();
                word.push(next);
            }
            tokens.push(Token::Word(word.to_ascii_lowercase()));
        } else {
            tokens.push(Token::Symbol(ch));
        }
    }
    Ok(tokens)
}

fn sqlite_declarations(sql: &str) -> Result<Vec<Vec<Token>>, DbErr> {
    let mut depth = 0_usize;
    let mut declaration = Vec::new();
    let mut declarations = Vec::new();
    for token in sqlite_tokens(sql)? {
        match token {
            Token::Symbol('(') => {
                if depth > 0 {
                    declaration.push(token);
                }
                depth += 1;
            }
            Token::Symbol(')') if depth == 1 => {
                declarations.push(declaration);
                return Ok(declarations);
            }
            Token::Symbol(')') if depth > 1 => {
                depth -= 1;
                declaration.push(token);
            }
            Token::Symbol(',') if depth == 1 => {
                declarations.push(std::mem::take(&mut declaration));
            }
            _ if depth > 0 => declaration.push(token),
            _ => {}
        }
    }
    Err(incompatible("incomplete SQLite table definition"))
}

async fn validate_sqlite_column(
    manager: &SchemaManager<'_>,
    table: &str,
    column: &str,
) -> Result<(), DbErr> {
    // SQLite accepts single-quoted names in foreign-key column lists. Catalog
    // resolution, rather than literal tokenization, identifies those columns.
    // https://www.sqlite.org/pragma.html#pragma_foreign_key_list
    for row in query(
        manager,
        "SELECT \"from\" AS source_column FROM pragma_foreign_key_list(?)",
        vec![table.into()],
    )
    .await?
    {
        if row
            .try_get::<String>("", "source_column")?
            .eq_ignore_ascii_case(column)
        {
            return Err(incompatible(&format!("foreign key on {table}.{column}")));
        }
    }
    let rows = query(
        manager,
        "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?",
        vec![table.into()],
    )
    .await?;
    let sql = rows
        .first()
        .ok_or_else(|| incompatible(table))?
        .try_get::<String>("", "sql")?;
    let expected = sqlite_tokens(&format!("{column} varchar(128) COLLATE BINARY"))?;
    let mut found = false;
    for declaration in sqlite_declarations(&sql)? {
        if declaration.first() == Some(&Token::Word(column.to_owned())) {
            if found || declaration != expected {
                return Err(incompatible(&format!("{table}.{column}")));
            }
            found = true;
        } else if declaration
            .iter()
            .any(|token| token == &Token::Word(column.to_owned()))
        {
            // A table/other-column constraint can change nullability or valid
            // identities even when the target column itself looks compatible.
            return Err(incompatible(&format!("constraint on {table}.{column}")));
        }
    }
    if !found {
        return Err(incompatible(&format!("{table}.{column}")));
    }
    Ok(())
}

async fn validate_index(manager: &SchemaManager<'_>, index: &ScopedIndex) -> Result<(), DbErr> {
    let columns =
        match manager.get_database_backend() {
            DatabaseBackend::MySql => {
                let rows = query(
                    manager,
                    "SELECT COLUMN_NAME AS column_name, CAST(NON_UNIQUE AS CHAR) AS non_unique, \
                 SUB_PART AS prefix_length, COLLATION AS direction, INDEX_TYPE AS index_type, \
                 IS_VISIBLE AS visible, EXPRESSION AS expression \
                 FROM information_schema.statistics WHERE table_schema = DATABASE() \
                 AND table_name = ? AND index_name = ? ORDER BY SEQ_IN_INDEX",
                    vec![index.table.into(), index.name.into()],
                )
                .await?;
                let mut columns = Vec::new();
                for row in rows {
                    if row.try_get::<String>("", "non_unique")? != "1"
                        || row.try_get::<Option<i64>>("", "prefix_length")?.is_some()
                        || row.try_get::<Option<String>>("", "direction")?.as_deref() != Some("A")
                        || row.try_get::<String>("", "index_type")? != "BTREE"
                        || row.try_get::<String>("", "visible")? != "YES"
                        || row.try_get::<Option<String>>("", "expression")?.is_some()
                    {
                        return Err(incompatible(index.name));
                    }
                    columns.push(row.try_get::<String>("", "column_name")?);
                }
                columns
            }
            DatabaseBackend::Postgres => {
                // The per-column pg_get_indexdef output omits collation,
                // opclass and ordering decorations. Check their catalog OIDs
                // and flags instead. These keys use varchar (text_ops) or
                // bigint (int8_ops); both must keep their built-in defaults.
                // https://www.postgresql.org/docs/17/catalog-pg-index.html
                // https://www.postgresql.org/docs/17/catalog-pg-opclass.html
                let rows = query(
                    manager,
                    "SELECT a.attname AS column_name, \
                 CASE WHEN NOT i.indisunique AND i.indisvalid AND i.indisready \
                 AND i.indpred IS NULL AND i.indexprs IS NULL AND i.indnkeyatts = i.indnatts \
                 AND am.amname = 'btree' AND i.indcollation[n.position - 1] = a.attcollation \
                 AND i.indoption[n.position - 1] = 0 \
                 AND opc.opcmethod = x.relam AND opc.opcdefault \
                 AND opc.opcnamespace = 'pg_catalog'::regnamespace \
                 AND opc.opcname = CASE \
                     WHEN a.atttypid = 'pg_catalog.varchar'::regtype THEN 'text_ops' \
                     WHEN a.atttypid = 'pg_catalog.int8'::regtype THEN 'int8_ops' END \
                 THEN 'yes' ELSE 'no' END AS compatible \
                 FROM pg_index i JOIN pg_class t ON t.oid = i.indrelid \
                 JOIN pg_namespace s ON s.oid = t.relnamespace \
                 JOIN pg_class x ON x.oid = i.indexrelid JOIN pg_am am ON am.oid = x.relam \
                 CROSS JOIN LATERAL generate_series(1, i.indnatts) AS n(position) \
                 JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = i.indkey[n.position - 1] \
                 JOIN pg_opclass opc ON opc.oid = i.indclass[n.position - 1] \
                 WHERE s.nspname = current_schema() AND t.relname = $1 AND x.relname = $2 \
                 ORDER BY n.position",
                    vec![index.table.into(), index.name.into()],
                )
                .await?;
                let mut columns = Vec::new();
                for row in rows {
                    if row.try_get::<String>("", "compatible")? != "yes" {
                        return Err(incompatible(index.name));
                    }
                    columns.push(row.try_get::<String>("", "column_name")?);
                }
                columns
            }
            DatabaseBackend::Sqlite => {
                let rows = query(
                    manager,
                    "SELECT \"unique\", origin, partial FROM pragma_index_list(?) WHERE name = ?",
                    vec![index.table.into(), index.name.into()],
                )
                .await?;
                let row = rows.first().ok_or_else(|| incompatible(index.name))?;
                if row.try_get::<i64>("", "unique")? != 0
                    || row.try_get::<i64>("", "partial")? != 0
                    || row.try_get::<String>("", "origin")? != "c"
                {
                    return Err(incompatible(index.name));
                }
                let rows = query(manager,
                "SELECT name, \"desc\", coll, key, cid FROM pragma_index_xinfo(?) ORDER BY seqno",
                vec![index.name.into()]).await?;
                let mut columns = Vec::new();
                for row in rows {
                    if row.try_get::<i64>("", "key")? == 0 {
                        continue;
                    }
                    let name = row.try_get::<String>("", "name")?;
                    if row.try_get::<i64>("", "desc")? != 0
                        || row.try_get::<i64>("", "cid")? < 0
                        || (COLUMNS.contains(&name.as_str())
                            && row.try_get::<String>("", "coll")? != "BINARY")
                    {
                        return Err(incompatible(index.name));
                    }
                    columns.push(name);
                }
                columns
            }
        };
    if !columns
        .iter()
        .map(String::as_str)
        .eq(index.columns.iter().copied())
    {
        return Err(incompatible(index.name));
    }
    Ok(())
}

async fn mysql_key_bytes(
    manager: &SchemaManager<'_>,
    table: &str,
    column: &str,
) -> Result<u64, DbErr> {
    if COLUMNS.contains(&column) {
        return Ok(128 * 4);
    }
    let row = server_column(manager, table, column).await?;
    let kind = row.try_get::<String>("", "kind")?;
    if column == "sequence" && kind == "bigint" {
        return Ok(8);
    }
    if kind != "varchar" {
        return Err(incompatible(&format!("indexed {table}.{column}")));
    }
    row.try_get::<Option<String>>("", "octets")?
        .and_then(|width| width.parse().ok())
        .ok_or_else(|| incompatible(&format!("indexed width {table}.{column}")))
}

async fn mysql_widths(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    if manager.get_database_backend() != DatabaseBackend::MySql {
        return Ok(());
    }
    for index in &INDEXES {
        let mut bytes = 0_u64;
        for column in index.columns {
            bytes = bytes
                .checked_add(mysql_key_bytes(manager, index.table, column).await?)
                .ok_or_else(|| incompatible(index.name))?;
        }
        // InnoDB secondary indexes retain the primary key. Account for every
        // predecessor PK component, including capture event sequence.
        let primary: &[&str] = match index.table {
            "acp_capture_connections" => &["connection_id"],
            "acp_canonical_sessions" | "acp_effective_assessments" => &["session_key"],
            "acp_capture_events" => &["connection_id", "sequence"],
            "checkpoint_evolution" => &["record_id"],
            "acp_checkpoints" => &["checkpoint_id"],
            "acp_checkpoint_members" => &["checkpoint_id", "session_key"],
            "acp_resource_observations" => &["observation_id"],
            "acp_assessment_revisions" => &["revision_id"],
            _ => return Err(incompatible(index.table)),
        };
        for column in primary {
            bytes = bytes
                .checked_add(mysql_key_bytes(manager, index.table, column).await?)
                .ok_or_else(|| incompatible(index.name))?;
        }
        if bytes > 3072 {
            return Err(incompatible(&format!("MySQL key width for {}", index.name)));
        }
    }
    Ok(())
}

async fn validate_index_name(
    manager: &SchemaManager<'_>,
    index: &ScopedIndex,
) -> Result<(), DbErr> {
    let (sql, kind) = match manager.get_database_backend() {
        // SQLite and PostgreSQL index names share a schema namespace. A name
        // already used by another table must fail before adding any columns.
        DatabaseBackend::Sqlite => (
            "SELECT name, tbl_name AS owner_table, type AS object_kind FROM sqlite_schema \
             WHERE lower(name) = ?",
            "index",
        ),
        DatabaseBackend::Postgres => (
            "SELECT x.relname AS name, t.relname AS owner_table, x.relkind::text AS object_kind \
             FROM pg_class x JOIN pg_namespace s ON s.oid = x.relnamespace \
             LEFT JOIN pg_index i ON i.indexrelid = x.oid LEFT JOIN pg_class t ON t.oid = i.indrelid \
             WHERE s.nspname = current_schema() AND x.relname = $1",
            "i",
        ),
        DatabaseBackend::MySql => return Ok(()), // Names are table-local.
    };
    for row in query(manager, sql, vec![index.name.into()]).await? {
        if row.try_get::<String>("", "name")? != index.name
            || row.try_get::<Option<String>>("", "owner_table")?.as_deref() != Some(index.table)
            || row.try_get::<String>("", "object_kind")? != kind
        {
            return Err(incompatible(index.name));
        }
    }
    Ok(())
}

async fn preflight(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    for table in TABLES {
        if !manager.has_table(table).await? {
            return Err(incompatible(&format!("missing predecessor {table}")));
        }
        for column in COLUMNS {
            if has_column(manager, table, column).await? {
                validate_column(manager, table, column).await?;
            }
        }
    }
    for index in &INDEXES {
        validate_index_name(manager, index).await?;
        if manager.has_index(index.table, index.name).await? {
            validate_index(manager, index).await?;
        }
    }
    Ok(())
}

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Inspect the entire partially applied schema before adding anything.
        preflight(manager).await?;
        mysql_widths(manager).await?;
        for table in TABLES {
            for column in COLUMNS {
                if !has_column(manager, table, column).await? {
                    manager
                        .alter_table(
                            Table::alter()
                                .table(Alias::new(table))
                                .add_column(identity(
                                    manager.get_database_backend(),
                                    column,
                                    128,
                                    true,
                                ))
                                .to_owned(),
                        )
                        .await?;
                }
            }
        }
        for index in &INDEXES {
            if !manager.has_index(index.table, index.name).await? {
                manager.create_index(index.create()).await?;
            }
        }
        preflight(manager).await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        preflight(manager).await?;
        dependencies::down(manager).await?;
        // ALL tables must pass before the first DROP, including mixed-NULL rows
        // and partial DDL. No row may be relabeled Legacy by removing metadata.
        for table in TABLES {
            let mut populated = Condition::any();
            let mut present = false;
            for column in COLUMNS {
                if has_column(manager, table, column).await? {
                    populated = populated.add(Expr::col(Alias::new(column)).is_not_null());
                    present = true;
                }
            }
            if present {
                let statement = Query::select()
                    .expr(Expr::val(1))
                    .from(Alias::new(table))
                    .cond_where(populated)
                    .limit(1)
                    .to_owned();
                if manager
                    .get_connection()
                    .query_one(manager.get_database_backend().build(&statement))
                    .await?
                    .is_some()
                {
                    return Err(DbErr::Custom(format!(
                        "migration033 cannot remove populated ownership from {table}"
                    )));
                }
            }
        }
        for index in INDEXES.iter().rev() {
            if manager.has_index(index.table, index.name).await? {
                manager.drop_index(index.drop()).await?;
            }
        }
        for table in TABLES.into_iter().rev() {
            for column in COLUMNS.into_iter().rev() {
                if has_column(manager, table, column).await? {
                    manager
                        .alter_table(
                            Table::alter()
                                .table(Alias::new(table))
                                .drop_column(Alias::new(column))
                                .to_owned(),
                        )
                        .await?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
