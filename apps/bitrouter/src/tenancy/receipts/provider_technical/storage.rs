//! One optional immutable object, independently bound to the original digest.
use super::super::store::format::Terminal;
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    provider_technical_facts::{MAX_BYTES, ProviderTechnicalFactsV1},
    provider_technical_facts_v2::ProviderTechnicalFactsV2,
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
    #[cfg(test)]
    pub(in crate::tenancy) fn new(facts: &ProviderTechnicalFactsV1) -> Result<Self> {
        facts.validate()?;
        // validate() charged/counts the encoded bound before this allocation.
        let json = serde_json::to_string(facts)?;
        ensure!(json.len() <= MAX_BYTES, "provider technical byte limit");
        Ok(Self {
            version: 1,
            digest: digest(1, &json)?,
            json,
        })
    }
    pub(in crate::tenancy) fn new_v2(facts: &ProviderTechnicalFactsV2) -> Result<Self> {
        facts.validate()?;
        let json = serde_json::to_string(facts)?;
        ensure!(json.len() <= MAX_BYTES, "combined technical byte limit");
        Ok(Self {
            version: 2,
            digest: digest(2, &json)?,
            json,
        })
    }
}
fn digest(version: i32, json: &str) -> Result<String> {
    let mut hash = Sha256::new();
    match version {
        1 => hash.update(b"bitrouter.provider-technical-facts.v1"),
        2 => hash.update(b"bitrouter.provider-technical-facts.v2"),
        _ => anyhow::bail!("unknown optional technical payload"),
    }
    hash.update(u64::try_from(json.len()).unwrap_or(u64::MAX).to_be_bytes());
    hash.update(json.as_bytes());
    Ok(hex::encode(hash.finalize()))
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

enum Payload {
    V1(ProviderTechnicalFactsV1),
    V2(ProviderTechnicalFactsV2),
}
struct Stored {
    prepared: Prepared,
    payload: Payload,
}

async fn read_stored(db: &impl ConnectionTrait, terminal: &Terminal) -> Result<Option<Stored>> {
    crate::db::migration::provider_technical::validate(db).await?;
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
    let expected_digest = digest(version, &json)?;
    ensure!(
        row.try_get::<i64>("", "byte_count")? == i64::try_from(json.len())?
            && row.try_get::<String>("", "receipt_digest")? == terminal.digest()
            && row.try_get::<String>("", "technical_digest")? == expected_digest,
        "provider technical binding/length/digest"
    );
    let payload = match version {
        1 => {
            let value: ProviderTechnicalFactsV1 = serde_json::from_str(&json)?;
            match terminal {
                Terminal::V1(r) => value.validate_against(&r.receipt)?,
                Terminal::V2(r) => value.validate_against_v2(&r.receipt)?,
            }
            Payload::V1(value)
        }
        2 => {
            let value: ProviderTechnicalFactsV2 = serde_json::from_str(&json)?;
            match terminal {
                Terminal::V1(r) => value.validate_against(&r.receipt)?,
                Terminal::V2(r) => value.validate_against_v2(&r.receipt)?,
            }
            Payload::V2(value)
        }
        _ => anyhow::bail!("unsupported optional technical payload"),
    };
    Ok(Some(Stored {
        prepared: Prepared {
            version,
            json,
            digest: expected_digest,
        },
        payload,
    }))
}

/// Existing V1 wire remains an operation-only view. No retained row is changed.
pub(in crate::tenancy) async fn read(
    db: &impl ConnectionTrait,
    terminal: &Terminal,
) -> Result<Option<ProviderTechnicalFactsV1>> {
    Ok(
        match read_stored(db, terminal)
            .await?
            .map(|stored| stored.payload)
        {
            Some(Payload::V1(value)) => Some(value),
            Some(Payload::V2(value)) => value.operation_view(),
            None => None,
        },
    )
}

/// Old payload1 has no observed payload2 scope, so it remains explicit null.
pub(in crate::tenancy) async fn read_v2(
    db: &impl ConnectionTrait,
    terminal: &Terminal,
) -> Result<Option<ProviderTechnicalFactsV2>> {
    Ok(
        match read_stored(db, terminal)
            .await?
            .map(|stored| stored.payload)
        {
            Some(Payload::V2(value)) => Some(value),
            Some(Payload::V1(_)) | None => None,
        },
    )
}

/// Exact validated retained bytes for the original close/readback accounting.
/// A typed compatibility view must never conceal a different stored payload.
pub(in crate::tenancy) async fn read_prepared(
    db: &impl ConnectionTrait,
    terminal: &Terminal,
) -> Result<Option<Prepared>> {
    Ok(read_stored(db, terminal)
        .await?
        .map(|stored| stored.prepared))
}

/// One autocommit statement. It only inserts its exact optional child object;
/// no receipt/usage/archive byte or private disposition is updated. Conflicts
/// never overwrite another byte sequence, nor a completed original receipt.
#[cfg(test)]
pub(in crate::tenancy) async fn insert(
    db: &impl ConnectionTrait,
    terminal: &Terminal,
    prepared: &Prepared,
) -> Result<bool> {
    crate::db::migration::provider_technical::validate(db).await?;
    if let Some(old) = read_prepared(db, terminal).await? {
        ensure!(
            old.version == prepared.version && old.json == prepared.json,
            "conflicting optional provider facts"
        );
        return Ok(false);
    }
    insert_new(db, terminal, prepared).await
}

/// The same one INSERT after an owned caller has positively checked absence.
/// It never overwrites conflicts; callers close/read back any uncertain result.
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
        Expr::cust(format!(
            "coalesce(sum({measured}+{}),0)",
            super::super::capacity::ROW_OVERHEAD
        )),
        Alias::new("bytes"),
    );
    let row = db
        .query_one(backend.build(&query))
        .await?
        .context("technical inventory")?;
    Ok(u64::try_from(row.try_get::<i64>("", "bytes")?)?)
}
