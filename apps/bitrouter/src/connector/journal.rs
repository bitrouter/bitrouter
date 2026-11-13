//! Minimal local recovery facts. This store retains identities indefinitely;
//! it stores neither prompt bodies nor commercial fulfillment records.

use std::{collections::BTreeMap, path::Path};

use sea_orm::{
    DatabaseConnection,
    sqlx::{self, Acquire, Row, Sqlite, pool::PoolConnection},
};
use serde::{Deserialize, Serialize};

use super::controller::ControllerError;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    Reserved,
    Sending,
    Running,
    Unknown,
    Succeeded,
    Failed,
    Cancelled,
    Reconciled,
}

impl ExecutionState {
    pub fn is_outstanding(self) -> bool {
        matches!(
            self,
            Self::Reserved | Self::Sending | Self::Running | Self::Unknown
        )
    }
    pub fn is_terminal(self) -> bool {
        !self.is_outstanding()
    }
}

#[derive(Clone, Copy, Debug)]
pub enum TerminalEvidence {
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRecord {
    pub request_id: String,
    pub digest: String,
    pub pool_id: String,
    pub pool_generation: String,
    pub model: String,
    pub member_id: String,
    pub input_tokens: u32,
    pub max_output_tokens: u32,
    /// Absolute Unix milliseconds, never a lease or automatic release timer.
    pub deadline_ms: i64,
    pub attempt_id: String,
    pub owner_epoch: u64,
    pub state: ExecutionState,
    pub revision: u64,
    pub evidence: Option<String>,
    pub operator_evidence_reference: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

pub(super) struct Journal {
    // The pool's connection lifecycle hook retains the exclusive canonical-file
    // lease even if a cancelled operation outlives its original caller.
    db: DatabaseConnection,
    pub epoch: u64,
    pub paused: bool,
}

impl Journal {
    pub async fn open(
        state_dir: &str,
        pool_id: &str,
        initialize: bool,
    ) -> Result<(Self, BTreeMap<String, ExecutionRecord>), ControllerError> {
        if !cfg!(unix) {
            return Err(ControllerError::UnsupportedHost);
        }
        let dir = Path::new(state_dir);
        if initialize {
            std::fs::create_dir_all(dir).map_err(|_| ControllerError::Storage)?;
        }
        let dir = dir
            .canonicalize()
            .map_err(|_| ControllerError::StateNotInitialized)?;
        let file = dir.join("connector.sqlite3");
        let exists = file.try_exists().map_err(|_| ControllerError::Storage)?;
        let marker = dir.join("connector.initialized");
        if !exists && marker.try_exists().map_err(|_| ControllerError::Storage)? {
            return Err(ControllerError::StateNotInitialized);
        }
        if !exists && !initialize {
            return Err(ControllerError::StateNotInitialized);
        }
        let lease = crate::tenancy::ownership::acquire_path(&file, initialize)
            .map_err(|_| ControllerError::OwnershipUnavailable)?;
        let url = format!(
            "sqlite://{}?mode={}",
            lease.path.display(),
            if exists { "rw" } else { "rwc" }
        );
        let db = crate::db::connect_tenancy(&url, &lease)
            .await
            .map_err(|_| ControllerError::Storage)?;
        // Set journal mode on the only connection before creating any facts.
        let mut conn = db
            .get_sqlite_connection_pool()
            .acquire()
            .await
            .map_err(storage)?;
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode=WAL")
            .fetch_one(&mut *conn)
            .await
            .map_err(storage)?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(ControllerError::Storage);
        }
        drop(conn);
        let mut journal = Self {
            db,
            epoch: 0,
            paused: false,
        };
        let mut conn = journal.connection().await?;
        let mut tx = conn.begin().await.map_err(storage)?;
        if !exists {
            sqlx::query("CREATE TABLE connector_meta (singleton INTEGER PRIMARY KEY CHECK(singleton=1), format_revision INTEGER NOT NULL, pool_id TEXT NOT NULL, owner_epoch INTEGER NOT NULL, paused INTEGER NOT NULL CHECK(paused IN(0,1)))")
                .execute(&mut *tx).await.map_err(storage)?;
            sqlx::query("CREATE TABLE connector_executions (request_id TEXT PRIMARY KEY NOT NULL, payload TEXT NOT NULL)")
                .execute(&mut *tx).await.map_err(storage)?;
            sqlx::query("INSERT INTO connector_meta VALUES (1,1,?,0,0)")
                .bind(pool_id)
                .execute(&mut *tx)
                .await
                .map_err(storage)?;
        }
        let meta = sqlx::query("SELECT format_revision,pool_id,owner_epoch,paused FROM connector_meta WHERE singleton=1")
            .fetch_optional(&mut *tx).await.map_err(storage)?.ok_or(ControllerError::Storage)?;
        if meta.try_get::<i64, _>("format_revision").map_err(storage)? != 1
            || meta.try_get::<String, _>("pool_id").map_err(storage)? != pool_id
        {
            return Err(ControllerError::StoreIdentityMismatch);
        }
        let previous: i64 = meta.try_get("owner_epoch").map_err(storage)?;
        let paused = meta.try_get::<i64, _>("paused").map_err(storage)? == 1;
        let epoch = previous
            .checked_add(1)
            .filter(|value| *value > 0)
            .ok_or(ControllerError::Storage)?;
        let rows = sqlx::query("SELECT request_id,payload FROM connector_executions LIMIT 100001")
            .fetch_all(&mut *tx)
            .await
            .map_err(storage)?;
        if rows.len() > 100_000 {
            return Err(ControllerError::Storage);
        }
        let mut records = BTreeMap::new();
        let now = chrono::Utc::now().timestamp_millis();
        for row in rows {
            let id: String = row.try_get("request_id").map_err(storage)?;
            let payload: String = row.try_get("payload").map_err(storage)?;
            if payload.len() > 8192 {
                return Err(ControllerError::Storage);
            }
            let mut record: ExecutionRecord =
                serde_json::from_str(&payload).map_err(|_| ControllerError::Storage)?;
            if record.request_id != id
                || record.pool_id != pool_id
                || record.owner_epoch == 0
                || record.owner_epoch > previous as u64
                || record.revision == 0
            {
                return Err(ControllerError::StoreIdentityMismatch);
            }
            match record.state {
                ExecutionState::Reserved => {
                    record.state = ExecutionState::Cancelled;
                    record.evidence = Some("restart_confirmed_unsent".into());
                    record.updated_at_ms = now;
                    record.revision = record
                        .revision
                        .checked_add(1)
                        .ok_or(ControllerError::Storage)?;
                }
                ExecutionState::Sending | ExecutionState::Running => {
                    record.state = ExecutionState::Unknown;
                    record.evidence = Some("restart_execution_unresolved".into());
                    record.updated_at_ms = now;
                    record.revision = record
                        .revision
                        .checked_add(1)
                        .ok_or(ControllerError::Storage)?;
                }
                _ => {}
            }
            let encoded = serde_json::to_string(&record).map_err(|_| ControllerError::Storage)?;
            sqlx::query("UPDATE connector_executions SET payload=? WHERE request_id=?")
                .bind(encoded)
                .bind(&id)
                .execute(&mut *tx)
                .await
                .map_err(storage)?;
            records.insert(id, record);
        }
        sqlx::query("UPDATE connector_meta SET owner_epoch=? WHERE singleton=1")
            .bind(epoch)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        tx.commit().await.map_err(storage)?;
        journal.epoch = epoch as u64;
        journal.paused = paused;
        if !marker.try_exists().map_err(|_| ControllerError::Storage)? {
            use std::io::Write;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut marker_file = options
                .open(&marker)
                .map_err(|_| ControllerError::Storage)?;
            marker_file
                .write_all(b"connector-state-v1\n")
                .map_err(|_| ControllerError::Storage)?;
            marker_file
                .sync_all()
                .map_err(|_| ControllerError::Storage)?;
            std::fs::File::open(&dir)
                .and_then(|directory| directory.sync_all())
                .map_err(|_| ControllerError::Storage)?;
        }
        Ok((journal, records))
    }

    /// Set and verify FULL on this checked-out connection, including after a
    /// reconnect. The transaction is subsequently begun on this same handle.
    async fn connection(&self) -> Result<PoolConnection<Sqlite>, ControllerError> {
        let mut conn = self
            .db
            .get_sqlite_connection_pool()
            .acquire()
            .await
            .map_err(storage)?;
        sqlx::query("PRAGMA synchronous=FULL")
            .execute(&mut *conn)
            .await
            .map_err(storage)?;
        let sync: i64 = sqlx::query_scalar("PRAGMA synchronous")
            .fetch_one(&mut *conn)
            .await
            .map_err(storage)?;
        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&mut *conn)
            .await
            .map_err(storage)?;
        if sync != 2 || !mode.eq_ignore_ascii_case("wal") {
            return Err(ControllerError::Storage);
        }
        Ok(conn)
    }

    #[cfg(all(test, unix))]
    pub(super) async fn hold_connection(&self) -> Result<PoolConnection<Sqlite>, ControllerError> {
        self.connection().await
    }

    pub async fn save(
        &self,
        record: &ExecutionRecord,
        insert: bool,
    ) -> Result<(), ControllerError> {
        let mut conn = self.connection().await?;
        let mut tx = conn.begin().await.map_err(storage)?;
        let epoch: i64 =
            sqlx::query_scalar("SELECT owner_epoch FROM connector_meta WHERE singleton=1")
                .fetch_one(&mut *tx)
                .await
                .map_err(storage)?;
        if epoch < 1 || epoch as u64 != self.epoch {
            return Err(ControllerError::OwnershipUnavailable);
        }
        let payload = serde_json::to_string(record).map_err(|_| ControllerError::Storage)?;
        let statement = if insert {
            "INSERT INTO connector_executions (payload,request_id) VALUES (?,?)"
        } else {
            "UPDATE connector_executions SET payload=? WHERE request_id=?"
        };
        let result = sqlx::query(statement)
            .bind(payload)
            .bind(&record.request_id)
            .execute(&mut *tx)
            .await
            .map_err(storage)?;
        if result.rows_affected() != 1 {
            return Err(ControllerError::Storage);
        }
        tx.commit().await.map_err(storage)?;
        Ok(())
    }

    pub async fn set_paused(&self, paused: bool) -> Result<(), ControllerError> {
        let mut conn = self.connection().await?;
        let mut tx = conn.begin().await.map_err(storage)?;
        let result =
            sqlx::query("UPDATE connector_meta SET paused=? WHERE singleton=1 AND owner_epoch=?")
                .bind(paused)
                .bind(self.epoch as i64)
                .execute(&mut *tx)
                .await
                .map_err(storage)?;
        if result.rows_affected() != 1 {
            return Err(ControllerError::OwnershipUnavailable);
        }
        tx.commit().await.map_err(storage)?;
        Ok(())
    }
}

fn storage(_: impl std::fmt::Display) -> ControllerError {
    ControllerError::Storage
}
