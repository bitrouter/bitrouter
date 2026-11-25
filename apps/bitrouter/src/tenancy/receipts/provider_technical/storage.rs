//! One optional immutable object, independently bound to the original digest.
use super::super::store::format::Terminal;
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    provider_technical_facts::{MAX_BYTES, ProviderTechnicalFacts},
    validation::Validate,
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend,
    sea_query::{Alias, Expr, Query, SelectStatement},
};
use sha2::{Digest, Sha256};
const TABLE: &str = "tenancy_provider_technical_facts";

pub(in crate::tenancy) struct Prepared {
    pub version: i32,
    pub json: String,
    pub digest: String,
}
impl Prepared {
    /// Freeze one bounded payload. An invalid observer category is omitted while
    /// retaining the validated operation, pipeline and hop observations.
    pub(in crate::tenancy) fn capture(facts: &ProviderTechnicalFacts) -> Result<Self> {
        let mut retained = facts.clone();
        if retained.validate().is_err() {
            retained.observer_hops.clear();
        }
        retained.validate()?;
        let json = serde_json::to_string(&retained)?;
        ensure!(json.len() <= MAX_BYTES, "provider technical byte limit");
        Ok(Self {
            version: 3,
            digest: digest(&json),
            json,
        })
    }
    #[cfg(test)]
    pub(in crate::tenancy) fn new(facts: &ProviderTechnicalFacts) -> Result<Self> {
        facts.validate()?;
        Self::capture(facts)
    }
}
fn digest(json: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"bitrouter.provider-technical-facts.current\0");
    hash.update(u64::try_from(json.len()).unwrap_or(u64::MAX).to_be_bytes());
    hash.update(json.as_bytes());
    hex::encode(hash.finalize())
}
fn scope(terminal: &Terminal) -> SelectStatement {
    let facts = terminal.facts();
    Query::select()
        .from(Alias::new(TABLE))
        .and_where(Expr::col(Alias::new("installation_id")).eq(facts.installation.as_str()))
        .and_where(Expr::col(Alias::new("tenant_id")).eq(facts.tenant.as_str()))
        .and_where(Expr::col(Alias::new("execution_id")).eq(facts.execution.as_str()))
        .to_owned()
}
fn octets(backend: DatabaseBackend, name: &str) -> String {
    if backend == DatabaseBackend::Sqlite {
        format!("length(CAST({name} AS BLOB))")
    } else {
        format!("octet_length({name})")
    }
}

struct Stored {
    prepared: Prepared,
    payload: ProviderTechnicalFacts,
}

async fn read_stored(db: &impl ConnectionTrait, terminal: &Terminal) -> Result<Option<Stored>> {
    if !crate::db::migration::provider_technical::available(db).await? {
        return Ok(None);
    }
    let backend = db.get_database_backend();
    let mut query = scope(terminal);
    // Actual lengths, not forged byte_count, guard every string BEFORE transfer.
    for (name, max) in [
        ("technical_json", MAX_BYTES),
        ("technical_digest", 64),
        ("receipt_digest", 64),
    ] {
        query.expr_as(
            Expr::cust(format!(
                "CASE WHEN {} BETWEEN 1 AND {max} THEN {name} ELSE NULL END",
                octets(backend, name)
            )),
            Alias::new(name),
        );
    }
    for name in ["schema_version", "byte_count"] {
        let expression = if backend == DatabaseBackend::Sqlite {
            format!("CASE WHEN typeof({name})='integer' THEN {name} ELSE NULL END")
        } else {
            name.to_owned()
        };
        query.expr_as(Expr::cust(expression), Alias::new(name));
    }
    query.limit(2);
    let rows = db.query_all(backend.build(&query)).await?;
    ensure!(rows.len() <= 1, "provider technical multiplicity");
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let json = row.try_get::<String>("", "technical_json")?;
    let version = row.try_get::<i32>("", "schema_version")?;
    ensure!(version == 3, "unsupported provider technical format");
    let expected_digest = digest(&json);
    ensure!(
        row.try_get::<i64>("", "byte_count")? == i64::try_from(json.len())?
            && row.try_get::<String>("", "receipt_digest")? == terminal.digest()
            && row.try_get::<String>("", "technical_digest")? == expected_digest,
        "provider technical binding/length/digest"
    );
    let payload: ProviderTechnicalFacts = serde_json::from_str(&json)?;
    match terminal {
        Terminal::Strict(r) => payload.validate_against(&r.receipt)?,
        Terminal::Ordered(r) => payload.validate_against(&r.receipt)?,
    }
    Ok(Some(Stored {
        prepared: Prepared {
            version,
            json,
            digest: expected_digest,
        },
        payload,
    }))
}

/// Read the immutable technical payload bound to this receipt.
pub(in crate::tenancy) async fn read(
    db: &impl ConnectionTrait,
    terminal: &Terminal,
) -> Result<Option<ProviderTechnicalFacts>> {
    Ok(read_stored(db, terminal)
        .await?
        .map(|stored| stored.payload))
}

/// Exact validated retained bytes for the original close/readback accounting.
/// Typed reads must never conceal a different stored payload.
pub(in crate::tenancy) async fn read_prepared(
    db: &impl ConnectionTrait,
    terminal: &Terminal,
) -> Result<Option<Prepared>> {
    Ok(read_stored(db, terminal)
        .await?
        .map(|stored| stored.prepared))
}

/// Insert a test payload and its capacity delta in the same transaction.
#[cfg(test)]
pub(in crate::tenancy) async fn insert(
    db: &(impl ConnectionTrait + sea_orm::TransactionTrait),
    terminal: &Terminal,
    prepared: &Prepared,
) -> Result<bool> {
    crate::db::migration::provider_technical::validate(db).await?;
    let tx = db.begin().await?;
    let result = async {
        if let Some(old) = read_prepared(&tx, terminal).await? {
            ensure!(
                old.version == prepared.version && old.json == prepared.json,
                "conflicting optional provider facts"
            );
            return Ok(false);
        }
        insert_new(&tx, terminal, prepared).await
    }
    .await;
    match result {
        Ok(inserted) => {
            tx.commit().await?;
            Ok(inserted)
        }
        Err(error) => {
            tx.rollback().await?;
            Err(error)
        }
    }
}

/// The caller retains a transaction through insertion and capacity accounting.
pub(in crate::tenancy) async fn insert_new(
    db: &impl ConnectionTrait,
    terminal: &Terminal,
    prepared: &Prepared,
) -> Result<bool> {
    let facts = terminal.facts();
    let backend = db.get_database_backend();
    let query = Query::insert()
        .into_table(Alias::new(TABLE))
        .columns(
            [
                "installation_id",
                "tenant_id",
                "execution_id",
                "schema_version",
                "receipt_digest",
                "technical_digest",
                "technical_json",
                "byte_count",
            ]
            .map(Alias::new),
        )
        .values([
            facts.installation.as_str().into(),
            facts.tenant.as_str().into(),
            facts.execution.as_str().into(),
            prepared.version.into(),
            terminal.digest().into(),
            prepared.digest.clone().into(),
            prepared.json.clone().into(),
            i64::try_from(prepared.json.len())?.into(),
        ])?
        .to_owned();
    let result = db
        .execute(backend.build(&query))
        .await
        .context("optional provider technical insert")?;
    ensure!(
        result.rows_affected() == 1,
        "optional provider insert acknowledgement"
    );
    let bytes = u64::try_from(prepared.json.len())?
        .checked_add(crate::tenancy::receipts::capacity::ROW_OVERHEAD)
        .context("provider technical accounting overflow")?;
    crate::db::migration::capacity::adjust(
        db,
        crate::db::migration::capacity::Delta {
            technical_bytes: i64::try_from(bytes)?,
            ..Default::default()
        },
    )
    .await?;
    Ok(true)
}

/// Opaque actual storage bytes, including malformed/oversized optional data.
/// This never decodes diagnostics as part of original receipt validation.
pub(in crate::tenancy) async fn retained_bytes(db: &impl ConnectionTrait) -> Result<u64> {
    crate::db::migration::provider_technical::validate(db).await?;
    let backend = db.get_database_backend();
    let measured = octets(backend, "technical_json");
    let mut query = Query::select();
    query.from(Alias::new(TABLE)).expr_as(
        Expr::cust(if backend == DatabaseBackend::Postgres {
            format!(
                "CAST(coalesce(sum({measured}+{}),0) AS BIGINT)",
                super::super::capacity::ROW_OVERHEAD
            )
        } else {
            format!(
                "coalesce(sum({measured}+{}),0)",
                super::super::capacity::ROW_OVERHEAD
            )
        }),
        Alias::new("bytes"),
    );
    let row = db
        .query_one(backend.build(&query))
        .await?
        .context("technical inventory")?;
    Ok(u64::try_from(row.try_get::<i64>("", "bytes")?)?)
}

#[cfg(test)]
mod tests;
