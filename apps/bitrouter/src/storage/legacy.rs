//! SQL exclusion for the still-Legacy capture/checkpoint/evolution graph.
//!
//! Keep ownership columns out of public entity/DTO shapes. Existing inserts omit
//! them and migration033 supplies no defaults. Every read and mutation requires
//! all three columns NULL, including malformed partial ownership. This helper
//! grants no owned access and does not derive authority from a legacy owner.

use sea_orm::sea_query::{Alias, Condition, Expr};

pub(crate) fn scope(table: &'static str) -> Condition {
    let mut condition = Condition::all();
    for column in ["installation_id", "tenant_id", "creator_principal_id"] {
        condition = condition.add(Expr::col((Alias::new(table), Alias::new(column))).is_null());
    }
    condition
}

/// Conflict-safe insert without a MySQL self-update of a foreign occupied key.
/// Callers reload through the Legacy predicate and validate their identity.
/// https://dev.mysql.com/doc/refman/8.4/en/innodb-error-handling.html
/// https://www.postgresql.org/docs/current/sql-insert.html#SQL-ON-CONFLICT
/// https://sqlite.org/lang_upsert.html
pub(crate) async fn insert(
    db: &impl sea_orm::ConnectionTrait,
    mut statement: sea_orm::sea_query::InsertStatement,
) -> Result<(), sea_orm::DbErr> {
    use sea_orm::{DatabaseBackend, SqlErr};
    let backend = db.get_database_backend();
    if backend != DatabaseBackend::MySql {
        statement.on_conflict(
            sea_orm::sea_query::OnConflict::new()
                .do_nothing()
                .to_owned(),
        );
    }
    match db.execute(backend.build(&statement)).await {
        Ok(_) => Ok(()),
        Err(error)
            if backend == DatabaseBackend::MySql
                && matches!(error.sql_err(), Some(SqlErr::UniqueConstraintViolation(_))) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(all(test, feature = "core", feature = "harness"))]
pub(crate) mod tests;
