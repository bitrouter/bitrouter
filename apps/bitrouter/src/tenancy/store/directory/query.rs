//! Bounded SQL projections; malformed cells remain visible as read failures.

use anyhow::{Result, ensure};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, QueryResult, Statement,
    sea_query::{Alias, Expr, ExprTrait, Func, Query, SelectStatement, SimpleExpr},
};

#[derive(Clone, Copy)]
pub(in crate::tenancy::store) enum Table {
    Header,
    Operation,
    Lifecycle,
    Resource,
    ConfigurationBody,
    Tenant,
    Principal,
    Binding,
    Credential,
    Active,
    Configuration,
}

impl Table {
    pub(in crate::tenancy::store) fn name(self) -> &'static str {
        match self {
            Self::Header => "tenancy_installation",
            Self::Operation => "tenancy_operations",
            Self::Lifecycle => "tenancy_resource_lifecycle",
            Self::Resource => "tenancy_resource_revisions",
            Self::ConfigurationBody => "tenancy_configurations",
            Self::Tenant => "tenancy_tenants",
            Self::Principal => "tenancy_principals",
            Self::Binding => "tenancy_bindings",
            Self::Credential => "tenancy_credentials",
            Self::Active => "tenancy_active_configurations",
            Self::Configuration => "tenancy_configurations",
        }
    }

    fn columns(self) -> &'static [(&'static str, bool)] {
        match self {
            Self::Header => &[("installation_id", false), ("created_at", false)],
            Self::Operation => &[
                ("installation_id", false),
                ("principal_id", false),
                ("operation_id", false),
                ("tenant_id", true),
                ("authority_class", false),
                ("read_context_json", false),
                ("command_digest", false),
                ("result_json", false),
                ("response_json", false),
                ("created_at", false),
            ],
            Self::Lifecycle => &[
                ("installation_id", false),
                ("owner_kind", false),
                ("owner_id", false),
                ("resource_kind", false),
                ("resource_id", false),
                ("revision_id", false),
            ],
            Self::Resource => &[
                ("owner_kind", false),
                ("owner_id", false),
                ("resource_kind", false),
                ("resource_id", false),
                ("revision_id", false),
                ("compiler_version", false),
                ("payload_json", false),
                ("dependency_json", false),
                ("content_digest", false),
                ("created_at", false),
            ],
            Self::ConfigurationBody => &[
                ("tenant_id", false),
                ("revision_id", false),
                ("document_json", false),
                ("compiler_version", false),
                ("content_digest", false),
                ("compiled_json", false),
                ("dependencies_json", false),
                ("creator_principal_id", false),
                ("created_at", false),
            ],
            Self::Tenant => &[
                ("id", false),
                ("revision", false),
                ("state", false),
                ("created_at", false),
            ],
            Self::Principal => &[
                ("id", false),
                ("revision", false),
                ("issuer", false),
                ("subject", false),
                ("created_at", false),
            ],
            Self::Binding => &[
                ("tenant_id", false),
                ("id", false),
                ("principal_id", false),
                ("revision", false),
                ("role", false),
                ("scope_json", false),
                ("expires_at", false),
                ("management_owner", true),
                ("management_class", true),
            ],
            Self::Credential => &[
                ("id", false),
                ("tenant_id", false),
                ("principal_id", false),
                ("binding_id", true),
                ("revision", false),
                ("key_hash", false),
                ("state", false),
                ("permissions_json", false),
                ("scope_json", false),
                ("expires_at", false),
                ("created_at", false),
            ],
            Self::Active | Self::Configuration => &[("tenant_id", false), ("revision_id", false)],
        }
    }

    fn integers(self) -> &'static [(&'static str, bool)] {
        match self {
            Self::Header => &[
                ("singleton", false),
                ("schema_version", false),
                ("authority_revision", false),
                ("operation_ids", false),
                ("safety_operation_ids", false),
                ("saturated", true),
            ],
            Self::Operation => &[("hash_version", false), ("safety", true)],
            Self::Lifecycle => &[
                ("is_head", true),
                ("revoked", true),
                ("authority_revision", false),
            ],
            Self::ConfigurationBody => &[("schema_version", false), ("authority_revision", false)],
            Self::Credential => &[("audit_reader", true)],
            Self::Active | Self::Configuration => &[("authority_revision", false)],
            _ => &[],
        }
    }
}

pub(in crate::tenancy::store) fn select(
    db: &impl ConnectionTrait,
    table: Table,
    bytes: usize,
) -> Result<SelectStatement> {
    if bytes == 0 || bytes > isize::MAX as usize {
        return Err(super::CapacityExceeded.into());
    }
    let backend = db.get_database_backend();
    let mut total: SimpleExpr = Expr::val(0_i64).into();
    let mut valid: SimpleExpr = Expr::val(true).into();
    for (name, nullable) in table.columns() {
        let column = Expr::col(Alias::new(*name));
        // Measure bytes before transfer to the driver, including embedded NUL.
        // https://sqlite.org/lang_corefunc.html#length
        // https://www.postgresql.org/docs/current/functions-string.html
        // https://dev.mysql.com/doc/refman/8.4/en/string-functions.html#function_octet-length
        let length = if backend == DatabaseBackend::Sqlite {
            Func::cust(Alias::new("length")).arg(column.clone().cast_as(Alias::new("BLOB")))
        } else {
            Func::cust(Alias::new("octet_length")).arg(column.clone())
        };
        total = Expr::expr(total).add(Func::cust(Alias::new("coalesce")).arg(length).arg(0_i64));
        let kind = if backend == DatabaseBackend::Sqlite {
            // SQLite affinity alone cannot reject arbitrary BLOB/TEXT cells.
            // https://sqlite.org/datatype3.html
            Expr::expr(Func::cust(Alias::new("typeof")).arg(column.clone())).eq("text")
        } else {
            column.clone().is_not_null()
        };
        valid = Expr::expr(valid).and(if *nullable {
            Expr::expr(kind).or(column.is_null())
        } else {
            kind
        });
    }
    for (name, boolean) in table.integers() {
        total = Expr::expr(total).add(8_i64);
        let column = Expr::col(Alias::new(*name));
        let mut kind = if backend == DatabaseBackend::Sqlite {
            Expr::expr(Func::cust(Alias::new("typeof")).arg(column.clone())).eq("integer")
        } else {
            column.clone().is_not_null()
        };
        if *boolean && backend == DatabaseBackend::Sqlite {
            kind = Expr::expr(kind).and(column.is_in([0_i64, 1_i64]));
        }
        valid = Expr::expr(valid).and(kind);
    }
    let fits = Expr::expr(total.clone()).lte(u64::try_from(bytes)?);
    let projected = Expr::expr(valid.clone()).and(fits.clone());
    let mut query = Query::select();
    query.from(Alias::new(table.name()));
    for (name, _) in table.columns() {
        query.expr_as(
            Expr::case(projected.clone(), Expr::col(Alias::new(*name)))
                .finally(Expr::val(Option::<String>::None)),
            Alias::new(*name),
        );
    }
    for (name, boolean) in table.integers() {
        let null: SimpleExpr = if *boolean {
            Expr::val(Option::<bool>::None).into()
        } else {
            Expr::val(Option::<i64>::None).into()
        };
        query.expr_as(
            Expr::case(projected.clone(), Expr::col(Alias::new(*name))).finally(null),
            Alias::new(*name),
        );
    }
    query.expr_as(
        Expr::case(valid, 1_i32).finally(0_i32),
        Alias::new("directory_valid"),
    );
    query.expr_as(total, Alias::new("directory_bytes"));
    query.expr_as(
        Expr::case(fits, 1_i32).finally(0_i32),
        Alias::new("directory_fits"),
    );
    Ok(query)
}

/// Fixed SQLite operation point query. Only the successful branch may return
/// payloads; the other preserves malformed/oversized rows as scalar failures.
pub(in crate::tenancy::store) fn sqlite_operation() -> String {
    let table = Table::Operation;
    let mut columns = Vec::new();
    let mut absent = Vec::new();
    let mut valid = vec!["1".to_owned()];
    let mut bytes = vec!["0".to_owned()];
    for (name, nullable) in table.columns() {
        let column = format!("\"{name}\"");
        columns.push(format!("{column} AS {column}"));
        absent.push(format!("NULL AS {column}"));
        let kind = format!("typeof({column})='text'");
        valid.push(if *nullable {
            format!("({kind} OR {column} IS NULL)")
        } else {
            kind
        });
        // Same aggregate byte accounting as select(), including embedded NUL.
        bytes.push(format!("coalesce(length(CAST({column} AS BLOB)),0)"));
    }
    for (name, boolean) in table.integers() {
        let column = format!("\"{name}\"");
        columns.push(format!("{column} AS {column}"));
        absent.push(format!("NULL AS {column}"));
        valid.push(format!("typeof({column})='integer'"));
        if *boolean {
            valid.push(format!("{column} IN (0,1)"));
        }
        bytes.push("8".to_owned());
    }
    let valid = format!("({})", valid.join(" AND "));
    let bytes = format!("({})", bytes.join("+"));
    let predicate = format!("{valid} AND {bytes}<=?4");
    let keys = "\"tenancy_operations\".\"installation_id\"=?1 AND \"tenancy_operations\".\"principal_id\"=?2 AND \"tenancy_operations\".\"operation_id\"=?3";
    // CASE covers both false and NULL. UNION ALL retains duplicate identities;
    // LIMIT applies to the entire compound, before any canonical DTO is built.
    // https://sqlite.org/lang_select.html#compound_select_statements
    // https://sqlite.org/lang_expr.html#the_case_expression
    format!(
        "SELECT {},1 AS directory_valid,{bytes} AS directory_bytes,1 AS directory_fits FROM \"{}\" WHERE {keys} AND ({predicate}) UNION ALL SELECT {},CASE WHEN {valid} THEN 1 ELSE 0 END AS directory_valid,{bytes} AS directory_bytes,CASE WHEN {bytes}<=?4 THEN 1 ELSE 0 END AS directory_fits FROM \"{}\" WHERE {keys} AND CASE WHEN {predicate} THEN 0 ELSE 1 END=1 LIMIT 2",
        columns.join(","),
        table.name(),
        absent.join(","),
        table.name(),
    )
}

pub(in crate::tenancy::store) async fn read<T>(
    db: &impl ConnectionTrait,
    query: SelectStatement,
    decode: impl Fn(&QueryResult) -> Result<T>,
) -> Result<Vec<T>> {
    db.query_all(db.get_database_backend().build(&query))
        .await?
        .iter()
        .map(|row| guarded(row, &decode))
        .collect()
}

fn guarded<T>(row: &QueryResult, decode: &impl Fn(&QueryResult) -> Result<T>) -> Result<T> {
    ensure!(
        row.try_get::<i32>("", "directory_valid")? == 1,
        "invalid directory row storage types"
    );
    let _encoded_bytes = usize::try_from(row.try_get::<i64>("", "directory_bytes")?)?;
    if row.try_get::<i32>("", "directory_fits")? != 1 {
        return Err(super::CapacityExceeded.into());
    }
    decode(row)
}

pub(in crate::tenancy::store) async fn one<T>(
    db: &impl ConnectionTrait,
    mut query: SelectStatement,
    decode: impl Fn(&QueryResult) -> Result<T>,
) -> Result<Option<T>> {
    // A second match is corruption, never an arbitrary winner from LIMIT 1.
    query.limit(2);
    one_statement(db, db.get_database_backend().build(&query), decode).await
}

/// The caller supplies a point statement with a whole-query LIMIT 2.
pub(in crate::tenancy::store) async fn one_statement<T>(
    db: &impl ConnectionTrait,
    statement: Statement,
    decode: impl Fn(&QueryResult) -> Result<T>,
) -> Result<Option<T>> {
    let rows = db.query_all(statement).await?;
    // Reject duplicates before creating DTOs: a point decoder reserves one
    // canonical object even though SQL fetches a second row to prove uniqueness.
    ensure!(rows.len() <= 1, "duplicate indexed authority identity");
    rows.first().map(|row| guarded(row, &decode)).transpose()
}

#[cfg(test)]
mod operation_tests;
