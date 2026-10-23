//! Historical ACP session projections and offline checkpoint access.
//!
//! Native IDs remain the harness's authority. Transport replays are retained
//! for audit, separately from live canonical events and model execution costs.

pub mod checkpoint;

#[cfg(test)]
pub(crate) mod fixtures;

#[cfg(all(test, feature = "harness"))]
mod live_tests;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::storage::capture::{CaptureEvent, CaptureKind};
use anyhow::{Context, Result, ensure};
use sea_orm::sea_query::Expr;
use sea_orm::{
    ColumnTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder,
    TransactionTrait,
};
use serde::{Deserialize, Serialize};

use crate::storage::capture::{connections, events, sessions};

/// Version of the canonical event interpretation, independent of ACP's version.
pub const CANONICAL_VERSION: u32 = 1;

use crate::storage::session_identity::SessionIdentity;

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

impl CanonicalStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    /// Seed persisted history in reader tests without granting a production writer.
    #[cfg(test)]
    pub(crate) async fn fixture_history(
        &self,
        scope: fixtures::FixtureScope,
    ) -> Result<std::sync::Arc<fixtures::PersistedHistory>> {
        fixtures::PersistedHistory::new(self.db.clone(), scope).await
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
