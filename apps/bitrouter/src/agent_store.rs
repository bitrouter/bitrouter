//! App-owned transactional storage for the BRO native execution runtime.

use bitrouter_orchestrator::store::{
    AcceptedKey, ExecutionHead, ExecutionIndexPage, ExecutionOwner, ExecutionPage, ExecutionRecord,
    ExecutionStore, OwnerClaim, RUNTIME_FORMAT_VERSION, StoredExecution, ThreadHistoryChunk,
    owner_time_ms, push_history_event, validate_owner_id, validate_runtime_format,
};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect, Set, TransactionTrait,
};

pub struct DatabaseExecutionStore {
    db: DatabaseConnection,
}

impl DatabaseExecutionStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

mod execution {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "bro_executions")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: String,
        pub version: i64,
        pub format_version: i32,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

mod discovery {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "bro_execution_index")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub position: i64,
        pub execution_id: String,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

mod record {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "bro_execution_records")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub execution_id: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub sequence: i64,
        pub payload: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

mod acceptance_key {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "bro_acceptance_keys")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub scope: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub key: String,
        pub fingerprint: String,
        pub thread_id: String,
        pub turn_id: Option<String>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

mod runtime_ownership {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "bro_runtime_ownership")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: i32,
        pub generation: i64,
        pub server_instance_id: Option<String>,
        pub stopped_at_ms: Option<i64>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

mod runtime_owner {
    use sea_orm::entity::prelude::*;
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "bro_runtime_owners")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub generation: i64,
        pub server_instance_id: String,
        pub stopped_at_ms: Option<i64>,
    }
    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

#[async_trait::async_trait]
impl ExecutionStore for DatabaseExecutionStore {
    async fn read_index(
        &self,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
        max_bytes: usize,
    ) -> Result<ExecutionIndexPage, String> {
        if !(1..=128).contains(&limit) || !(1..=4 * 1024 * 1024).contains(&max_bytes) {
            return Err("invalid execution index bounds".into());
        }
        let tx = self.db.begin().await.map_err(|e| e.to_string())?;
        let head = discovery::Entity::find()
            .order_by_desc(discovery::Column::Position)
            .one(&tx)
            .await
            .map_err(|e| e.to_string())?
            .map_or(Ok(0), |row| {
                u64::try_from(row.position).map_err(|e| e.to_string())
            })?;
        let cutoff = cutoff.unwrap_or(head);
        if after > cutoff || cutoff > head {
            return Err("invalid execution index cutoff".into());
        }
        let rows = discovery::Entity::find()
            .filter(
                discovery::Column::Position.gt(i64::try_from(after).map_err(|e| e.to_string())?),
            )
            .filter(
                discovery::Column::Position.lte(i64::try_from(cutoff).map_err(|e| e.to_string())?),
            )
            .order_by_asc(discovery::Column::Position)
            .limit((limit + 1) as u64)
            .all(&tx)
            .await
            .map_err(|e| e.to_string())?;
        let mut entries = Vec::new();
        let mut bytes = 0_usize;
        let mut more = false;
        for row in rows {
            let root = execution::Entity::find_by_id(&row.execution_id)
                .one(&tx)
                .await
                .map_err(|e| e.to_string())?
                .ok_or("execution index points to missing root")?;
            let value = ExecutionHead {
                format_version: u32::try_from(root.format_version).unwrap_or(0),
                position: u64::try_from(row.position).map_err(|e| e.to_string())?,
                execution_id: row.execution_id,
                version: u64::try_from(root.version).map_err(|e| e.to_string())?,
            };
            let size = serde_json::to_vec(&value).map_err(|e| e.to_string())?.len();
            if entries.len() == limit || bytes.saturating_add(size) > max_bytes {
                if entries.is_empty() {
                    return Err("execution index entry exceeds byte bound".into());
                }
                more = true;
                break;
            }
            bytes += size;
            entries.push(value);
        }
        let next_after = if more {
            entries.last().map(|entry| entry.position)
        } else {
            None
        };
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(ExecutionIndexPage {
            cutoff,
            entries,
            next_after,
        })
    }

    async fn claim_owner(&self, server_instance_id: &str) -> Result<OwnerClaim, String> {
        validate_owner_id(server_instance_id)?;
        let transaction = self.db.begin().await.map_err(|error| error.to_string())?;
        let current = lock_ownership(&transaction).await?;
        if let Some(instance) = &current.server_instance_id {
            let owner = convert_owner(instance.clone(), current.generation, current.stopped_at_ms)?;
            if instance == server_instance_id && owner.stopped_at_ms.is_none() {
                reconcile_execution_index(&transaction).await?;
                transaction
                    .commit()
                    .await
                    .map_err(|error| error.to_string())?;
                return Ok(OwnerClaim::Acquired { owner });
            }
            if instance == server_instance_id || owner.stopped_at_ms.is_none() {
                transaction
                    .commit()
                    .await
                    .map_err(|error| error.to_string())?;
                return Ok(OwnerClaim::Blocked { owner });
            }
        } else {
            if current.generation != 0 || current.stopped_at_ms.is_some() {
                return Err("invalid uninitialized execution owner fence".into());
            }
            if execution::Entity::find()
                .one(&transaction)
                .await
                .map_err(|error| error.to_string())?
                .is_some()
            {
                transaction
                    .commit()
                    .await
                    .map_err(|error| error.to_string())?;
                return Ok(OwnerClaim::Unfenced);
            }
        }
        let generation = current
            .generation
            .checked_add(1)
            .ok_or("execution owner generation exhausted")?;
        runtime_owner::ActiveModel {
            generation: Set(generation),
            server_instance_id: Set(server_instance_id.into()),
            stopped_at_ms: Set(None),
        }
        .insert(&transaction)
        .await
        .map_err(|error| error.to_string())?;
        runtime_ownership::ActiveModel {
            id: Set(1),
            generation: Set(generation),
            server_instance_id: Set(Some(server_instance_id.into())),
            stopped_at_ms: Set(None),
        }
        .update(&transaction)
        .await
        .map_err(|error| error.to_string())?;
        let owner = convert_owner(server_instance_id.into(), generation, None)?;
        reconcile_execution_index(&transaction).await?;
        transaction
            .commit()
            .await
            .map_err(|error| error.to_string())?;
        Ok(OwnerClaim::Acquired { owner })
    }

    async fn read_owner(&self, server_instance_id: &str) -> Result<Option<ExecutionOwner>, String> {
        validate_owner_id(server_instance_id)?;
        runtime_owner::Entity::find()
            .filter(runtime_owner::Column::ServerInstanceId.eq(server_instance_id))
            .one(&self.db)
            .await
            .map_err(|error| error.to_string())?
            .map(|row| convert_owner(row.server_instance_id, row.generation, row.stopped_at_ms))
            .transpose()
    }

    async fn stop_owner(&self, owner: &ExecutionOwner) -> Result<ExecutionOwner, String> {
        let transaction = self.db.begin().await.map_err(|error| error.to_string())?;
        let current = lock_ownership(&transaction).await?;
        if current.server_instance_id.as_deref() != Some(&owner.server_instance_id)
            || u64::try_from(current.generation).map_err(|error| error.to_string())?
                != owner.generation
        {
            return Err("execution owner fence changed".into());
        }
        let stopped = match current.stopped_at_ms {
            Some(stopped) => stopped,
            None => i64::try_from(owner_time_ms()?).map_err(|error| error.to_string())?,
        };
        runtime_ownership::Entity::update_many()
            .col_expr(
                runtime_ownership::Column::StoppedAtMs,
                sea_orm::sea_query::Expr::value(stopped),
            )
            .filter(runtime_ownership::Column::Id.eq(1))
            .exec(&transaction)
            .await
            .map_err(|error| error.to_string())?;
        let updated = runtime_owner::Entity::update_many()
            .col_expr(
                runtime_owner::Column::StoppedAtMs,
                sea_orm::sea_query::Expr::value(stopped),
            )
            .filter(runtime_owner::Column::Generation.eq(current.generation))
            .filter(runtime_owner::Column::ServerInstanceId.eq(&owner.server_instance_id))
            .exec(&transaction)
            .await
            .map_err(|error| error.to_string())?;
        if updated.rows_affected != 1 {
            return Err("execution owner proof is missing".into());
        }
        transaction
            .commit()
            .await
            .map_err(|error| error.to_string())?;
        convert_owner(
            owner.server_instance_id.clone(),
            current.generation,
            Some(stopped),
        )
    }

    async fn commit_owned(
        &self,
        owner: &ExecutionOwner,
        execution_id: &str,
        expected_version: u64,
        records: &[ExecutionRecord],
    ) -> Result<u64, String> {
        self.commit_records(Some(owner), execution_id, expected_version, records)
            .await
    }

    async fn read_records(
        &self,
        execution_id: &str,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
        max_bytes: usize,
    ) -> Result<Option<ExecutionPage>, String> {
        if limit == 0 || limit > 128 || max_bytes == 0 || max_bytes > 4 * 1024 * 1024 {
            return Err("invalid execution page bounds".into());
        }
        let transaction = self.db.begin().await.map_err(|error| error.to_string())?;
        let Some(execution) = execution::Entity::find_by_id(execution_id)
            .one(&transaction)
            .await
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        validate_runtime_format(u32::try_from(execution.format_version).unwrap_or(0))?;
        let version = u64::try_from(execution.version).map_err(|error| error.to_string())?;
        let cutoff = cutoff.unwrap_or(version);
        if after > cutoff || cutoff > version {
            return Err("invalid execution page cutoff".into());
        }
        let end = i64::try_from(cutoff).map_err(|error| error.to_string())?;
        let mut cursor = after;
        let mut records = Vec::new();
        let mut bytes: usize = 0;
        while cursor < cutoff && records.len() < limit {
            let row = record::Entity::find()
                .filter(record::Column::ExecutionId.eq(execution_id))
                .filter(
                    record::Column::Sequence
                        .gt(i64::try_from(cursor).map_err(|error| error.to_string())?),
                )
                .filter(record::Column::Sequence.lte(end))
                .order_by_asc(record::Column::Sequence)
                .limit(1)
                .one(&transaction)
                .await
                .map_err(|error| error.to_string())?
                .ok_or("execution record history is incomplete")?;
            let sequence = u64::try_from(row.sequence).map_err(|error| error.to_string())?;
            if sequence != cursor + 1 {
                return Err("execution record sequence is not contiguous".into());
            }
            if bytes.saturating_add(row.payload.len()) > max_bytes {
                if records.is_empty() {
                    return Err("execution record exceeds page byte bound".into());
                }
                break;
            }
            bytes = bytes.saturating_add(row.payload.len());
            records.push(serde_json::from_str(&row.payload).map_err(|error| error.to_string())?);
            cursor = sequence;
        }
        transaction
            .commit()
            .await
            .map_err(|error| error.to_string())?;
        Ok(Some(ExecutionPage {
            cutoff,
            records,
            next_after: (cursor < cutoff).then_some(cursor),
        }))
    }

    async fn thread_history(
        &self,
        execution_id: &str,
        after: u64,
        cutoff: u64,
        limit: usize,
        max_bytes: usize,
    ) -> Result<ThreadHistoryChunk, String> {
        if limit == 0 || limit > 1000 || max_bytes == 0 || after > cutoff {
            return Err("invalid Thread history bounds".into());
        }
        let cutoff = i64::try_from(cutoff).map_err(|error| error.to_string())?;
        let mut after = i64::try_from(after).map_err(|error| error.to_string())?;
        let transaction = self.db.begin().await.map_err(|error| error.to_string())?;
        let execution = execution::Entity::find_by_id(execution_id)
            .one(&transaction)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("unknown execution")?;
        validate_runtime_format(u32::try_from(execution.format_version).unwrap_or(0))?;
        if cutoff > execution.version {
            return Err("history cutoff is ahead of execution".into());
        }
        let mut events = Vec::new();
        let mut bytes = 0;
        let mut more = false;
        loop {
            // Select only committed public projection rows.
            let mut query = record::Entity::find()
                .filter(record::Column::ExecutionId.eq(execution_id))
                .filter(record::Column::Sequence.gt(after))
                .filter(record::Column::Sequence.lte(cutoff))
                .order_by_asc(record::Column::Sequence);
            query =
                query.filter(record::Column::Payload.starts_with("{\"record\":\"thread_event\","));
            let row = query
                .limit(1)
                .one(&transaction)
                .await
                .map_err(|error| error.to_string())?;
            let Some(row) = row else {
                break;
            };
            if row.payload.len() > max_bytes.saturating_add(128) {
                if !events.is_empty() {
                    more = true;
                    break;
                }
                return Err("Thread history event exceeds page byte bound".into());
            }
            let fact: ExecutionRecord =
                serde_json::from_str(&row.payload).map_err(|error| error.to_string())?;
            let event = match fact {
                ExecutionRecord::ThreadEvent { event } => Some(event),
                _ => return Err("history row is not a Thread event".into()),
            };
            after = row.sequence;
            let Some(event) = event else {
                continue;
            };
            if i64::try_from(event.seq).map_err(|error| error.to_string())? != row.sequence
                || event.thread_id != execution_id
            {
                return Err("Thread event cursor or identity does not match stored row".into());
            }
            if events.len() == limit {
                more = true;
                break;
            }
            if !push_history_event(&mut events, &mut bytes, event, max_bytes)? {
                more = true;
                break;
            }
        }
        transaction
            .commit()
            .await
            .map_err(|error| error.to_string())?;
        Ok(ThreadHistoryChunk { events, more })
    }

    async fn commit(
        &self,
        execution_id: &str,
        expected_version: u64,
        records: &[ExecutionRecord],
    ) -> Result<u64, String> {
        self.commit_records(None, execution_id, expected_version, records)
            .await
    }

    async fn find_key(&self, scope: &str, key: &str) -> Result<Option<AcceptedKey>, String> {
        self.read_key(scope, key).await
    }

    async fn load(&self, execution_id: &str) -> Result<Option<StoredExecution>, String> {
        let transaction = self.db.begin().await.map_err(|error| error.to_string())?;
        let Some(execution) = execution::Entity::find_by_id(execution_id)
            .one(&transaction)
            .await
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        validate_runtime_format(u32::try_from(execution.format_version).unwrap_or(0))?;
        let rows = record::Entity::find()
            .filter(record::Column::ExecutionId.eq(execution_id))
            .order_by_asc(record::Column::Sequence)
            .all(&transaction)
            .await
            .map_err(|error| error.to_string())?;
        let version = u64::try_from(execution.version).map_err(|error| error.to_string())?;
        if u64::try_from(rows.len()).map_err(|error| error.to_string())? != version {
            return Err("execution record history is incomplete".into());
        }
        let records = rows
            .into_iter()
            .enumerate()
            .map(|(offset, row)| {
                if row.sequence != i64::try_from(offset).map_err(|error| error.to_string())? + 1 {
                    return Err("execution record sequence is not contiguous".into());
                }
                serde_json::from_str(&row.payload).map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, String>>()?;
        transaction
            .commit()
            .await
            .map_err(|error| error.to_string())?;
        Ok(Some(StoredExecution {
            format_version: u32::try_from(execution.format_version).unwrap_or(0),
            execution_id: execution_id.into(),
            version,
            records,
        }))
    }
}

fn convert_owner(
    server_instance_id: String,
    generation: i64,
    stopped_at_ms: Option<i64>,
) -> Result<ExecutionOwner, String> {
    validate_owner_id(&server_instance_id)?;
    if generation <= 0 {
        return Err("invalid execution owner generation".into());
    }
    Ok(ExecutionOwner {
        server_instance_id,
        generation: u64::try_from(generation).map_err(|error| error.to_string())?,
        stopped_at_ms: stopped_at_ms
            .map(u64::try_from)
            .transpose()
            .map_err(|error| error.to_string())?,
    })
}

async fn lock_ownership(
    transaction: &sea_orm::DatabaseTransaction,
) -> Result<runtime_ownership::Model, String> {
    runtime_ownership::Entity::update_many()
        .col_expr(
            runtime_ownership::Column::Generation,
            sea_orm::sea_query::Expr::col(runtime_ownership::Column::Generation).into(),
        )
        .filter(runtime_ownership::Column::Id.eq(1))
        .exec(transaction)
        .await
        .map_err(|error| error.to_string())?;
    runtime_ownership::Entity::find_by_id(1)
        .one(transaction)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "runtime ownership control row is missing".into())
}

impl DatabaseExecutionStore {
    async fn commit_records(
        &self,
        owner: Option<&ExecutionOwner>,
        execution_id: &str,
        expected_version: u64,
        records: &[ExecutionRecord],
    ) -> Result<u64, String> {
        if records.is_empty() {
            return Err("empty execution commit".into());
        }
        let expected = i64::try_from(expected_version).map_err(|error| error.to_string())?;
        let count = i64::try_from(records.len()).map_err(|error| error.to_string())?;
        let version = expected
            .checked_add(count)
            .ok_or("execution version exhausted")?;
        let payloads = records
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        let transaction = self.db.begin().await.map_err(|error| error.to_string())?;
        let current = lock_ownership(&transaction).await?;
        match owner {
            Some(owner)
                if current.server_instance_id.as_deref() == Some(&owner.server_instance_id)
                    && current.generation
                        == i64::try_from(owner.generation)
                            .map_err(|error| error.to_string())?
                    && current.stopped_at_ms.is_none()
                    && owner.stopped_at_ms.is_none() => {}
            None if current.server_instance_id.is_none()
                && current.generation == 0
                && current.stopped_at_ms.is_none() => {}
            _ => return Err("execution owner fence changed, stopped or missing".into()),
        }
        if let Some(root) = execution::Entity::find_by_id(execution_id)
            .one(&transaction)
            .await
            .map_err(|e| e.to_string())?
        {
            validate_runtime_format(u32::try_from(root.format_version).unwrap_or(0))?;
        }
        if expected == 0 {
            execution::ActiveModel {
                id: Set(execution_id.into()),
                version: Set(version),
                format_version: Set(
                    i32::try_from(RUNTIME_FORMAT_VERSION).map_err(|e| e.to_string())?
                ),
            }
            .insert(&transaction)
            .await
            .map_err(|error| format!("create execution: {error}"))?;
            discovery::ActiveModel {
                execution_id: Set(execution_id.into()),
                ..Default::default()
            }
            .insert(&transaction)
            .await
            .map_err(|e| format!("create execution index: {e}"))?;
        } else {
            let updated = execution::Entity::update_many()
                .col_expr(
                    execution::Column::Version,
                    sea_orm::sea_query::Expr::value(version),
                )
                .col_expr(
                    execution::Column::FormatVersion,
                    sea_orm::sea_query::Expr::value(
                        i32::try_from(RUNTIME_FORMAT_VERSION).map_err(|e| e.to_string())?,
                    ),
                )
                .filter(execution::Column::Id.eq(execution_id))
                .filter(execution::Column::Version.eq(expected))
                .exec(&transaction)
                .await
                .map_err(|error| error.to_string())?;
            if updated.rows_affected != 1 {
                return Err("execution commit version conflict".into());
            }
        }
        for entry in records.iter().filter_map(|record| match record {
            ExecutionRecord::AcceptedKey { entry } => Some(entry),
            _ => None,
        }) {
            if entry.thread_id != execution_id {
                return Err("acceptance key belongs to a different Thread".into());
            }
            acceptance_key::ActiveModel {
                scope: Set(entry.scope.clone()),
                key: Set(entry.key.clone()),
                fingerprint: Set(entry.fingerprint.clone()),
                thread_id: Set(entry.thread_id.clone()),
                turn_id: Set(entry.turn_id.clone()),
            }
            .insert(&transaction)
            .await
            .map_err(|error| format!("commit acceptance key: {error}"))?;
        }
        for (offset, payload) in payloads.into_iter().enumerate() {
            let offset = i64::try_from(offset).map_err(|error| error.to_string())?;
            record::ActiveModel {
                execution_id: Set(execution_id.into()),
                sequence: Set(expected + offset + 1),
                payload: Set(payload),
            }
            .insert(&transaction)
            .await
            .map_err(|error| error.to_string())?;
        }
        transaction
            .commit()
            .await
            .map_err(|error| error.to_string())?;
        u64::try_from(version).map_err(|error| error.to_string())
    }
}

// Older cooperating writers can add roots after migration backfill but before
// retirement. Reconcile under the same store owner lock before admitting a new
// owner; this never infers effect status or modifies execution facts.
async fn reconcile_execution_index(tx: &sea_orm::DatabaseTransaction) -> Result<(), String> {
    use sea_orm::sea_query::{Expr, Query};
    let existing = Query::select()
        .column(discovery::Column::ExecutionId)
        .from(discovery::Entity)
        .to_owned();
    let select = Query::select()
        .column(execution::Column::Id)
        .from(execution::Entity)
        .and_where(Expr::col(execution::Column::Id).not_in_subquery(existing))
        .order_by(execution::Column::Id, sea_orm::sea_query::Order::Asc)
        .to_owned();
    let mut insert = Query::insert();
    insert
        .into_table(discovery::Entity)
        .columns([discovery::Column::ExecutionId]);
    insert.select_from(select).map_err(|e| e.to_string())?;
    tx.execute(tx.get_database_backend().build(&insert))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

// Keys outlive the evictable runtime projection and are not instance-scoped.
impl DatabaseExecutionStore {
    async fn read_key(&self, scope: &str, key: &str) -> Result<Option<AcceptedKey>, String> {
        let row = acceptance_key::Entity::find()
            .filter(acceptance_key::Column::Scope.eq(scope))
            .filter(acceptance_key::Column::Key.eq(key))
            .one(&self.db)
            .await
            .map_err(|error| error.to_string())?;
        Ok(row.map(|row| AcceptedKey {
            scope: row.scope,
            key: row.key,
            fingerprint: row.fingerprint,
            thread_id: row.thread_id,
            turn_id: row.turn_id,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ownership_fences_independent_connections_and_preserves_stopped_proofs()
    -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let url = format!("sqlite://{}/owners.db", directory.path().display());
        let db = crate::db::connect(&url).await?;
        crate::db::run_migrations(&db).await?;
        let first = DatabaseExecutionStore::new(db);
        let second = DatabaseExecutionStore::new(crate::db::connect(&url).await?);
        let (left, right) = tokio::join!(first.claim_owner("first"), second.claim_owner("second"));
        let left = left.map_err(anyhow::Error::msg)?;
        let right = right.map_err(anyhow::Error::msg)?;
        let owner = match (left, right) {
            (OwnerClaim::Acquired { owner }, OwnerClaim::Blocked { owner: blocked })
            | (OwnerClaim::Blocked { owner: blocked }, OwnerClaim::Acquired { owner }) => {
                assert_eq!(owner, blocked);
                owner
            }
            _ => anyhow::bail!("independent connections did not elect one owner"),
        };
        first
            .commit_owned(&owner, "execution", 0, &[settled()])
            .await
            .map_err(anyhow::Error::msg)?;
        assert!(second.commit("execution", 1, &[settled()]).await.is_err());
        let mut invalid = owner.clone();
        invalid.generation += 1;
        assert!(
            second
                .commit_owned(&invalid, "execution", 1, &[settled()])
                .await
                .is_err()
        );
        assert!(
            second
                .commit_owned(&owner, "execution", 0, &[settled()])
                .await
                .is_err()
        );
        assert_eq!(
            first
                .load("execution")
                .await
                .map_err(anyhow::Error::msg)?
                .ok_or_else(|| anyhow::anyhow!("facts missing"))?
                .version,
            1
        );
        let stopped = first.stop_owner(&owner).await.map_err(anyhow::Error::msg)?;
        assert!(stopped.stopped_at_ms.is_some());
        drop(first);
        drop(second);
        let reopened = DatabaseExecutionStore::new(crate::db::connect(&url).await?);
        assert_eq!(
            reopened
                .read_owner(&owner.server_instance_id)
                .await
                .map_err(anyhow::Error::msg)?,
            Some(stopped.clone())
        );
        let OwnerClaim::Acquired { owner: next } = reopened
            .claim_owner("next")
            .await
            .map_err(anyhow::Error::msg)?
        else {
            anyhow::bail!("stopped owner did not transfer");
        };
        assert_eq!(next.generation, stopped.generation + 1);
        assert!(
            reopened
                .commit_owned(&owner, "execution", 1, &[settled()])
                .await
                .is_err()
        );
        assert!(reopened.stop_owner(&owner).await.is_err());
        reopened
            .commit_owned(&next, "execution", 1, &[settled()])
            .await
            .map_err(anyhow::Error::msg)?;
        assert_eq!(
            reopened
                .read_owner(&owner.server_instance_id)
                .await
                .map_err(anyhow::Error::msg)?,
            Some(stopped)
        );
        reopened
            .stop_owner(&next)
            .await
            .map_err(anyhow::Error::msg)?;
        assert!(
            reopened
                .claim_owner(&owner.server_instance_id)
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn crash_owner_writer() -> anyhow::Result<()> {
        let Ok(url) = std::env::var("BRO_TEST_OWNER_STORE_URL") else {
            return Ok(());
        };
        let store = DatabaseExecutionStore::new(crate::db::connect(&url).await?);
        let OwnerClaim::Acquired { owner } = store
            .claim_owner("crashed-owner")
            .await
            .map_err(anyhow::Error::msg)?
        else {
            anyhow::bail!("child could not claim");
        };
        store
            .commit_owned(&owner, "crashed-owned-execution", 0, &[settled()])
            .await
            .map_err(anyhow::Error::msg)?;
        use std::io::Write;
        println!("BRO_OWNER_COMMITTED");
        std::io::stdout().flush()?;
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        Ok(())
    }

    #[tokio::test]
    async fn abrupt_owner_process_loss_remains_blocked_after_reopen() -> anyhow::Result<()> {
        use tokio::io::AsyncBufReadExt;
        let directory = tempfile::tempdir()?;
        let url = format!("sqlite://{}/owner-crash.db", directory.path().display());
        let db = crate::db::connect(&url).await?;
        crate::db::run_migrations(&db).await?;
        db.close().await?;
        let mut child = tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "agent_store::tests::crash_owner_writer",
                "--nocapture",
            ])
            .env("BRO_TEST_OWNER_STORE_URL", &url)
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("child stdout missing"))?;
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(line) = lines.next_line().await? {
                if line == "BRO_OWNER_COMMITTED" {
                    return Ok::<_, anyhow::Error>(());
                }
            }
            anyhow::bail!("child exited before owned commit acknowledgement")
        })
        .await??;
        let peer = DatabaseExecutionStore::new(crate::db::connect(&url).await?);
        assert!(
            matches!(peer.claim_owner("peer-before-kill").await.map_err(anyhow::Error::msg)?, OwnerClaim::Blocked { owner } if owner.server_instance_id == "crashed-owner" && owner.stopped_at_ms.is_none())
        );
        child.kill().await?;
        let reopened = DatabaseExecutionStore::new(crate::db::connect(&url).await?);
        assert!(
            matches!(reopened.claim_owner("peer-after-kill").await.map_err(anyhow::Error::msg)?, OwnerClaim::Blocked { owner } if owner.server_instance_id == "crashed-owner" && owner.stopped_at_ms.is_none())
        );
        assert!(
            reopened
                .read_owner("peer-after-kill")
                .await
                .map_err(anyhow::Error::msg)?
                .is_none()
        );
        assert_eq!(
            reopened
                .load("crashed-owned-execution")
                .await
                .map_err(anyhow::Error::msg)?
                .ok_or_else(|| anyhow::anyhow!("owned facts lost"))?
                .version,
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn record_pages_preserve_cutoff_and_reject_oversized_or_missing_rows_after_reopen()
    -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let url = format!("sqlite://{}/pages.db", directory.path().display());
        let db = crate::db::connect(&url).await?;
        crate::db::run_migrations(&db).await?;
        let store = DatabaseExecutionStore::new(db.clone());
        let large = ExecutionRecord::TurnQueued {
            turn_id: "turn".into(),
            user_item_id: "user".into(),
            prompt: "x".repeat(2048),
            queue_order: 1,
        };
        store
            .commit("pages", 0, &[settled(), large, settled()])
            .await
            .map_err(anyhow::Error::msg)?;
        let first = store
            .read_records("pages", 0, None, 3, 1000)
            .await
            .map_err(anyhow::Error::msg)?
            .ok_or_else(|| anyhow::anyhow!("missing first page"))?;
        assert_eq!(first.cutoff, 3);
        assert_eq!(first.records.len(), 1);
        assert_eq!(first.next_after, Some(1));
        assert!(
            store
                .read_records("pages", 1, Some(first.cutoff), 3, 1000)
                .await
                .is_err()
        );
        store
            .commit("pages", 3, &[settled()])
            .await
            .map_err(anyhow::Error::msg)?;
        drop(store);
        db.close().await?;
        let reopened_db = crate::db::connect(&url).await?;
        let reopened = DatabaseExecutionStore::new(reopened_db.clone());
        let rest = reopened
            .read_records("pages", 1, Some(first.cutoff), 3, 4096)
            .await
            .map_err(anyhow::Error::msg)?
            .ok_or_else(|| anyhow::anyhow!("missing second page"))?;
        assert_eq!(rest.cutoff, 3);
        assert_eq!(rest.records.len(), 2);
        assert!(rest.next_after.is_none());
        assert!(
            matches!(&rest.records[0], ExecutionRecord::TurnQueued { prompt, .. } if prompt.len() == 2048)
        );
        assert!(
            reopened
                .read_records("pages", 3, Some(3), 3, 4096)
                .await
                .map_err(anyhow::Error::msg)?
                .ok_or_else(|| anyhow::anyhow!("missing empty page"))?
                .records
                .is_empty()
        );
        assert!(
            reopened
                .read_records("pages", 0, Some(5), 3, 4096)
                .await
                .is_err()
        );
        assert!(
            reopened
                .read_records("pages", 0, None, 129, 4096)
                .await
                .is_err()
        );
        record::Entity::delete_by_id(("pages".to_string(), 2))
            .exec(&reopened_db)
            .await?;
        assert!(
            reopened
                .read_records("pages", 1, Some(3), 3, 4096)
                .await
                .is_err()
        );
        Ok(())
    }

    fn history_event(seq: u64, text: &str) -> ExecutionRecord {
        use bitrouter_orchestrator::thread::{ThreadChange, ThreadEvent};
        use bitrouter_orchestrator::turn::TurnReceipt;
        ExecutionRecord::ThreadEvent {
            event: ThreadEvent {
                server_instance_id: "first-process".into(),
                thread_id: "thread".into(),
                seq,
                timestamp_ms: 42,
                changes: vec![ThreadChange::TurnQueued {
                    receipt: TurnReceipt {
                        user_item_id: String::new(),
                        thread_id: "thread".into(),
                        turn_id: format!("turn-{seq}"),
                        queue_order: seq,
                        status: bitrouter_orchestrator::turn::TurnStatus::Queued,
                    },
                    user_item_id: format!("user-{seq}"),
                    prompt: text.into(),
                }],
            },
        }
    }

    #[tokio::test]
    async fn thread_history_pages_use_durable_cutoff_and_count_and_byte_bounds_after_reopen()
    -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let url = format!("sqlite://{}/history.db", directory.path().display());
        let db = crate::db::connect(&url).await?;
        crate::db::run_migrations(&db).await?;
        let store = DatabaseExecutionStore::new(db.clone());
        let first = history_event(2, "first public Item");
        let second = history_event(4, "second public Item");
        store
            .commit("thread", 0, &[settled(), first.clone(), settled(), second])
            .await
            .map_err(anyhow::Error::msg)?;
        let page = store
            .thread_history("thread", 0, 4, 1, 2048)
            .await
            .map_err(anyhow::Error::msg)?;
        assert_eq!(page.events.len(), 1);
        assert!(page.more);
        assert_eq!(page.events[0].seq, 2);
        let single_bytes = serde_json::to_vec(&page.events[0])?.len();
        let byte_page = store
            .thread_history("thread", 0, 4, 100, single_bytes)
            .await
            .map_err(anyhow::Error::msg)?;
        assert_eq!(byte_page.events.len(), 1);
        assert!(byte_page.more);
        assert!(
            store
                .thread_history("thread", 0, 4, 1, single_bytes - 1)
                .await
                .is_err()
        );
        store
            .commit(
                "thread",
                4,
                &[settled(), history_event(6, "later public Item")],
            )
            .await
            .map_err(anyhow::Error::msg)?;
        drop(store);
        db.close().await?;
        let reopened = DatabaseExecutionStore::new(crate::db::connect(&url).await?);
        let last = reopened
            .thread_history("thread", 2, 4, 100, 2048)
            .await
            .map_err(anyhow::Error::msg)?;
        assert_eq!(last.events.len(), 1);
        assert_eq!(last.events[0].seq, 4);
        assert!(!last.more);
        assert_eq!(last.events[0].server_instance_id, "first-process");
        assert!(!serde_json::to_string(&last.events)?.contains("later public Item"));
        assert!(
            reopened
                .thread_history("thread", 4, 4, 1, 2048)
                .await
                .map_err(anyhow::Error::msg)?
                .events
                .is_empty()
        );
        assert!(
            reopened
                .thread_history("thread", 0, 7, 1, 2048)
                .await
                .is_err()
        );
        assert!(
            reopened
                .thread_history("thread", 0, 4, 1001, 2048)
                .await
                .is_err()
        );
        assert!(
            reopened
                .thread_history("missing", 0, 0, 1, 2048)
                .await
                .is_err()
        );
        Ok(())
    }

    fn settled() -> ExecutionRecord {
        ExecutionRecord::Settled {
            outcome: None,
            context_version: 0,
            messages: Vec::new(),
            model_steps: 1,
            tool_calls: 0,
            estimated_spend_microusd: 0,
            active_duration_ms: 1,
        }
    }

    #[tokio::test]
    async fn committed_records_survive_reopen_and_conflicts_do_not_append() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let url = format!("sqlite://{}/runtime.db", directory.path().display());
        let db = crate::db::connect(&url).await?;
        crate::db::run_migrations(&db).await?;
        let store = DatabaseExecutionStore::new(db.clone());
        assert_eq!(
            store
                .commit("execution", 0, &[settled()])
                .await
                .map_err(anyhow::Error::msg)?,
            1
        );
        assert!(store.commit("execution", 0, &[settled()]).await.is_err());
        assert!(store.commit("execution", 7, &[settled()]).await.is_err());
        assert_eq!(
            store
                .commit("execution", 1, &[settled(), settled()])
                .await
                .map_err(anyhow::Error::msg)?,
            3
        );
        drop(store);
        db.close().await?;
        let reopened = DatabaseExecutionStore::new(crate::db::connect(&url).await?);
        let stored = reopened
            .load("execution")
            .await
            .map_err(anyhow::Error::msg)?
            .ok_or_else(|| anyhow::anyhow!("missing execution"))?;
        assert_eq!(stored.version, 3);
        assert_eq!(stored.records.len(), 3);
        Ok(())
    }

    #[tokio::test]
    async fn acceptance_keys_survive_reopen_and_conflicts_roll_back_batches() -> anyhow::Result<()>
    {
        let directory = tempfile::tempdir()?;
        let url = format!("sqlite://{}/keys.db", directory.path().display());
        let db = crate::db::connect(&url).await?;
        crate::db::run_migrations(&db).await?;
        let store = DatabaseExecutionStore::new(db.clone());
        let key = AcceptedKey {
            scope: "caller/thread/enqueue".into(),
            key: "input-1".into(),
            fingerprint: "request-hash".into(),
            thread_id: "thread-1".into(),
            turn_id: Some("turn-1".into()),
        };
        let queued = ExecutionRecord::TurnQueued {
            turn_id: "turn-1".into(),
            user_item_id: "user-1".into(),
            prompt: "first input".into(),
            queue_order: 1,
        };
        assert_eq!(
            store
                .commit(
                    "thread-1",
                    0,
                    &[
                        queued.clone(),
                        ExecutionRecord::AcceptedKey { entry: key.clone() }
                    ]
                )
                .await
                .map_err(anyhow::Error::msg)?,
            2
        );
        assert!(
            store
                .commit(
                    "thread-1",
                    2,
                    &[
                        queued.clone(),
                        ExecutionRecord::AcceptedKey { entry: key.clone() }
                    ]
                )
                .await
                .is_err()
        );
        let conflicting = AcceptedKey {
            thread_id: "thread-2".into(),
            turn_id: Some("turn-2".into()),
            ..key.clone()
        };
        assert!(
            store
                .commit(
                    "thread-2",
                    0,
                    &[queued, ExecutionRecord::AcceptedKey { entry: conflicting }]
                )
                .await
                .is_err()
        );
        assert!(
            store
                .load("thread-2")
                .await
                .map_err(anyhow::Error::msg)?
                .is_none()
        );
        drop(store);
        db.close().await?;
        let reopened = DatabaseExecutionStore::new(crate::db::connect(&url).await?);
        assert_eq!(
            reopened
                .find_key(&key.scope, &key.key)
                .await
                .map_err(anyhow::Error::msg)?,
            Some(key)
        );
        let stored = reopened
            .load("thread-1")
            .await
            .map_err(anyhow::Error::msg)?
            .ok_or_else(|| anyhow::anyhow!("accepted Thread disappeared"))?;
        assert_eq!(stored.version, 2);
        assert_eq!(stored.records.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn crash_record_writer() -> anyhow::Result<()> {
        let Ok(url) = std::env::var("BRO_TEST_EXECUTION_STORE_URL") else {
            return Ok(());
        };
        let store = DatabaseExecutionStore::new(crate::db::connect(&url).await?);
        store
            .commit("crash-execution", 0, &[settled()])
            .await
            .map_err(anyhow::Error::msg)?;
        use std::io::Write;
        println!("BRO_COMMITTED");
        std::io::stdout().flush()?;
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        Ok(())
    }

    #[tokio::test]
    async fn committed_batch_survives_abrupt_process_loss() -> anyhow::Result<()> {
        use tokio::io::AsyncBufReadExt;
        let directory = tempfile::tempdir()?;
        let url = format!("sqlite://{}/crash.db", directory.path().display());
        let db = crate::db::connect(&url).await?;
        crate::db::run_migrations(&db).await?;
        db.close().await?;
        let mut child = tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "agent_store::tests::crash_record_writer",
                "--nocapture",
            ])
            .env("BRO_TEST_EXECUTION_STORE_URL", &url)
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("child stdout missing"))?;
        let mut lines = tokio::io::BufReader::new(stdout).lines();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(line) = lines.next_line().await? {
                if line == "BRO_COMMITTED" {
                    return Ok::<_, anyhow::Error>(());
                }
            }
            anyhow::bail!("child exited before commit acknowledgement")
        })
        .await??;
        child.kill().await?;
        let reopened = DatabaseExecutionStore::new(crate::db::connect(&url).await?);
        let saved = reopened
            .load("crash-execution")
            .await
            .map_err(anyhow::Error::msg)?
            .ok_or_else(|| anyhow::anyhow!("committed batch lost after process kill"))?;
        assert_eq!(saved.version, 1);
        assert!(matches!(
            saved.records.as_slice(),
            [ExecutionRecord::Settled { .. }]
        ));
        Ok(())
    }
}

#[cfg(test)]
mod discovery_tests {
    use super::*;
    use sea_orm_migration::MigratorTrait;

    fn fact() -> ExecutionRecord {
        ExecutionRecord::Settled {
            outcome: None,
            messages: Vec::new(),
            context_version: 0,
            model_steps: 0,
            tool_calls: 0,
            estimated_spend_microusd: 0,
            active_duration_ms: 0,
        }
    }

    #[tokio::test]
    async fn database_discovery_survives_reopen_and_never_indexes_failed_root_transactions()
    -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let url = format!("sqlite://{}/discovery.db", directory.path().display());
        let db = crate::db::connect(&url).await?;
        crate::db::run_migrations(&db).await?;
        let store = DatabaseExecutionStore::new(db);
        let key = AcceptedKey {
            scope: "scope".into(),
            key: "key".into(),
            fingerprint: "one".into(),
            thread_id: "first".into(),
            turn_id: None,
        };
        store
            .commit(
                "first",
                0,
                &[ExecutionRecord::AcceptedKey { entry: key.clone() }, fact()],
            )
            .await
            .map_err(anyhow::Error::msg)?;
        store
            .commit("second", 0, &[fact()])
            .await
            .map_err(anyhow::Error::msg)?;
        let first = store
            .read_index(0, None, 1, 1024)
            .await
            .map_err(anyhow::Error::msg)?;
        let mut conflict = key;
        conflict.thread_id = "failed".into();
        assert!(
            store
                .commit(
                    "failed",
                    0,
                    &[ExecutionRecord::AcceptedKey { entry: conflict }]
                )
                .await
                .is_err()
        );
        store
            .commit("later", 0, &[fact()])
            .await
            .map_err(anyhow::Error::msg)?;
        store
            .commit("second", 1, &[fact()])
            .await
            .map_err(anyhow::Error::msg)?;
        drop(store);
        let reopened = DatabaseExecutionStore::new(crate::db::connect(&url).await?);
        let next = reopened
            .read_index(
                first
                    .next_after
                    .ok_or_else(|| anyhow::anyhow!("continuation missing"))?,
                Some(first.cutoff),
                128,
                4096,
            )
            .await
            .map_err(anyhow::Error::msg)?;
        assert_eq!(next.entries.len(), 1);
        assert_eq!(next.entries[0].execution_id, "second");
        assert_eq!(next.entries[0].version, 2);
        assert!(next.next_after.is_none());
        let all = reopened
            .read_index(0, None, 128, 4096)
            .await
            .map_err(anyhow::Error::msg)?;
        assert_eq!(all.entries.len(), 3);
        assert!(
            !all.entries
                .iter()
                .any(|entry| entry.execution_id == "failed")
        );
        assert!(reopened.read_index(0, None, 1, 1).await.is_err());
        assert!(
            reopened
                .read_index(0, Some(all.cutoff + 1), 1, 1024)
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn migration_and_owner_transfer_cover_roots_from_older_writers() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        let migrations = crate::db::migration::Migrator::migrations();
        let position = migrations
            .iter()
            .position(|migration| migration.name() == "m20240101_000025_create_bro_execution_index")
            .ok_or_else(|| anyhow::anyhow!("index migration missing"))?;
        crate::db::migration::Migrator::up(&db, Some(u32::try_from(position)?)).await?;
        // Emulate the pre-format writer without referencing the new column.
        let insert = sea_orm::sea_query::Query::insert()
            .into_table(execution::Entity)
            .columns([execution::Column::Id, execution::Column::Version])
            .values(["before-migration".into(), 1_i64.into()])?
            .to_owned();
        db.execute(db.get_database_backend().build(&insert)).await?;
        crate::db::migration::Migrator::up(&db, None).await?;
        let old_root = execution::Entity::find_by_id("before-migration")
            .one(&db)
            .await?
            .ok_or_else(|| anyhow::anyhow!("old root missing"))?;
        assert_eq!(old_root.format_version, 0);
        let store = DatabaseExecutionStore::new(db.clone());
        let page = store
            .read_index(0, None, 128, 4096)
            .await
            .map_err(anyhow::Error::msg)?;
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].execution_id, "before-migration");
        // Set up a clean owner proof, then emulate an old writer that knows the
        // root table and owner fence but predates the new discovery index.
        runtime_ownership::Entity::update_many()
            .col_expr(
                runtime_ownership::Column::Generation,
                sea_orm::sea_query::Expr::value(1_i64),
            )
            .col_expr(
                runtime_ownership::Column::ServerInstanceId,
                sea_orm::sea_query::Expr::value("older"),
            )
            .col_expr(
                runtime_ownership::Column::StoppedAtMs,
                sea_orm::sea_query::Expr::value(1_i64),
            )
            .exec(&db)
            .await?;
        runtime_owner::ActiveModel {
            generation: Set(1),
            server_instance_id: Set("older".into()),
            stopped_at_ms: Set(Some(1)),
        }
        .insert(&db)
        .await?;
        execution::ActiveModel {
            format_version: Set(0),
            id: Set("after-migration".into()),
            version: Set(1),
        }
        .insert(&db)
        .await?;
        assert!(matches!(
            store
                .claim_owner("newer")
                .await
                .map_err(anyhow::Error::msg)?,
            OwnerClaim::Acquired { .. }
        ));
        let all = store
            .read_index(0, None, 128, 4096)
            .await
            .map_err(anyhow::Error::msg)?;
        assert_eq!(all.entries.len(), 2);
        assert_eq!(all.entries[0], page.entries[0]);
        assert!(
            all.entries
                .iter()
                .any(|entry| entry.execution_id == "after-migration")
        );
        Ok(())
    }
    #[tokio::test]
    async fn old_runtime_format_is_rejected_before_decoding_and_preserves_owner_and_journal()
    -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::migration::Migrator::up(&db, None).await?;
        let store = DatabaseExecutionStore::new(db.clone());
        let OwnerClaim::Acquired { owner } = store
            .claim_owner("old-owner")
            .await
            .map_err(anyhow::Error::msg)?
        else {
            anyhow::bail!("missing owner");
        };
        execution::ActiveModel {
            id: Set("old-root".into()),
            version: Set(1),
            format_version: Set(0),
        }
        .insert(&db)
        .await?;
        let payload = "{\"record\":\"accepted\",\"unrecognized_old_payload\":true}";
        record::ActiveModel {
            execution_id: Set("old-root".into()),
            sequence: Set(1),
            payload: Set(payload.into()),
        }
        .insert(&db)
        .await?;
        discovery::ActiveModel {
            execution_id: Set("old-root".into()),
            ..Default::default()
        }
        .insert(&db)
        .await?;
        let error = store
            .read_records("old-root", 0, None, 1, 4096)
            .await
            .err()
            .ok_or_else(|| anyhow::anyhow!("old format read"))?;
        assert!(error.contains("unsupported_runtime_format"));
        assert!(
            store
                .thread_history("old-root", 0, 1, 1, 4096)
                .await
                .is_err()
        );
        assert!(store.load("old-root").await.is_err());
        assert!(
            store
                .commit_owned(&owner, "old-root", 1, &[ExecutionRecord::QueueResumed])
                .await
                .err()
                .is_some_and(|error| error.contains("unsupported_runtime_format"))
        );
        assert_eq!(
            store
                .read_owner("old-owner")
                .await
                .map_err(anyhow::Error::msg)?,
            Some(owner)
        );
        let saved = record::Entity::find_by_id(("old-root".to_owned(), 1))
            .one(&db)
            .await?
            .ok_or_else(|| anyhow::anyhow!("lost old payload"))?;
        assert_eq!(saved.payload, payload);
        let heads = store
            .read_index(0, None, 128, 4096)
            .await
            .map_err(anyhow::Error::msg)?;
        assert_eq!(heads.entries[0].format_version, 0);
        Ok(())
    }
}
