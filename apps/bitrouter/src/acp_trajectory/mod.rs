//! Opt-in, app-owned ACP content capture and canonical session projections.
//!
//! Native IDs remain the harness's authority. Transport replays are retained
//! for audit, separately from live canonical events and model execution costs.

pub mod checkpoint;
pub mod entities;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use bitrouter_sdk::acp::capture::{CaptureError, CaptureEvent, CaptureKind, CapturePort};
use sea_orm::sea_query::{Expr, OnConflict};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter,
    QueryOrder, Set, TransactionTrait,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use entities::{connections, events, sessions};

/// Version of the canonical event interpretation, independent of ACP's version.
pub const CANONICAL_VERSION: u32 = 1;

/// The complete native identity scope; `key()` is only a storage index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIdentity {
    pub owner: String,
    pub source: String,
    pub native_session_id: String,
}

impl SessionIdentity {
    pub fn key(&self) -> Result<String> {
        ensure!(
            !self.owner.is_empty() && !self.source.is_empty() && !self.native_session_id.is_empty(),
            "native identity fields must not be empty"
        );
        Ok(Sha256::digest(serde_json::to_vec(self)?)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect())
    }
}

/// Recording scope and optional, locally established metering namespace.
#[derive(Debug, Clone)]
pub struct RecordingScope {
    pub owner: String,
    pub source: String,
    pub controller_instance_id: Option<String>,
    pub route_scope_id: Option<String>,
}

#[derive(Clone)]
pub struct CanonicalStore {
    db: DatabaseConnection,
}

/// An immutable observed event with a stable connection reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalEvent {
    pub node_id: String,
    pub sequence: i64,
    pub captured_at: String,
    pub event: CaptureEvent,
}

/// Setup evidence whose native identity became known when its response arrived.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupEvidence {
    pub node_id: String,
    pub response_node_id: String,
    pub captured_at: String,
    pub event: CaptureEvent,
}

/// Session-level correlation does not claim a tool-to-model causal join.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestAssociation {
    pub request_id: String,
    pub model_id: String,
    pub provider_id: String,
    pub native_turn_id: Option<String>,
    pub native_agent_thread_id: Option<String>,
    pub basis: String,
    pub charge_micro_usd: Option<i64>,
    /// First local metering observation, not an inferred model start time.
    pub first_metered_at: String,
    pub identity_evidence: Option<serde_json::Value>,
    pub route_events: Vec<crate::trajectory::types::TrajectoryEvent>,
}

#[derive(Debug, Serialize)]
pub struct SessionTranscript {
    pub canonical_version: u32,
    pub session: sessions::Model,
    pub events: Vec<CanonicalEvent>,
    pub setup_evidence: Vec<SetupEvidence>,
    pub replay_events: Vec<events::Model>,
    pub connections: Vec<connections::Model>,
    pub gaps: Vec<String>,
    pub requests: Vec<RequestAssociation>,
    /// Sum over a unique request set. Unknown costs are never coerced to zero.
    pub known_cost_micro_usd: i64,
    pub unpriced_requests: usize,
    pub metering_complete: bool,
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
    store: CanonicalStore,
    connection_id: String,
    scope: RecordingScope,
    state: Mutex<RecorderState>,
}

impl CanonicalStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn recorder(&self, scope: RecordingScope) -> Result<Arc<Recorder>> {
        ensure!(
            !scope.owner.is_empty() && !scope.source.is_empty(),
            "recording scope is empty"
        );
        let connection_id = uuid::Uuid::new_v4().to_string();
        connections::ActiveModel {
            connection_id: Set(connection_id.clone()),
            owner: Set(scope.owner.clone()),
            source: Set(scope.source.clone()),
            controller_instance_id: Set(scope.controller_instance_id.clone()),
            route_scope_id: Set(scope.route_scope_id.clone()),
            state: Set("recording".into()),
            head: Set(0),
            started_at: Set(chrono::Utc::now().to_rfc3339()),
            metadata_json: Set("{}".into()),
        }
        .insert(&self.db)
        .await?;
        Ok(Arc::new(Recorder {
            store: self.clone(),
            connection_id,
            scope,
            state: Mutex::new(RecorderState::default()),
        }))
    }

    pub async fn list(&self, owner: &str, source: &str) -> Result<Vec<sessions::Model>> {
        Ok(sessions::Entity::find()
            .filter(sessions::Column::Deleted.eq(false))
            .filter(sessions::Column::Owner.eq(owner))
            .filter(sessions::Column::Source.eq(source))
            .order_by_asc(sessions::Column::NativeSessionId)
            .all(&self.db)
            .await?)
    }

    pub async fn transcript(&self, identity: &SessionIdentity) -> Result<SessionTranscript> {
        let key = identity.key()?;
        let session = sessions::Entity::find_by_id(&key)
            .one(&self.db)
            .await?
            .context("no recorded ACP session with this native identity")?;
        ensure!(!session.deleted, "recorded ACP content was deleted");
        let rows = events::Entity::find()
            .filter(events::Column::SessionKey.eq(&key))
            .order_by_asc(events::Column::SessionSequence)
            .order_by_asc(events::Column::ConnectionId)
            .order_by_asc(events::Column::Sequence)
            .all(&self.db)
            .await?;
        let mut canonical = Vec::new();
        let mut replay = Vec::new();
        let mut setup_calls = BTreeMap::new();
        let mut connection_ids = BTreeSet::new();
        let mut gaps = BTreeSet::new();
        if session.history_origin != "new" {
            gaps.insert(
                if session.history_origin == "native_id_reused" {
                    "native_session_id_reused_for_new_session"
                } else {
                    "history_before_recording_is_not_verified"
                }
                .to_owned(),
            );
        }
        for row in rows {
            connection_ids.insert(row.connection_id.clone());
            if row.replay {
                replay.push(row);
                continue;
            }
            let event: CaptureEvent = serde_json::from_str(&row.event_json)?;
            if matches!(event.method.as_str(), "session/load" | "session/resume") {
                gaps.insert("history_across_load_or_resume_is_not_verified".to_owned());
            }
            if event.method == "session/new"
                && event.kind == CaptureKind::Response
                && let Some(call) = event.call_id
            {
                setup_calls.insert(
                    (row.connection_id.clone(), call),
                    format!("{}:{}", row.connection_id, row.sequence),
                );
            }
            canonical.push(CanonicalEvent {
                node_id: format!("{}:{}", row.connection_id, row.sequence),
                sequence: row
                    .session_sequence
                    .context("canonical event has no session sequence")?,
                captured_at: row.captured_at,
                event,
            });
        }
        if canonical.len() as u128 != session.head as u128
            || canonical
                .iter()
                .enumerate()
                .any(|(index, node)| node.sequence as u128 != index as u128 + 1)
        {
            gaps.insert("canonical_sequence_gap".into());
        }
        let mut setup_evidence = Vec::new();
        for connection in &connection_ids {
            let unbound = events::Entity::find()
                .filter(events::Column::ConnectionId.eq(connection))
                .filter(events::Column::SessionKey.is_null())
                .all(&self.db)
                .await?;
            for row in unbound {
                let event: CaptureEvent = serde_json::from_str(&row.event_json)?;
                if event.kind == CaptureKind::Request
                    && let Some(call) = event.call_id
                    && let Some(response_node_id) =
                        setup_calls.remove(&(row.connection_id.clone(), call))
                {
                    setup_evidence.push(SetupEvidence {
                        node_id: format!("{}:{}", row.connection_id, row.sequence),
                        response_node_id,
                        captured_at: row.captured_at,
                        event,
                    });
                }
            }
        }
        setup_evidence.sort_by(|left, right| left.node_id.cmp(&right.node_id));
        if !setup_calls.is_empty() {
            gaps.insert("session_setup_evidence_missing".into());
        }
        let mut captured_connections = Vec::new();
        for id in connection_ids {
            let connection = connections::Entity::find_by_id(id)
                .one(&self.db)
                .await?
                .context("missing capture connection")?;
            if connection.state != "complete" {
                gaps.insert(format!(
                    "connection_{}:{}",
                    connection.state, connection.connection_id
                ));
            }
            captured_connections.push(connection);
        }
        let requests = self
            .associated_requests(identity, &captured_connections)
            .await?;
        let mut known_cost_micro_usd = 0_i64;
        let mut unpriced_requests = 0;
        for request in &requests {
            match request.charge_micro_usd {
                Some(cost) => {
                    known_cost_micro_usd = known_cost_micro_usd
                        .checked_add(cost)
                        .context("ACP request cost overflow")?
                }
                None => unpriced_requests += 1,
            }
        }
        let current = sessions::Entity::find_by_id(&key)
            .one(&self.db)
            .await?
            .context("capture session disappeared while reading")?;
        ensure!(
            !current.deleted && current.head == session.head,
            "ACP session changed while reading; retry with the new watermark"
        );
        Ok(SessionTranscript {
            canonical_version: CANONICAL_VERSION,
            session,
            events: canonical,
            setup_evidence,
            replay_events: replay,
            connections: captured_connections,
            gaps: gaps.into_iter().collect(),
            requests,
            known_cost_micro_usd,
            unpriced_requests,
            // ACP cannot prove all hidden/remote requests were metered locally.
            metering_complete: false,
        })
    }

    async fn associated_requests(
        &self,
        identity: &SessionIdentity,
        captured_connections: &[connections::Model],
    ) -> Result<Vec<RequestAssociation>> {
        use crate::metering::entities::requests;
        let mut unique = BTreeMap::new();
        let mut loaded_episodes = BTreeSet::new();
        let mut route_index: HashMap<String, Vec<crate::trajectory::types::TrajectoryEvent>> =
            HashMap::new();
        let ledger = crate::trajectory::store::TrajectoryStore::new(self.db.clone());
        for connection in captured_connections {
            let (Some(controller), Some(principal)) = (
                &connection.controller_instance_id,
                &connection.route_scope_id,
            ) else {
                continue;
            };
            let rows = requests::Entity::find()
                .filter(requests::Column::RouteScopeId.eq(principal))
                .filter(requests::Column::ControllerInstanceId.eq(controller))
                .filter(
                    Condition::any()
                        .add(requests::Column::AcpSessionId.eq(&identity.native_session_id))
                        .add(requests::Column::NativeRootSessionId.eq(&identity.native_session_id))
                        .add(requests::Column::NativeAgentThreadId.eq(&identity.native_session_id)),
                )
                .all(&self.db)
                .await?;
            for row in rows {
                if unique.contains_key(&row.request_id) {
                    continue;
                }
                if let Some(request) = ledger.request(&row.user_id, &row.request_id).await?
                    && loaded_episodes.insert((row.user_id.clone(), request.episode_id.clone()))
                {
                    for event in ledger
                        .events_for_episode(&row.user_id, &request.episode_id)
                        .await?
                    {
                        if let Some(id) = &event.request_id {
                            route_index.entry(id.clone()).or_default().push(event);
                        }
                    }
                }
                let route_events = route_index
                    .get(&row.request_id)
                    .cloned()
                    .unwrap_or_default();
                let charge = match row.charge_status.as_str() {
                    "computed" => Some(row.estimated_charge_micro_usd),
                    "not_charged" => Some(0),
                    _ => None,
                };
                let evidence = row
                    .session_identity_json
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()?;
                unique.insert(
                    row.request_id.clone(),
                    RequestAssociation {
                        request_id: row.request_id,
                        model_id: row.model_id,
                        provider_id: row.provider_id,
                        native_turn_id: row.native_turn_id,
                        native_agent_thread_id: row.native_agent_thread_id,
                        basis: "scoped_declared_session_identity; tool_to_request_unresolved"
                            .into(),
                        charge_micro_usd: charge,
                        first_metered_at: row.created_at,
                        identity_evidence: evidence,
                        route_events,
                    },
                );
            }
        }
        Ok(unique.into_values().collect())
    }

    /// Explicit local content deletion; leaves metering and route ledgers intact.
    pub async fn delete(&self, identity: &SessionIdentity) -> Result<()> {
        let key = identity.key()?;
        let tx = self.db.begin().await?;
        // Fence appenders before deleting any content. Metadata is retained to
        // prevent an outstanding response from recreating the deleted session.
        sessions::Entity::update_many()
            .col_expr(sessions::Column::Deleted, Expr::value(true))
            .filter(sessions::Column::SessionKey.eq(&key))
            .exec(&tx)
            .await?;
        checkpoint::delete_dependents(&tx, &key).await?;
        let rows = events::Entity::find()
            .filter(events::Column::SessionKey.eq(&key))
            .all(&tx)
            .await?;
        let mut setup_calls: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();
        for row in &rows {
            let event: CaptureEvent = serde_json::from_str(&row.event_json)?;
            if let Some(call) = event.call_id {
                setup_calls
                    .entry(row.connection_id.clone())
                    .or_default()
                    .insert(call);
            }
        }
        for (connection, calls) in setup_calls {
            let unbound = events::Entity::find()
                .filter(events::Column::ConnectionId.eq(connection))
                .filter(events::Column::SessionKey.is_null())
                .all(&tx)
                .await?;
            for row in unbound {
                let event: CaptureEvent = serde_json::from_str(&row.event_json)?;
                if event.call_id.is_some_and(|call| calls.contains(&call)) {
                    events::Entity::delete_by_id((row.connection_id, row.sequence))
                        .exec(&tx)
                        .await?;
                }
            }
        }
        events::Entity::delete_many()
            .filter(events::Column::SessionKey.eq(&key))
            .exec(&tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}

impl Recorder {
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
                    .one(&self.store.db)
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
        let tx = self.store.db.begin().await?;
        // The connection row and session head serialize appenders on all supported
        // databases. The event and both cursors commit in the same transaction.
        let bumped = connections::Entity::update_many()
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
        let connection = connections::Entity::find_by_id(&self.connection_id)
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
            sessions::Entity::insert(sessions::ActiveModel {
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
            })
            .on_conflict(
                OnConflict::column(sessions::Column::SessionKey)
                    .do_nothing()
                    .to_owned(),
            )
            .do_nothing()
            .exec(&tx)
            .await?;
            let current = sessions::Entity::find_by_id(&key)
                .one(&tx)
                .await?
                .context("capture session disappeared")?;
            ensure!(
                !current.deleted,
                "recorded ACP content was deleted; reconnect to change recording settings"
            );
            if is_new || forked {
                let prior = events::Entity::find()
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
                    .col_expr(
                        sessions::Column::Head,
                        Expr::col(sessions::Column::Head).add(1),
                    )
                    .filter(sessions::Column::SessionKey.eq(&key))
                    .exec(&tx)
                    .await?;
                session_sequence = Some(
                    sessions::Entity::find_by_id(&key)
                        .one(&tx)
                        .await?
                        .context("capture session disappeared")?
                        .head,
                );
            }
            session_key = Some(key);
        }
        events::ActiveModel {
            connection_id: Set(self.connection_id.clone()),
            sequence: Set(connection.head),
            session_key: Set(session_key),
            session_sequence: Set(session_sequence),
            replay: Set(replay),
            event_json: Set(serde_json::to_string(event)?),
            captured_at: Set(chrono::Utc::now().to_rfc3339()),
        }
        .insert(&tx)
        .await?;
        if event.kind == CaptureKind::Connected {
            connections::Entity::update_many()
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
                .col_expr(connections::Column::State, Expr::value("interrupted"))
                .filter(connections::Column::ConnectionId.eq(&self.connection_id))
                .exec(&self.store.db)
                .await;
            tracing::error!(connection_id = %self.connection_id, %error, "ACP recording interrupted");
            return Err(CaptureError(
                "durable append failed; recording is incomplete".into(),
            ));
        }
        Ok(())
    }
}

/// The common JSON/human CLI surface for local recording operations.
#[derive(Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum RecordingReport {
    Sessions(Vec<sessions::Model>),
    Transcript(Box<SessionTranscript>),
    Deleted(SessionIdentity),
}

impl crate::output::CliReport for RecordingReport {
    fn render(&self, h: &mut crate::output::human::Human<'_>) -> std::io::Result<()> {
        match self {
            Self::Sessions(rows) => {
                for row in rows {
                    h.line(&format!(
                        "{} / {} ({} events, {})",
                        row.source, row.native_session_id, row.head, row.history_origin
                    ))?;
                }
                Ok(())
            }
            Self::Transcript(value) => {
                h.line(&format!(
                    "{} / {}",
                    value.session.source, value.session.native_session_id
                ))?;
                for gap in &value.gaps {
                    h.line(&format!("Gap: {gap}"))?;
                }
                for setup in &value.setup_evidence {
                    h.line(&format!("Setup {}: {}", setup.node_id, setup.event.payload))?;
                }
                for node in &value.events {
                    h.line(&format!(
                        "{} {:?} {} {}",
                        node.sequence, node.event.kind, node.event.method, node.event.payload
                    ))?;
                }
                h.line(&format!(
                    "Observed cost: {} micro-USD; {} unpriced requests",
                    value.known_cost_micro_usd, value.unpriced_requests
                ))
            }
            Self::Deleted(identity) => h.line(&format!(
                "Deleted recorded content: {} / {}",
                identity.source, identity.native_session_id
            )),
        }
    }
}

#[cfg(test)]
mod tests;
