//! Live ACP recording and capture/session mutation authority.
//!
//! Historical projections and checkpoints remain in `acp_trajectory`; only
//! this harness adapter creates recording connections and acknowledges appends.

#[cfg(feature = "core")]
pub(crate) mod owned;

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter, QuerySelect, QueryTrait,
    Set, TransactionTrait,
};
use tokio::sync::Mutex;

use crate::harness::acp::capture::{CaptureError, CapturePort};
use crate::storage::capture::{CaptureEvent, CaptureKind, connections, events, sessions};
use crate::storage::session_identity::SessionIdentity;

/// Recording scope and optional, locally established metering namespace.
#[derive(Debug, Clone)]
pub struct RecordingScope {
    pub owner: String,
    pub source: String,
    pub controller_instance_id: Option<String>,
    pub route_scope_id: Option<String>,
}

#[derive(Clone, Default)]
struct PendingCall {
    request_sequence: i64,
    session_id: Option<String>,
    parent_key: Option<String>,
    parent_watermark: Option<i64>,
}

#[derive(Default)]
struct RecorderState {
    failed: bool,
    calls: HashMap<u64, PendingCall>,
    replaying: HashMap<String, BTreeSet<u64>>,
}

/// One controller connection. Writes serialize before acknowledgment.
pub struct Recorder {
    db: DatabaseConnection,
    connection_id: String,
    scope: RecordingScope,
    state: Mutex<RecorderState>,
}

impl Recorder {
    pub async fn new(db: DatabaseConnection, scope: RecordingScope) -> Result<Arc<Recorder>> {
        ensure!(
            !scope.owner.is_empty() && !scope.source.is_empty(),
            "recording scope is empty"
        );
        let connection_id = uuid::Uuid::new_v4().to_string();
        let inserted = connections::Entity::insert(connections::ActiveModel {
            connection_id: Set(connection_id.clone()),
            owner: Set(scope.owner.clone()),
            source: Set(scope.source.clone()),
            controller_instance_id: Set(scope.controller_instance_id.clone()),
            route_scope_id: Set(scope.route_scope_id.clone()),
            state: Set("recording".into()),
            head: Set(0),
            started_at: Set(chrono::Utc::now().to_rfc3339()),
            metadata_json: Set("{}".into()),
        })
        .exec_without_returning(&db)
        .await?;
        ensure!(inserted == 1, "connections row was not inserted");
        Ok(Arc::new(Recorder {
            db,
            connection_id,
            scope,
            state: Mutex::new(RecorderState::default()),
        }))
    }

    /// Register gateway coverage before forwarding the first captured event.
    pub fn connection_id(&self) -> &str {
        &self.connection_id
    }

    async fn append(&self, event: &CaptureEvent, state: &mut RecorderState) -> Result<()> {
        let mut call = event
            .call_id
            .and_then(|id| state.calls.get(&id))
            .cloned()
            .unwrap_or_default();
        let direct_id = event
            .payload
            .get("sessionId")
            .and_then(serde_json::Value::as_str);
        if event.kind == CaptureKind::Request {
            call.session_id = direct_id.map(str::to_owned);
            if event.method == "session/fork"
                && let Some(id) = direct_id
            {
                let parent = SessionIdentity {
                    owner: self.scope.owner.clone(),
                    source: self.scope.source.clone(),
                    native_session_id: id.to_owned(),
                }
                .key()?;
                call.parent_watermark = sessions::Entity::find_by_id(&parent)
                    .filter(crate::storage::legacy::scope("acp_canonical_sessions"))
                    .one(&self.db)
                    .await?
                    .map(|row| row.head);
                call.parent_key = Some(parent);
            }
        }
        let created_id = if event.kind == CaptureKind::Response {
            event
                .payload
                .get("result")
                .and_then(|value| value.get("sessionId"))
                .and_then(serde_json::Value::as_str)
        } else {
            None
        };
        let session_id = created_id.or(direct_id).or(call.session_id.as_deref());
        let replay = event.kind == CaptureKind::Notification
            && session_id.is_some_and(|id| state.replaying.contains_key(id));
        let tx = self.db.begin().await?;
        // The connection row and session head serialize appenders on all supported
        // databases. The event and both cursors commit in the same transaction.
        let bumped = connections::Entity::update_many()
            .filter(crate::storage::legacy::scope("acp_capture_connections"))
            .col_expr(
                connections::Column::Head,
                Expr::col(connections::Column::Head).add(1),
            )
            .filter(connections::Column::ConnectionId.eq(&self.connection_id))
            .filter(connections::Column::State.eq("recording"))
            .exec(&tx)
            .await?;
        ensure!(
            bumped.rows_affected == 1,
            "capture connection is not writable"
        );
        // Keep RR's first consistent read after acquiring the session row.
        // This connection is already locked by the UPDATE; an ordinary SELECT
        // here would freeze an older snapshot while waiting for that session.
        // https://dev.mysql.com/doc/refman/8.4/en/innodb-consistent-read.html
        let connection = connections::Entity::find_by_id(&self.connection_id)
            .filter(crate::storage::legacy::scope("acp_capture_connections"))
            .lock_exclusive()
            .one(&tx)
            .await?
            .context("capture connection disappeared")?;
        let mut session_key = None;
        let mut session_sequence = None;
        if let Some(native_session_id) = session_id {
            let identity = SessionIdentity {
                owner: self.scope.owner.clone(),
                source: self.scope.source.clone(),
                native_session_id: native_session_id.to_owned(),
            };
            let key = identity.key()?;
            let is_new = event.kind == CaptureKind::Response
                && event.method == "session/new"
                && created_id.is_some();
            let forked = event.kind == CaptureKind::Response
                && event.method == "session/fork"
                && created_id.is_some();
            let statement = sessions::Entity::insert(sessions::ActiveModel {
                session_key: Set(key.clone()),
                owner: Set(identity.owner),
                source: Set(identity.source),
                native_session_id: Set(identity.native_session_id),
                head: Set(0),
                deleted: Set(false),
                history_origin: Set(if is_new { "new" } else { "partial" }.into()),
                parent_key: Set(if forked {
                    call.parent_key.clone()
                } else {
                    None
                }),
                parent_watermark: Set(if forked { call.parent_watermark } else { None }),
            });
            crate::storage::legacy::insert(&tx, statement.into_query()).await?;
            // Serialize session mutations and inspect its current tombstone
            // before establishing a consistent view of the session's events.
            // https://dev.mysql.com/doc/refman/8.4/en/innodb-locking-reads.html
            let current = sessions::Entity::find_by_id(&key)
                .filter(crate::storage::legacy::scope("acp_canonical_sessions"))
                .lock_exclusive()
                .one(&tx)
                .await?
                .context("capture session disappeared")?;
            ensure!(
                !current.deleted,
                "recorded ACP content was deleted; reconnect to change recording settings"
            );
            if is_new || forked {
                // This is the first nonlocking read in the transaction, after
                // the session lock. Earlier writers have committed and future
                // ones must wait on that row. Locking the event range instead
                // would gap-lock unrelated sessions and deadlock their inserts.
                // https://dev.mysql.com/doc/refman/8.4/en/innodb-locking.html#innodb-gap-locks
                let prior = events::Entity::find()
                    .filter(crate::storage::legacy::scope("acp_capture_events"))
                    .filter(events::Column::SessionKey.eq(&key))
                    .filter(
                        Condition::any()
                            .add(events::Column::ConnectionId.ne(&self.connection_id))
                            .add(events::Column::Sequence.lt(call.request_sequence)),
                    )
                    .one(&tx)
                    .await?;
                let origin = if prior.is_some() {
                    "native_id_reused"
                } else if is_new {
                    "new"
                } else {
                    "partial"
                };
                let mut update = sessions::Entity::update_many()
                    .filter(crate::storage::legacy::scope("acp_canonical_sessions"))
                    .col_expr(sessions::Column::HistoryOrigin, Expr::value(origin))
                    .filter(sessions::Column::SessionKey.eq(&key));
                if forked {
                    update = update
                        .col_expr(
                            sessions::Column::ParentKey,
                            Expr::value(call.parent_key.clone()),
                        )
                        .col_expr(
                            sessions::Column::ParentWatermark,
                            Expr::value(call.parent_watermark),
                        );
                }
                update.exec(&tx).await?;
            }
            if !replay {
                sessions::Entity::update_many()
                    .filter(crate::storage::legacy::scope("acp_canonical_sessions"))
                    .col_expr(
                        sessions::Column::Head,
                        Expr::col(sessions::Column::Head).add(1),
                    )
                    .filter(sessions::Column::SessionKey.eq(&key))
                    .exec(&tx)
                    .await?;
                session_sequence = Some(
                    sessions::Entity::find_by_id(&key)
                        .filter(crate::storage::legacy::scope("acp_canonical_sessions"))
                        .one(&tx)
                        .await?
                        .context("capture session disappeared")?
                        .head,
                );
            }
            session_key = Some(key);
        }
        let inserted = events::Entity::insert(events::ActiveModel {
            connection_id: Set(self.connection_id.clone()),
            sequence: Set(connection.head),
            session_key: Set(session_key),
            session_sequence: Set(session_sequence),
            replay: Set(replay),
            event_json: Set(serde_json::to_string(event)?),
            captured_at: Set(chrono::Utc::now().to_rfc3339()),
        })
        .exec_without_returning(&tx)
        .await?;
        ensure!(inserted == 1, "events row was not inserted");
        if event.kind == CaptureKind::Connected {
            connections::Entity::update_many()
                .filter(crate::storage::legacy::scope("acp_capture_connections"))
                .col_expr(
                    connections::Column::MetadataJson,
                    Expr::value(serde_json::to_string(&event.payload)?),
                )
                .filter(connections::Column::ConnectionId.eq(&self.connection_id))
                .exec(&tx)
                .await?;
        }
        if event.kind == CaptureKind::Disconnected {
            let clean = event
                .payload
                .get("clean")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
                && state.calls.is_empty();
            connections::Entity::update_many()
                .filter(crate::storage::legacy::scope("acp_capture_connections"))
                .col_expr(
                    connections::Column::State,
                    Expr::value(if clean { "complete" } else { "interrupted" }),
                )
                .filter(connections::Column::ConnectionId.eq(&self.connection_id))
                .exec(&tx)
                .await?;
        }
        tx.commit().await?;
        if let Some(id) = event.call_id {
            if event.kind == CaptureKind::Request {
                call.request_sequence = connection.head;
                if event.method == "session/load"
                    && let Some(session) = &call.session_id
                {
                    state
                        .replaying
                        .entry(session.clone())
                        .or_default()
                        .insert(id);
                }
                state.calls.insert(id, call);
            } else if event.kind == CaptureKind::Response {
                if let Some(session) = &call.session_id
                    && let Some(active) = state.replaying.get_mut(session)
                {
                    active.remove(&id);
                    if active.is_empty() {
                        state.replaying.remove(session);
                    }
                }
                state.calls.remove(&id);
            }
        }
        Ok(())
    }
}

#[async_trait]
impl CapturePort for Recorder {
    async fn record(&self, event: CaptureEvent) -> Result<(), CaptureError> {
        let mut state = self.state.lock().await;
        if state.failed {
            return Err(CaptureError("connection recording is incomplete".into()));
        }
        if let Err(error) = self.append(&event, &mut state).await {
            state.failed = true;
            // The row starts dirty; even a total database outage or process crash
            // cannot leave an acknowledged complete recording behind.
            let _marked = connections::Entity::update_many()
                .filter(crate::storage::legacy::scope("acp_capture_connections"))
                .col_expr(connections::Column::State, Expr::value("interrupted"))
                .filter(connections::Column::ConnectionId.eq(&self.connection_id))
                .exec(&self.db)
                .await;
            tracing::error!(connection_id = %self.connection_id, %error, "ACP recording interrupted");
            return Err(CaptureError(
                "durable append failed; recording is incomplete".into(),
            ));
        }
        Ok(())
    }
}
