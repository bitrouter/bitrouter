//! Bounded metadata reads for retained-epoch correlation in the caller's TX.

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseTransaction, FromQueryResult, Statement, Value,
};

use super::{PrefixMatch, SharedTrajectory, check_transaction, request_entity, store};
use crate::trajectory::types::{KeyedDigest, StoredRequest};

const MAX_REQUEST_IDS: usize = 32;
const MAX_PREFIX_DIGESTS: usize = 4096;

impl SharedTrajectory {
    /// Read only requested owner/namespace-scoped rows. At most 32 input IDs
    /// (before deduplication); absent IDs are omitted. No episode/history reads.
    pub async fn requests_in(
        &self,
        tx: &DatabaseTransaction,
        owner: &str,
        ids: &[String],
    ) -> Result<BTreeMap<String, StoredRequest>> {
        let requested = request_keys(owner, ids)?;
        check_transaction(tx).await?;
        if requested.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows = tx
            .query_all(batch_statement(
                &self.namespace,
                owner,
                &requested,
                Lookup::Requests,
            )?)
            .await?;
        validate_row_count(rows.len(), requested.len())?;
        let mut found = BTreeMap::new();
        for row in rows {
            let namespace: String = row.try_get("", "namespace")?;
            let model = request_entity::Model::from_query_result(&row, "")?;
            insert_request(
                &self.namespace,
                owner,
                &requested,
                &mut found,
                &namespace,
                model,
            )?;
        }
        Ok(found)
    }

    /// Read at most 4096 requested keyed digests, before deduplication. Returned
    /// keys preserve the full epoch-qualified digest; missing rows are omitted
    /// and ambiguous rows are explicit. No episode/history queries are issued.
    /// The caller must reject multiple selected episodes and validate the single
    /// selected episode's existence, owner and key epoch before using it.
    pub async fn prefixes_in(
        &self,
        tx: &DatabaseTransaction,
        owner: &str,
        digests: &[KeyedDigest],
    ) -> Result<BTreeMap<String, PrefixMatch>> {
        let requested = prefix_keys(owner, digests)?;
        check_transaction(tx).await?;
        if requested.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows = tx
            .query_all(batch_statement(
                &self.namespace,
                owner,
                &requested,
                Lookup::Prefixes,
            )?)
            .await?;
        validate_row_count(rows.len(), requested.len())?;
        let mut found = BTreeMap::new();
        for row in rows {
            insert_prefix(
                &self.namespace,
                owner,
                &requested,
                &mut found,
                PrefixRow::from_query_result(&row, "")?,
            )?;
        }
        Ok(found)
    }
}

fn request_keys<'a>(owner: &str, ids: &'a [String]) -> Result<BTreeSet<&'a str>> {
    ensure!(
        ids.len() <= MAX_REQUEST_IDS,
        "shared request lookup limit exceeded"
    );
    store::validate_owner(owner)?;
    let mut keys = BTreeSet::new();
    for id in ids {
        store::validate_opaque(id, "shared request lookup identity")?;
        keys.insert(id.as_str());
    }
    Ok(keys)
}

fn prefix_keys<'a>(owner: &str, digests: &'a [KeyedDigest]) -> Result<BTreeSet<&'a str>> {
    ensure!(
        digests.len() <= MAX_PREFIX_DIGESTS,
        "shared prefix lookup limit exceeded"
    );
    store::validate_owner(owner)?;
    Ok(digests.iter().map(KeyedDigest::as_str).collect())
}

#[derive(Clone, Copy)]
enum Lookup {
    Requests,
    Prefixes,
}

fn batch_statement(
    namespace: &str,
    owner: &str,
    keys: &BTreeSet<&str>,
    lookup: Lookup,
) -> Result<Statement> {
    let (select, key, maximum) = match lookup {
        Lookup::Requests => (
            "SELECT namespace, request_id, owner_user_id, episode_id, start_event_id, \
             settlement_event_id, settlement_outbox_id, full_input_digest, native_parent_id, \
             protocol, status FROM gateway_shared_trajectory_requests",
            "request_id",
            MAX_REQUEST_IDS,
        ),
        Lookup::Prefixes => (
            "SELECT namespace, owner_user_id, full_input_digest, episode_id, ambiguous \
             FROM gateway_shared_trajectory_prefix_index",
            "full_input_digest",
            MAX_PREFIX_DIGESTS,
        ),
    };
    ensure!(
        !keys.is_empty() && keys.len() <= maximum,
        "invalid shared lookup batch size"
    );
    // Only fixed SQL identifiers and bounded parameter numbers enter the SQL.
    // All namespace/owner/identity values and the row cap are bound parameters.
    // Predicates retain the namespace/request and namespace/owner/digest keys.
    // https://www.postgresql.org/docs/current/functions-comparisons.html#FUNCTIONS-COMPARISONS-IN-SCALAR
    // https://www.postgresql.org/docs/current/sql-prepare.html
    let placeholders = (3..keys.len() + 3)
        .map(|index| format!("${index}"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "{select} WHERE namespace=$1 AND owner_user_id=$2 AND {key} IN ({placeholders}) ORDER BY {key} LIMIT ${}",
        keys.len() + 3,
    );
    let mut values = Vec::<Value>::with_capacity(keys.len() + 3);
    values.push(namespace.into());
    values.push(owner.into());
    values.extend(keys.iter().map(|key| (*key).into()));
    // One sentinel row detects an unexpected schema/index multiplicity without
    // transferring an unbounded result; normal keys yield at most keys.len().
    values.push(i64::try_from(keys.len() + 1)?.into());
    Ok(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        sql,
        values,
    ))
}

fn validate_row_count(rows: usize, requested: usize) -> Result<()> {
    ensure!(rows <= requested, "shared lookup returned too many rows");
    Ok(())
}

fn validate_membership(
    namespace: &str,
    owner: &str,
    requested: &BTreeSet<&str>,
    row_namespace: &str,
    row_owner: &str,
    key: &str,
) -> Result<()> {
    ensure!(
        row_namespace == namespace && row_owner == owner,
        "shared lookup scope mismatch"
    );
    ensure!(
        requested.contains(key),
        "shared lookup returned an unrequested identity"
    );
    Ok(())
}

fn insert_request(
    namespace: &str,
    owner: &str,
    requested: &BTreeSet<&str>,
    found: &mut BTreeMap<String, StoredRequest>,
    row_namespace: &str,
    row: request_entity::Model,
) -> Result<()> {
    validate_membership(
        namespace,
        owner,
        requested,
        row_namespace,
        &row.owner_user_id,
        &row.request_id,
    )?;
    ensure!(
        !found.contains_key(&row.request_id),
        "duplicate shared request lookup identity"
    );
    for id in [&row.episode_id, &row.start_event_id, &row.protocol]
        .into_iter()
        .chain(row.settlement_event_id.iter())
        .chain(row.settlement_outbox_id.iter())
        .chain(row.native_parent_id.iter())
    {
        store::validate_opaque(id, "shared request lookup row field")?;
    }
    // Existing typed conversion validates the epoch-qualified digest and status.
    let request = store::stored_request(row)?;
    found.insert(request.request_id.clone(), request);
    Ok(())
}

#[derive(FromQueryResult)]
struct PrefixRow {
    namespace: String,
    owner_user_id: String,
    full_input_digest: String,
    episode_id: String,
    ambiguous: bool,
}

fn insert_prefix(
    namespace: &str,
    owner: &str,
    requested: &BTreeSet<&str>,
    found: &mut BTreeMap<String, PrefixMatch>,
    row: PrefixRow,
) -> Result<()> {
    validate_membership(
        namespace,
        owner,
        requested,
        &row.namespace,
        &row.owner_user_id,
        &row.full_input_digest,
    )?;
    ensure!(
        !found.contains_key(&row.full_input_digest),
        "duplicate shared prefix lookup identity"
    );
    KeyedDigest::parse(row.full_input_digest.clone())?;
    store::validate_opaque(&row.episode_id, "shared prefix episode identity")?;
    found.insert(
        row.full_input_digest,
        if row.ambiguous {
            PrefixMatch::Ambiguous
        } else {
            PrefixMatch::Unique(row.episode_id)
        },
    );
    Ok(())
}
