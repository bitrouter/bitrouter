use super::*;
use crate::{
    harness::trajectory::owned::entities::{
        checkpoints, connections, events, members, records, sessions,
    },
    storage::capture::{CaptureDirection as Direction, CaptureEvent, CaptureKind as Kind},
};
use sea_orm::{EntityTrait, QueryOrder, QuerySelect};

// Compare complete stored projections, including exact JSON and timestamps.
// Bounded fixture-owned tables only; never an unqualified production reader.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Graph {
    pub(super) connections: Vec<connections::Model>,
    pub(super) sessions: Vec<sessions::Model>,
    pub(super) events: Vec<events::Model>,
    pub(super) checkpoints: Vec<checkpoints::Model>,
    pub(super) members: Vec<members::Model>,
    pub(super) records: Vec<records::Model>,
}
impl Graph {
    pub(super) async fn read(db: &DatabaseConnection) -> Result<Self> {
        let value = Self {
            connections: connections::Entity::find()
                .order_by_asc(connections::Column::ConnectionId)
                .limit(257)
                .all(db)
                .await?,
            sessions: sessions::Entity::find()
                .order_by_asc(sessions::Column::SessionKey)
                .limit(257)
                .all(db)
                .await?,
            events: events::Entity::find()
                .order_by_asc(events::Column::ConnectionId)
                .order_by_asc(events::Column::Sequence)
                .limit(257)
                .all(db)
                .await?,
            checkpoints: checkpoints::Entity::find()
                .order_by_asc(checkpoints::Column::CheckpointId)
                .limit(257)
                .all(db)
                .await?,
            members: members::Entity::find()
                .order_by_asc(members::Column::CheckpointId)
                .order_by_asc(members::Column::SessionKey)
                .limit(257)
                .all(db)
                .await?,
            records: records::Entity::find()
                .order_by_asc(records::Column::RecordId)
                .limit(257)
                .all(db)
                .await?,
        };
        ensure!(
            [
                value.connections.len(),
                value.sessions.len(),
                value.events.len(),
                value.checkpoints.len(),
                value.members.len(),
                value.records.len()
            ]
            .into_iter()
            .all(|n| n <= 256),
            "capture fixture exceeded finite graph size"
        );
        Ok(value)
    }
    pub(super) fn closed(&self) -> Result<()> {
        ensure!(
            !self.connections.is_empty(),
            "cleanup cannot pass on an empty capture graph"
        );
        for connection in &self.connections {
            let metadata: Value = serde_json::from_str(&connection.metadata_json)?;
            ensure!(metadata["connected"] == true && metadata["closed"] == true);
            ensure!(matches!(
                connection.state.as_str(),
                "complete" | "interrupted"
            ));
            let last = self.last(&connection.connection_id)?;
            let event: CaptureEvent = serde_json::from_str(&last.event_json)?;
            ensure!(last.sequence == connection.head && event.kind == Kind::Disconnected);
        }
        Ok(())
    }
    pub(super) fn tenant(&self, tenant: &str) -> Self {
        Self {
            connections: self
                .connections
                .iter()
                .filter(|r| r.tenant_id.as_deref() == Some(tenant))
                .cloned()
                .collect(),
            sessions: self
                .sessions
                .iter()
                .filter(|r| r.tenant_id.as_deref() == Some(tenant))
                .cloned()
                .collect(),
            events: self
                .events
                .iter()
                .filter(|r| r.tenant_id.as_deref() == Some(tenant))
                .cloned()
                .collect(),
            checkpoints: self
                .checkpoints
                .iter()
                .filter(|r| r.tenant_id.as_deref() == Some(tenant))
                .cloned()
                .collect(),
            members: self
                .members
                .iter()
                .filter(|r| r.tenant_id.as_deref() == Some(tenant))
                .cloned()
                .collect(),
            records: self
                .records
                .iter()
                .filter(|r| r.tenant_id.as_deref() == Some(tenant))
                .cloned()
                .collect(),
        }
    }
    pub(super) fn session(&self, tenant: &str, native: &str) -> Result<&sessions::Model> {
        let mut matching = self
            .sessions
            .iter()
            .filter(|r| r.tenant_id.as_deref() == Some(tenant) && r.native_session_id == native);
        let value = matching.next().context("actual owned session missing")?;
        ensure!(matching.next().is_none(), "ambiguous fixture session");
        Ok(value)
    }
    pub(super) fn last(&self, connection: &str) -> Result<&events::Model> {
        self.events
            .iter()
            .rev()
            .find(|r| r.connection_id == connection)
            .context("actual committed capture event missing")
    }
}

fn event(
    kind: Kind,
    direction: Direction,
    call_id: Option<u64>,
    method: &str,
    payload: Value,
) -> CaptureEvent {
    CaptureEvent {
        direction,
        kind,
        call_id,
        method: method.into(),
        payload,
    }
}

/// This short protocol sequence enters the actual CapturePort callback; the
/// companion facade cases separately prove the real child/SDK reaches it.
pub(super) fn sequence(model: &str) -> Vec<CaptureEvent> {
    vec![
        event(
            Kind::Connected,
            Direction::Controller,
            None,
            "controller/connect",
            json!({}),
        ),
        event(
            Kind::Request,
            Direction::Client,
            Some(1),
            "session/new",
            json!({"cwd":"/fixture"}),
        ),
        event(
            Kind::Response,
            Direction::Agent,
            Some(1),
            "session/new",
            json!({"result":{"sessionId":"actor-native"}}),
        ),
        event(
            Kind::Request,
            Direction::Client,
            Some(2),
            "session/set_config_option",
            json!({"sessionId":"actor-native","configId":"model","value":model}),
        ),
        event(
            Kind::Response,
            Direction::Agent,
            Some(2),
            "session/set_config_option",
            json!({"result":{"configOptions":[{"id":"model","type":"select","currentValue":model,"options":[{"value":model,"name":"Route"}]}]}}),
        ),
        event(
            Kind::Request,
            Direction::Client,
            Some(3),
            "session/prompt",
            json!({"sessionId":"actor-native","prompt":[{"type":"text","text":"backend actor input"}]}),
        ),
        event(
            Kind::Notification,
            Direction::Agent,
            None,
            "session/update",
            json!({"sessionId":"actor-native","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"backend actor output"}}}),
        ),
        event(
            Kind::Response,
            Direction::Agent,
            Some(3),
            "session/prompt",
            json!({"result":{"stopReason":"end_turn"}}),
        ),
    ]
}
