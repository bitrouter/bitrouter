//! Borrowed actor transactions only. No pool, transaction consumer or task here.
use super::codec::{CELL_LIMIT, HttpCrypto, Metadata, Owner, Payload, VERSION, time};
use crate::storage::operation_budget::{Error as BudgetError, OperationBudget};
use bitrouter_ai::tenancy::operation::ErrorCode;
use chrono::{DateTime, Utc};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, QueryResult,
    sea_query::{Alias, Condition, Expr, Order, Query, SimpleExpr},
};

const TABLE: &str = "provider_continuations";
const TEXT_COLUMNS: [(&str, usize); 16] = [
    ("installation_id", 128),
    ("tenant_id", 128),
    ("creator_principal_id", 128),
    ("continuation_identity", 64),
    ("owner_identity", 64),
    ("target_fingerprint", 64),
    ("key_id", 64),
    ("created_at", 27),
    ("expires_at", 27),
    ("purge_after", 27),
    ("publication_state", 11),
    ("publication_generation", 36),
    ("publication_instance_id", 36),
    ("publication_lease_until", 27),
    ("ciphertext", CELL_LIMIT),
    ("nonce", 0),
];
const ROW_BYTES: usize = CELL_LIMIT + 1024;
pub(crate) const BATCH: usize = 32;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("HTTP continuation operation rejected")]
    Rejected(ErrorCode),
    #[error("HTTP continuation state unavailable")]
    Integrity,
    #[error("HTTP continuation storage unavailable")]
    Backend,
}
impl From<BudgetError> for Error {
    fn from(error: BudgetError) -> Self {
        match error {
            BudgetError::Rejected(code) => Self::Rejected(code),
            BudgetError::Capacity => Self::Rejected(ErrorCode::CapacityExceeded),
            BudgetError::Unavailable => Self::Integrity,
        }
    }
}
type Result<T> = std::result::Result<T, Error>;
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub metadata: Metadata,
    pub ciphertext: String,
}
impl Row {
    pub fn seal(crypto: &HttpCrypto, metadata: Metadata, payload: &Payload) -> Result<Self> {
        let ciphertext = crypto
            .seal(&metadata, payload)
            .map_err(|_| Error::Integrity)?;
        Ok(Self {
            metadata,
            ciphertext,
        })
    }
    pub fn open(&self, crypto: &HttpCrypto) -> Result<Payload> {
        crypto
            .open(&self.metadata, &self.ciphertext)
            .map_err(|_| Error::Integrity)
    }
    fn strings(&self) -> [(&'static str, &str); 15] {
        let Metadata {
            installation_id,
            tenant_id,
            creator_principal_id,
            continuation_identity,
            owner_identity,
            target_fingerprint,
            key_id,
            cipher_version: _,
            created_at,
            expires_at,
            purge_after,
            publication_state,
            publication_generation,
            publication_instance_id,
            publication_lease_until,
        } = &self.metadata;
        [
            ("installation_id", installation_id),
            ("tenant_id", tenant_id),
            ("creator_principal_id", creator_principal_id),
            ("continuation_identity", continuation_identity),
            ("owner_identity", owner_identity),
            ("target_fingerprint", target_fingerprint),
            ("key_id", key_id),
            ("created_at", created_at),
            ("expires_at", expires_at),
            ("purge_after", purge_after),
            ("publication_state", publication_state),
            ("publication_generation", publication_generation),
            ("publication_instance_id", publication_instance_id),
            ("publication_lease_until", publication_lease_until),
            ("ciphertext", &self.ciphertext),
        ]
    }
}
pub(crate) enum Resolution {
    Missing,
    Expired,
    Active(Box<Payload>),
}

fn column(name: &str) -> Expr {
    Expr::col(Alias::new(name))
}
pub(super) fn exact(backend: DatabaseBackend, name: &str, value: &str) -> SimpleExpr {
    // Old columns retain their historical collation. This predicate compares
    // their bytes, including case, trailing spaces, ciphertext and timestamps.
    let sql = match backend {
        DatabaseBackend::Sqlite => format!("CAST({name} AS BLOB) = CAST(? AS BLOB)"),
        DatabaseBackend::MySql => format!("CAST({name} AS BINARY) = CAST(? AS BINARY)"),
        DatabaseBackend::Postgres => {
            format!("{name} COLLATE \"C\" = CAST($1 AS TEXT) COLLATE \"C\"")
        }
    };
    Expr::cust_with_values(sql, [value])
}
fn owner_condition(owner: &Owner) -> Condition {
    Condition::all()
        .add(column("installation_id").eq(owner.installation().as_str()))
        .add(column("tenant_id").eq(owner.tenant().as_str()))
        .add(column("creator_principal_id").eq(owner.principal().as_str()))
}
fn snapshot(db: &impl ConnectionTrait, row: &Row) -> Condition {
    let mut condition = Condition::all()
        .add(column("continuation_identity").eq(row.metadata.continuation_identity.as_str()))
        .add(column("cipher_version").eq(row.metadata.cipher_version))
        .add(column("nonce").is_null());
    for (name, value) in row.strings() {
        condition = condition.add(exact(db.get_database_backend(), name, value));
    }
    condition
}
fn select(db: &impl ConnectionTrait) -> sea_orm::sea_query::SelectStatement {
    let mut query = Query::select();
    query.from(Alias::new(TABLE));
    for (name, limit) in TEXT_COLUMNS {
        let length = match db.get_database_backend() {
            DatabaseBackend::Sqlite => format!("length(CAST({name} AS BLOB))"),
            _ => format!("octet_length({name})"),
        };
        // Null and over-limit metadata are distinguished by a separate scalar
        // flag, including nonce; no unbounded cell ever reaches Rust.
        query.expr_as(
            Expr::cust(format!(
                "CASE WHEN {length} <= {limit} THEN {name} ELSE NULL END"
            )),
            Alias::new(name),
        );
        query.expr_as(
            Expr::cust(format!(
                "CASE WHEN {name} IS NULL OR {length} <= {limit} THEN 0 ELSE 1 END"
            )),
            Alias::new(format!("invalid_{name}")),
        );
    }
    query.column(Alias::new("cipher_version"));
    query
}
fn decode(row: QueryResult) -> Result<Row> {
    for (name, _) in TEXT_COLUMNS {
        if row
            .try_get::<i32>("", &format!("invalid_{name}"))
            .map_err(|_| Error::Integrity)?
            != 0
        {
            return Err(Error::Integrity);
        }
    }
    if row
        .try_get::<Option<String>>("", "nonce")
        .map_err(|_| Error::Integrity)?
        .is_some()
    {
        return Err(Error::Integrity);
    }
    let get = |name| {
        row.try_get::<String>("", name)
            .map_err(|_| Error::Integrity)
    };
    let metadata = Metadata {
        installation_id: get("installation_id")?,
        tenant_id: get("tenant_id")?,
        creator_principal_id: get("creator_principal_id")?,
        continuation_identity: get("continuation_identity")?,
        owner_identity: get("owner_identity")?,
        target_fingerprint: get("target_fingerprint")?,
        key_id: get("key_id")?,
        cipher_version: row
            .try_get("", "cipher_version")
            .map_err(|_| Error::Integrity)?,
        created_at: get("created_at")?,
        expires_at: get("expires_at")?,
        purge_after: get("purge_after")?,
        publication_state: get("publication_state")?,
        publication_generation: get("publication_generation")?,
        publication_instance_id: get("publication_instance_id")?,
        publication_lease_until: get("publication_lease_until")?,
    };
    Ok(Row {
        metadata,
        ciphertext: get("ciphertext")?,
    })
}
async fn rows(
    db: &impl ConnectionTrait,
    condition: Condition,
    limit: u64,
    budget: &OperationBudget<'_>,
) -> Result<Vec<Row>> {
    budget.charge(
        usize::try_from(limit).map_err(|_| Error::Integrity)?,
        usize::try_from(limit)
            .map_err(|_| Error::Integrity)?
            .checked_mul(ROW_BYTES)
            .ok_or(Error::Integrity)?,
    )?;
    let mut query = select(db);
    query.cond_where(condition).limit(limit);
    let rows = db
        .query_all(db.get_database_backend().build(&query))
        .await
        .map_err(|_| Error::Backend)?;
    budget.check()?;
    rows.into_iter().map(decode).collect()
}
pub(crate) async fn load(
    db: &impl ConnectionTrait,
    crypto: &HttpCrypto,
    owner: &Owner,
    public: &str,
    budget: &OperationBudget<'_>,
) -> Result<Option<Row>> {
    let id = crypto
        .identity(owner, public)
        .map_err(|_| Error::Rejected(ErrorCode::InvalidRequest))?;
    let condition = owner_condition(owner)
        .add(column("continuation_identity").eq(id.clone()))
        .add(exact(
            db.get_database_backend(),
            "continuation_identity",
            &id,
        ));
    let mut found = rows(db, condition, 2, budget).await?;
    if found.len() > 1 {
        return Err(Error::Integrity);
    }
    let row = found.pop();
    if let Some(row) = &row {
        let payload = row.open(crypto)?;
        if &payload.owner != owner || payload.public_id != public {
            return Err(Error::Integrity);
        }
    }
    Ok(row)
}
pub(crate) async fn resolve_in(
    db: &impl ConnectionTrait,
    crypto: &HttpCrypto,
    owner: &Owner,
    public: &str,
    now: DateTime<Utc>,
    budget: &OperationBudget<'_>,
) -> Result<Resolution> {
    let Some(row) = load(db, crypto, owner, public, budget).await? else {
        return Ok(Resolution::Missing);
    };
    if row.metadata.publication_state != "active" {
        return Ok(Resolution::Missing);
    }
    let payload = row.open(crypto)?;
    if payload.live.is_none()
        || time(&row.metadata.expires_at).map_err(|_| Error::Integrity)? <= now
    {
        return Ok(Resolution::Expired);
    }
    Ok(Resolution::Active(Box::new(payload)))
}
/// The actor has checked for an exact immutable duplicate before insertion.
/// A uniqueness error is returned for supervised rollback, never reread inside
/// an aborted PostgreSQL transaction and never used to replace another owner.
pub(crate) async fn insert_in(
    db: &impl ConnectionTrait,
    row: &Row,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    budget.charge(1, ROW_BYTES)?;
    let strings = row.strings();
    let mut query = Query::insert();
    query.into_table(Alias::new(TABLE)).columns(
        strings
            .iter()
            .map(|(name, _)| Alias::new(*name))
            .chain([Alias::new("cipher_version"), Alias::new("nonce")]),
    );
    query
        .values(
            strings
                .iter()
                .map(|(_, value)| (*value).into())
                .chain([VERSION.into(), Option::<String>::None.into()]),
        )
        .map_err(|_| Error::Integrity)?;
    let result = db
        .execute(db.get_database_backend().build(&query))
        .await
        .map_err(|_| Error::Backend)?;
    budget.check()?;
    if result.rows_affected() != 1 {
        return Err(Error::Integrity);
    }
    Ok(())
}
pub(crate) async fn replace_in(
    db: &impl ConnectionTrait,
    old: &Row,
    new: &Row,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    budget.charge(1, ROW_BYTES)?;
    let mut query = Query::update();
    query.table(Alias::new(TABLE)).cond_where(snapshot(db, old));
    for (name, value) in new.strings() {
        query.value(Alias::new(name), value);
    }
    let result = db
        .execute(db.get_database_backend().build(&query))
        .await
        .map_err(|_| Error::Backend)?;
    budget.check()?;
    if result.rows_affected() != 1 {
        return Err(Error::Integrity);
    }
    Ok(())
}
pub(crate) async fn delete_in(
    db: &impl ConnectionTrait,
    row: &Row,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    budget.charge(1, ROW_BYTES)?;
    let query = Query::delete()
        .from_table(Alias::new(TABLE))
        .cond_where(snapshot(db, row))
        .to_owned();
    let result = db
        .execute(db.get_database_backend().build(&query))
        .await
        .map_err(|_| Error::Backend)?;
    budget.check()?;
    if result.rows_affected() != 1 {
        return Err(Error::Integrity);
    }
    Ok(())
}

/// Each maintenance category has its own finite keyset cursor. No OFFSET and
/// no new durable-row quota. Timestamps are authenticated before mutation.
#[derive(Clone, Default)]
pub(crate) struct Cursor {
    pub after: Option<(String, String)>,
}
#[derive(Clone, Copy)]
pub(crate) enum Scan {
    Stale,
    Expired,
    Purge,
}
impl Scan {
    fn column(self) -> &'static str {
        match self {
            Self::Stale => "publication_lease_until",
            Self::Expired => "expires_at",
            Self::Purge => "purge_after",
        }
    }
}
pub(crate) async fn scan_in(
    db: &impl ConnectionTrait,
    crypto: &HttpCrypto,
    scan: Scan,
    cursor: &Cursor,
    now: DateTime<Utc>,
    budget: &OperationBudget<'_>,
) -> Result<Vec<Row>> {
    budget.charge(BATCH, BATCH * ROW_BYTES)?;
    let name = scan.column();
    let mut query = select(db);
    let mut condition = Condition::all()
        .add(column("installation_id").eq(crypto.installation().as_str()))
        .add(column(name).lte(super::codec::timestamp(now)));
    if matches!(scan, Scan::Stale) {
        condition = condition.add(column("publication_state").ne("active"));
    }
    if let Some((time, id)) = &cursor.after {
        condition = condition.add(
            Condition::any().add(column(name).gt(time.clone())).add(
                Condition::all()
                    .add(column(name).eq(time.clone()))
                    .add(column("continuation_identity").gt(id.clone())),
            ),
        );
    }
    query
        .cond_where(condition)
        .order_by(Alias::new(name), Order::Asc)
        .order_by(Alias::new("continuation_identity"), Order::Asc)
        .limit(BATCH as u64);
    let rows = db
        .query_all(db.get_database_backend().build(&query))
        .await
        .map_err(|_| Error::Backend)?;
    budget.check()?;
    let rows = rows.into_iter().map(decode).collect::<Result<Vec<_>>>()?;
    for row in &rows {
        row.open(crypto)?;
    }
    Ok(rows)
}

#[cfg(test)]
mod query_tests {
    use super::*;
    use anyhow::{Result, ensure};

    #[test]
    fn exact_predicate_binds_values_with_backend_placeholder_numbering() -> Result<()> {
        let value = "Case ' ; -- trailing  ";
        for backend in [
            DatabaseBackend::Sqlite,
            DatabaseBackend::Postgres,
            DatabaseBackend::MySql,
        ] {
            let query = Query::select()
                .expr(Expr::val("preceding parameter"))
                .from(Alias::new(TABLE))
                .and_where(exact(backend, "owner_identity", value))
                .to_owned();
            let statement = backend.build(&query);
            ensure!(!statement.sql.contains(value));
            ensure!(
                statement.values
                    == Some(sea_orm::sea_query::Values(vec![
                        "preceding parameter".into(),
                        value.into()
                    ]))
            );
            if backend == DatabaseBackend::Postgres {
                ensure!(
                    statement.sql.contains("$2") && !statement.sql.contains('?'),
                    "custom PostgreSQL predicate did not bind its value"
                );
            } else {
                ensure!(statement.sql.matches('?').count() == 2);
            }
        }
        Ok(())
    }
}
