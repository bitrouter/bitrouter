//! Shared trajectory storage in a caller-owned PostgreSQL transaction.
//!
//! Callers hold the gateway episode fence before domain mutation and roll back
//! the entire transaction on every error. These helpers never commit, retry,
//! acquire gateway locks, or perform external I/O. Local entities are used only
//! as decoded row shapes for the existing pure trajectory validators.

mod delivery;
#[cfg(test)]
pub(crate) mod tests;
mod write;

use anyhow::{Context, Result, ensure};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseTransaction, FromQueryResult, QueryResult, Statement,
    Value,
};

use super::store::{self, episode_entity, event_entity, outbox_entity, request_entity};
use super::types::{KeyedDigest, PendingOutbox, StoredEpisode, StoredRequest, TrajectoryEvent};

pub(crate) const MAX_HISTORY_EVENTS: usize = 4096;
pub(crate) const MAX_HISTORY_BYTES: usize = 16 * 1024 * 1024;
const MAX_OUTBOX_BYTES: usize = 1024 * 1024;

pub(crate) struct SharedTrajectory {
    namespace: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PrefixMatch {
    Missing,
    Unique(String),
    Ambiguous,
}

impl SharedTrajectory {
    pub fn new(namespace: &str) -> Result<Self> {
        ensure!(
            !namespace.is_empty()
                && namespace.len() <= 64
                && namespace
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
            "invalid shared trajectory namespace"
        );
        Ok(Self {
            namespace: namespace.into(),
        })
    }

    pub async fn request_in(
        &self,
        tx: &DatabaseTransaction,
        owner: &str,
        id: &str,
    ) -> Result<Option<StoredRequest>> {
        self.request_row(tx, owner, id)
            .await?
            .map(store::stored_request)
            .transpose()
    }

    pub async fn episode_in(
        &self,
        tx: &DatabaseTransaction,
        owner: &str,
        id: &str,
    ) -> Result<Option<StoredEpisode>> {
        self.episode_row(tx, owner, id)
            .await?
            .map(store::stored_episode)
            .transpose()
    }

    pub async fn history_in(
        &self,
        tx: &DatabaseTransaction,
        owner: &str,
        id: &str,
    ) -> Result<Vec<TrajectoryEvent>> {
        check_transaction(tx).await?;
        let episode = self
            .episode_row(tx, owner, id)
            .await?
            .context("unknown owner-scoped shared episode")?;
        // Check metadata before transferring JSON. Serializable isolation keeps
        // this bound and the following payload read on the same snapshot.
        // https://www.postgresql.org/docs/current/transaction-iso.html
        let sizes = tx.query_all(self.statement(tx,
            "SELECT namespace, octet_length(event_json) AS bytes FROM gateway_shared_trajectory_events
             WHERE namespace=$1 AND owner_user_id=$2 AND episode_id=$3 ORDER BY sequence LIMIT $4",
            vec![owner.into(), id.into(), i64::try_from(MAX_HISTORY_EVENTS + 1)?.into()])?).await?;
        ensure!(
            sizes.len() <= MAX_HISTORY_EVENTS,
            "shared trajectory history event limit exceeded"
        );
        let mut bytes = 0usize;
        for row in &sizes {
            self.check_namespace(row)?;
            bytes = bytes
                .checked_add(usize::try_from(row.try_get::<i32>("", "bytes")?)?)
                .context("shared trajectory history size overflow")?;
            ensure!(
                bytes <= MAX_HISTORY_BYTES,
                "shared trajectory history byte limit exceeded"
            );
        }
        let rows = tx
            .query_all(self.statement(
                tx,
                "SELECT * FROM gateway_shared_trajectory_events
             WHERE namespace=$1 AND owner_user_id=$2 AND episode_id=$3 ORDER BY sequence LIMIT $4",
                vec![
                    owner.into(),
                    id.into(),
                    i64::try_from(MAX_HISTORY_EVENTS + 1)?.into(),
                ],
            )?)
            .await?;
        ensure!(
            rows.len() == sizes.len(),
            "shared trajectory history snapshot changed"
        );
        let events = store::decode_event_history(
            rows.into_iter()
                .map(|row| {
                    let model = self.decode::<event_entity::Model>(row)?;
                    let event: TrajectoryEvent = serde_json::from_str(&model.event_json)?;
                    // Candidate budgets use the same canonical encoding as writes.
                    // Noncanonical stored JSON cannot silently consume extra bytes.
                    ensure!(
                        serde_json::to_string(&event)? == model.event_json,
                        "noncanonical shared trajectory event"
                    );
                    Ok(model)
                })
                .collect::<Result<Vec<_>>>()?,
        )?;
        store::validate_episode_head(&episode, &events)?;
        ensure!(
            events
                .first()
                .is_some_and(|event| event.captured_at == episode.first_captured_at)
                && events
                    .last()
                    .is_some_and(|event| event.captured_at == episode.last_captured_at),
            "shared trajectory head timestamps disagree with history"
        );
        super::health::reduce(&events, &Default::default())?;
        Ok(events)
    }

    pub async fn prefix_in(
        &self,
        tx: &DatabaseTransaction,
        owner: &str,
        digest: &KeyedDigest,
    ) -> Result<PrefixMatch> {
        store::validate_owner(owner)?;
        let row = tx.query_one(self.statement(tx,
            "SELECT namespace, episode_id, ambiguous FROM gateway_shared_trajectory_prefix_index
             WHERE namespace=$1 AND owner_user_id=$2 AND full_input_digest=$3",
            vec![owner.into(), digest.as_str().into()])?).await?;
        match row {
            None => Ok(PrefixMatch::Missing),
            Some(row) => {
                self.check_namespace(&row)?;
                if row.try_get::<bool>("", "ambiguous")? {
                    Ok(PrefixMatch::Ambiguous)
                } else {
                    let id: String = row.try_get("", "episode_id")?;
                    self.episode_row(tx, owner, &id)
                        .await?
                        .context("shared prefix lost its episode")?;
                    Ok(PrefixMatch::Unique(id))
                }
            }
        }
    }

    /// Exact immutable payload lookup, including already delivered items.
    pub async fn outbox_in(
        &self,
        tx: &DatabaseTransaction,
        owner: &str,
        id: &str,
    ) -> Result<Option<PendingOutbox>> {
        validate_scope(owner, id)?;
        let row = tx.query_one(self.statement(tx,
            "SELECT namespace, octet_length(payload_json) AS bytes FROM gateway_shared_trajectory_outbox
             WHERE namespace=$1 AND owner_user_id=$2 AND outbox_id=$3",
            vec![owner.into(), id.into()])?).await?;
        let Some(row) = row else {
            return Ok(None);
        };
        self.check_namespace(&row)?;
        ensure!(
            usize::try_from(row.try_get::<i32>("", "bytes")?)? <= MAX_OUTBOX_BYTES,
            "shared trajectory outbox byte limit exceeded"
        );
        check_transaction(tx).await?;
        let row = tx
            .query_one(self.statement(
                tx,
                "SELECT * FROM gateway_shared_trajectory_outbox
             WHERE namespace=$1 AND owner_user_id=$2 AND outbox_id=$3",
                vec![owner.into(), id.into()],
            )?)
            .await?
            .context("shared trajectory outbox snapshot changed")?;
        store::stored_outbox(self.decode::<outbox_entity::Model>(row)?).map(Some)
    }

    async fn request_row(
        &self,
        tx: &DatabaseTransaction,
        owner: &str,
        id: &str,
    ) -> Result<Option<request_entity::Model>> {
        validate_scope(owner, id)?;
        tx.query_one(self.statement(tx,
            "SELECT * FROM gateway_shared_trajectory_requests WHERE namespace=$1 AND owner_user_id=$2 AND request_id=$3",
            vec![owner.into(), id.into()])?).await?.map(|row| self.decode(row)).transpose()
    }

    async fn episode_row(
        &self,
        tx: &DatabaseTransaction,
        owner: &str,
        id: &str,
    ) -> Result<Option<episode_entity::Model>> {
        validate_scope(owner, id)?;
        tx.query_one(self.statement(tx,
            "SELECT * FROM gateway_shared_trajectory_episodes WHERE namespace=$1 AND owner_user_id=$2 AND episode_id=$3",
            vec![owner.into(), id.into()])?).await?.map(|row| self.decode(row)).transpose()
    }

    fn statement(
        &self,
        tx: &DatabaseTransaction,
        sql: &'static str,
        values: Vec<Value>,
    ) -> Result<Statement> {
        ensure!(
            tx.get_database_backend() == DatabaseBackend::Postgres,
            "shared trajectory requires PostgreSQL"
        );
        let mut scoped = Vec::with_capacity(values.len() + 1);
        scoped.push(self.namespace.clone().into());
        scoped.extend(values);
        Ok(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            sql,
            scoped,
        ))
    }

    fn check_namespace(&self, row: &QueryResult) -> Result<()> {
        ensure!(
            row.try_get::<String>("", "namespace")? == self.namespace,
            "shared trajectory namespace mismatch"
        );
        Ok(())
    }

    fn decode<T: FromQueryResult>(&self, row: QueryResult) -> Result<T> {
        self.check_namespace(&row)?;
        Ok(T::from_query_result(&row, "")?)
    }
}

fn validate_scope(owner: &str, id: &str) -> Result<()> {
    store::validate_owner(owner)?;
    store::validate_opaque(id, "shared trajectory identity")
}

async fn check_transaction(tx: &DatabaseTransaction) -> Result<()> {
    ensure!(
        tx.get_database_backend() == DatabaseBackend::Postgres,
        "shared trajectory requires PostgreSQL"
    );
    let row = tx
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            "SHOW transaction_isolation".to_owned(),
        ))
        .await?
        .context("missing transaction isolation")?;
    ensure!(
        row.try_get::<String>("", "transaction_isolation")? == "serializable",
        "shared trajectory requires Serializable isolation"
    );
    Ok(())
}
