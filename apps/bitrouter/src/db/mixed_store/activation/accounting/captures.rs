//! M0 inventory.begin required a recording under the authenticated controller.
//! This transfer covers closed, lifecycle-only captures with no bound session.
use super::stamp;
use crate::gateway::observability::inventory::{
    COVERAGE_KIND, CoverageRegistration, GatewayRequest, INVENTORY_VERSION,
};
use crate::storage::{
    capture::{CaptureDirection, CaptureEvent, CaptureKind, connections, events},
    evolution::{EvolutionStore, records},
    legacy,
};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, FixedOffset};
use sea_orm::{
    ColumnTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
};
use serde_json::Value;
use std::collections::BTreeMap;

pub(super) struct Capture {
    scope: String,
    controller: String,
    epoch: String,
    registered: DateTime<FixedOffset>,
    connected: DateTime<FixedOffset>,
    disconnected: DateTime<FixedOffset>,
    remaining_revision: i64,
}

pub(super) async fn load(db: &DatabaseConnection) -> Result<BTreeMap<String, Capture>> {
    let mut result = BTreeMap::new();
    let connections = connections::Entity::find().all(db).await?;
    let count = u64::try_from(connections.len())?;
    ensure!(
        events::Entity::find().count(db).await?
            == count.checked_mul(2).context("capture count overflow")?
            && records::Entity::find()
                .filter(records::Column::Kind.eq(COVERAGE_KIND))
                .count(db)
                .await?
                == count
            && connections::Entity::find()
                .filter(legacy::scope("acp_capture_connections"))
                .count(db)
                .await?
                == count,
        "unsupported or incomplete capture inventory"
    );
    for connection in connections {
        let id = &connection.connection_id;
        ensure!(
            connection.state == "complete"
                && connection.head == 2
                && !connection.owner.is_empty()
                && !connection.source.is_empty(),
            "unsealed or session-bearing capture"
        );
        uuid::Uuid::parse_str(id)?;
        let events = events::Entity::find()
            .filter(legacy::scope("acp_capture_events"))
            .filter(events::Column::ConnectionId.eq(id))
            .order_by_asc(events::Column::Sequence)
            .all(db)
            .await?;
        let [open, close] = events.as_slice() else {
            anyhow::bail!("missing capture lifecycle");
        };
        for (row, seq, kind, method) in [
            (open, 1, CaptureKind::Connected, "controller/connect"),
            (close, 2, CaptureKind::Disconnected, "controller/disconnect"),
        ] {
            let event: CaptureEvent = serde_json::from_str(&row.event_json)?;
            ensure!(
                row.sequence == seq
                    && row.session_key.is_none()
                    && row.session_sequence.is_none()
                    && !row.replay
                    && event.kind == kind
                    && event.direction == CaptureDirection::Controller
                    && event.call_id.is_none()
                    && event.method == method
                    && serde_json::to_value(&event)?
                        == serde_json::from_str::<Value>(&row.event_json)?
                    && if seq == 1 {
                        event.payload == serde_json::from_str::<Value>(&connection.metadata_json)?
                    } else {
                        event.payload["clean"] == true
                    },
                "inconsistent capture lifecycle"
            );
        }
        let owner = EvolutionStore::new(db.clone(), &connection.owner)?;
        let row = records::Entity::find_by_id(owner.id(COVERAGE_KIND, id)?)
            .filter(legacy::scope("checkpoint_evolution"))
            .one(db)
            .await?
            .context("missing capture coverage")?;
        let registration: CoverageRegistration = serde_json::from_str(&row.body)?;
        let registered = stamp(&registration.registered_at)?;
        let connected = stamp(&open.captured_at)?;
        let disconnected = stamp(&close.captured_at)?;
        ensure!(
            row.kind == COVERAGE_KIND
                && row.scope_id == owner.scope_id
                && row.session_key.is_none()
                && registration.version == INVENTORY_VERSION
                && registration.connection_id == *id
                && registration.invalid_reason.is_none()
                && serde_json::to_value(&registration)?
                    == serde_json::from_str::<Value>(&row.body)?
                && stamp(&connection.started_at)? <= registered
                && registered <= connected
                && connected <= disconnected,
            "conflicting capture registration"
        );
        uuid::Uuid::parse_str(&registration.runtime_epoch)?;
        result.insert(
            id.clone(),
            Capture {
                scope: connection.route_scope_id.context("unscoped capture")?,
                controller: connection
                    .controller_instance_id
                    .context("capture controller missing")?,
                epoch: registration.runtime_epoch,
                registered,
                connected,
                disconnected,
                remaining_revision: row.revision,
            },
        );
    }
    Ok(result)
}

pub(super) fn admit(
    captures: &mut BTreeMap<String, Capture>,
    request: &GatewayRequest,
) -> Result<()> {
    ensure!(
        !request.anchors.is_empty(),
        "M0 inventory has no recording anchor"
    );
    let admitted = stamp(&request.admitted_at)?;
    for (id, head) in &request.anchors {
        let capture = captures.get_mut(id).context("missing recording anchor")?;
        ensure!(
            capture.scope == request.route_scope_id
                && capture.controller == request.controller_instance_id
                && capture.epoch == request.runtime_epoch
                && match head {
                    0 => capture.registered <= admitted && admitted <= capture.connected,
                    1 => capture.connected <= admitted && admitted <= capture.disconnected,
                    _ => false,
                },
            "capture watermark or request ownership conflicts"
        );
        // initialize starts at zero. begin, settle and terminal each touch every anchor exactly once.
        ensure!(
            capture.remaining_revision >= 3,
            "missing gateway coverage revision"
        );
        capture.remaining_revision -= 3;
    }
    Ok(())
}

pub(super) fn complete(captures: &BTreeMap<String, Capture>) -> bool {
    captures
        .values()
        .all(|capture| capture.remaining_revision == 0)
}
