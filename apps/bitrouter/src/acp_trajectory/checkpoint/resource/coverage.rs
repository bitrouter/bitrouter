//! Join the acknowledged gateway inventory to an immutable canonical prefix.

use super::*;
use crate::acp_trajectory::RequestAssociation;
use crate::evolution::inventory::{
    COVERAGE_KIND, CoverageRegistration, GatewayInventory, GatewayRequest, INVENTORY_KIND,
    INVENTORY_VERSION,
};
use crate::evolution::store::EvolutionStore;

pub(super) struct InventoryObservation {
    pub requests: BTreeMap<String, RequestAssociation>,
    pub known_ids: BTreeSet<String>,
    pub unassigned: BTreeSet<String>,
    pub coverage: GatewayCoverage,
    pub connections: BTreeMap<String, connections::Model>,
}

/// No prompt response means there may still be an admitted generation ahead of
/// the frozen prefix. A pause with a completed prompt is a valid boundary even
/// while the controller connection remains open for a later continuation.
fn prompts_closed(content: &CheckpointContent) -> bool {
    let mut open = BTreeSet::new();
    let mut finished = BTreeSet::new();
    for node in content
        .events
        .iter()
        .filter(|node| node.event.method == "session/prompt")
    {
        let Some(call_id) = node.event.call_id else {
            return false;
        };
        let Some((connection, _)) = node.node_id.rsplit_once(':') else {
            return false;
        };
        let key = (connection.to_owned(), call_id);
        let valid = match node.event.kind {
            CaptureKind::Request => !finished.contains(&key) && open.insert(key),
            CaptureKind::Response => open.remove(&key) && finished.insert(key),
            _ => true,
        };
        if !valid {
            return false;
        }
    }
    open.is_empty() && !finished.is_empty()
}

fn belongs(request: &GatewayRequest, segment: &PrefixSegment, own: bool) -> Result<bool> {
    let Some(binding) = &request.binding else {
        return Ok(false);
    };
    if binding.identity != segment.identity || binding.watermark > segment.watermark {
        return Ok(false);
    }
    // An own-session request at the stopped head is still part of that session,
    // including auxiliary calls after prompt completion. A subsequent prompt
    // advances the head before dispatch. Inherited parent segments must retain
    // their timestamp cutoff, so later parent calls cannot leak into a fork.
    Ok(own
        || binding.watermark < segment.watermark
        || at_or_before(&request.admitted_at, &segment.boundary_at)?)
}

fn unresolved_in_prefix(
    request: &GatewayRequest,
    segment: &PrefixSegment,
    own: bool,
    own_successor_at: Option<&str>,
) -> Result<bool> {
    if request.binding.is_some() {
        return Ok(false);
    }
    for captured in &segment.connections {
        let Some(anchor) = request.anchors.get(&captured.connection_id) else {
            continue;
        };
        let sequences: Vec<_> = segment
            .events
            .iter()
            .chain(&segment.setup)
            .filter(|node| node.connection_id == captured.connection_id)
            .map(|node| node.sequence)
            .collect();
        let (Some(first), Some(last)) = (sequences.iter().min(), sequences.iter().max()) else {
            continue;
        };
        if own && anchor >= first {
            // An unresolved request may be post-stop work even when another
            // native session has advanced the shared connection. Keep it unknown
            // until this native session's next content event, not the next event
            // from any session. Later work cannot contaminate an older prefix.
            let before_next = match own_successor_at {
                Some(next) => {
                    chrono::DateTime::parse_from_rfc3339(&request.admitted_at)?
                        < chrono::DateTime::parse_from_rfc3339(next)?
                }
                None => true,
            };
            if before_next {
                return Ok(true);
            }
        }
        if own {
            continue;
        }
        if anchor >= first
            && (anchor < last
                || (anchor == last && at_or_before(&request.admitted_at, &segment.boundary_at)?))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

impl CanonicalStore {
    pub(super) async fn inventory_coverage(
        &self,
        content: &CheckpointContent,
    ) -> Result<InventoryObservation> {
        let cp = &content.checkpoint;
        let own_successor_at = events::Entity::find()
            .filter(events::Column::SessionKey.eq(cp.identity.key()?))
            .filter(events::Column::SessionSequence.gt(cp.watermark))
            .filter(events::Column::Replay.eq(false))
            .order_by_asc(events::Column::SessionSequence)
            .one(&self.db)
            .await?
            .map(|event| event.captured_at);
        let store = EvolutionStore::new(self.db.clone(), &cp.identity.owner)?;
        let mut captured = BTreeMap::new();
        let mut revisions = BTreeMap::new();
        let mut reasons: BTreeSet<String> = cp.gaps.iter().cloned().collect();
        if !prompts_closed(content) {
            reasons.insert("prompt_boundary_incomplete".into());
        }
        let mut namespaces = BTreeSet::new();
        // Read the fences before requests, then recheck them under connection
        // locks when persisting the resource observation.
        for segment in &cp.segments {
            ensure!(
                segment.identity.owner == cp.identity.owner,
                "inherited resource owner changed"
            );
            for connection in &segment.connections {
                let id = &connection.connection_id;
                if captured.contains_key(id) {
                    continue;
                }
                let row = connections::Entity::find_by_id(id)
                    .one(&self.db)
                    .await?
                    .context("capture connection missing")?;
                ensure!(
                    row.owner == cp.identity.owner
                        && row.controller_instance_id == connection.controller_instance_id
                        && row.route_scope_id == connection.route_scope_id,
                    "recording scope changed"
                );
                if row.state != "recording" && row.state != "complete" {
                    reasons.insert(format!("capture_{}:{id}", row.state));
                }
                let registration = store.get::<CoverageRegistration>(COVERAGE_KIND, id).await?;
                revisions.insert(
                    id.clone(),
                    registration.as_ref().map(|(revision, _)| *revision),
                );
                match registration {
                    Some((_, registration))
                        if registration.version == INVENTORY_VERSION
                            && registration.connection_id == *id
                            && registration.invalid_reason.is_none() => {}
                    Some((_, registration)) => {
                        reasons.insert(format!(
                            "coverage_invalid:{id}:{}",
                            registration
                                .invalid_reason
                                .as_deref()
                                .unwrap_or("unsupported_contract")
                        ));
                    }
                    None => {
                        reasons.insert(format!("coverage_unacknowledged:{id}"));
                    }
                }
                match (&row.route_scope_id, &row.controller_instance_id) {
                    (Some(principal), Some(controller)) => {
                        namespaces.insert((principal.clone(), controller.clone()));
                    }
                    _ => {
                        reasons.insert(format!("gateway_namespace_missing:{id}"));
                    }
                }
                captured.insert(id.clone(), row);
            }
        }
        if captured.is_empty() {
            reasons.insert("capture_connections_missing".into());
        }
        let mut inventory = BTreeMap::<String, GatewayRequest>::new();
        for (principal, controller) in namespaces {
            let gateway = GatewayInventory::store_for(self.db.clone(), &principal, &controller)?;
            for (_, _, request) in gateway.list::<GatewayRequest>(INVENTORY_KIND).await? {
                if let Some(existing) = inventory.get(&request.request_id) {
                    ensure!(
                        existing.attempt_id == request.attempt_id,
                        "request ID collision across gateway namespaces"
                    );
                }
                inventory.insert(request.request_id.clone(), request);
            }
        }
        let known_ids = inventory.keys().cloned().collect();
        let mut requests = BTreeMap::new();
        let mut unassigned = BTreeSet::new();
        for (id, request) in inventory {
            let mut member = false;
            for segment in &cp.segments {
                let own = segment.identity == cp.identity;
                member |= belongs(&request, segment, own)?;
                if unresolved_in_prefix(&request, segment, own, own_successor_at.as_deref())? {
                    unassigned.insert(id.clone());
                }
            }
            if !member {
                continue;
            }
            if !request.coverage_ready {
                reasons.insert(format!("admission_coverage_incomplete:{id}"));
            }
            let settled = request.settlement.as_ref();
            let terminal = settled.filter(|settlement| settlement.outcome.is_some());
            if terminal.is_none() {
                reasons.insert(format!("request_unfinished:{id}"));
            }
            let cost = terminal
                .and_then(|settlement| settlement.total_cost_micro_usd)
                .map(i64::try_from)
                .transpose()?;
            requests.insert(
                id.clone(),
                RequestAssociation {
                    request_id: id,
                    model_id: settled.map(|s| s.final_model.clone()).unwrap_or_default(),
                    provider_id: settled
                        .map(|s| s.final_provider.clone())
                        .unwrap_or_default(),
                    native_turn_id: None,
                    native_agent_thread_id: request
                        .binding
                        .as_ref()
                        .map(|binding| binding.identity.native_session_id.clone()),
                    basis: "canonical_gateway_admission".into(),
                    charge_micro_usd: cost,
                    first_metered_at: settled.map(|s| s.settled_at.clone()).unwrap_or_default(),
                    identity_evidence: Some(serde_json::json!({
                        "version": INVENTORY_VERSION, "attempt_id": request.attempt_id,
                        "admitted_at": request.admitted_at, "binding": request.binding,
                    })),
                    route_events: vec![],
                },
            );
        }
        Ok(InventoryObservation {
            requests,
            known_ids,
            unassigned,
            connections: captured,
            coverage: GatewayCoverage {
                version: INVENTORY_VERSION.into(),
                scope: "bitrouter_managed_model_requests".into(),
                revisions,
                reasons: reasons.into_iter().collect(),
            },
        })
    }
}
