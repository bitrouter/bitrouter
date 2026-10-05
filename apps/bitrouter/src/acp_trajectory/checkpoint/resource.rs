//! Resource snapshots can advance without mutating an immutable content prefix.

use super::*;
use crate::evolution::inventory::{coverage_matches, lock_connection};
use crate::evolution::store::EvolutionStore;

mod coverage;

fn observation_digest(observation: &ResourceObservation) -> Result<String> {
    digest(&(
        &observation.membership_version,
        &observation.requests,
        &observation.unassigned_request_ids,
        &observation.gateway_coverage,
        observation.metering_complete,
    ))
}

fn at_or_before(value: &str, boundary: &str) -> Result<bool> {
    if boundary.is_empty() {
        return Ok(false);
    }
    Ok(chrono::DateTime::parse_from_rfc3339(value)?
        <= chrono::DateTime::parse_from_rfc3339(boundary)?)
}

pub(super) fn totals(requests: &[super::super::RequestAssociation]) -> Result<(i64, usize)> {
    let mut known = 0_i64;
    let mut unknown = 0;
    for request in requests {
        if let Some(cost) = request.charge_micro_usd {
            known = known.checked_add(cost).context("request cost overflow")?;
        } else {
            unknown += 1;
        }
    }
    Ok((known, unknown))
}

impl CanonicalStore {
    /// Refresh only already stored metering and route observations. No receipt
    /// fetch or other external validation is started by this operation.
    pub async fn observe_checkpoint_resources(
        &self,
        identity: &SessionIdentity,
        id: &str,
    ) -> Result<ResourceObservation> {
        let content = self.checkpoint_content(identity, id).await?;
        let inventory = self.inventory_coverage(&content).await?;
        let cp = content.checkpoint;
        let mut requests = inventory.requests;
        let mut unassigned = inventory.unassigned;
        let mut coverage = inventory.coverage;
        for segment in &cp.segments {
            let mut scoped = Vec::new();
            for captured in &segment.connections {
                let row = connections::Entity::find_by_id(&captured.connection_id)
                    .one(&self.db)
                    .await?
                    .context("capture connection missing")?;
                ensure!(
                    row.controller_instance_id == captured.controller_instance_id
                        && row.route_scope_id == captured.route_scope_id,
                    "recording scope changed"
                );
                scoped.push(row);
            }
            for mut request in self.associated_requests(&segment.identity, &scoped).await? {
                // Canonical gateway admission takes precedence over loose
                // metering correlations, including a known different session.
                if inventory.known_ids.contains(&request.request_id) {
                    continue;
                }
                let mut preceding = Vec::new();
                for event in request.route_events {
                    if at_or_before(&event.captured_at, &segment.boundary_at)? {
                        preceding.push(event);
                    }
                }
                let metered_before = at_or_before(&request.first_metered_at, &segment.boundary_at)?;
                if preceding.is_empty() && !metered_before {
                    unassigned.insert(request.request_id);
                    continue;
                }
                request.route_events = preceding;
                request.basis.push_str(if metered_before {
                    "; metered_before_prefix_boundary"
                } else {
                    "; route_evidence_before_prefix_boundary"
                });
                coverage
                    .reasons
                    .push(format!("request_inventory_missing:{}", request.request_id));
                requests.insert(request.request_id.clone(), request);
            }
        }
        unassigned.retain(|id| !requests.contains_key(id));
        let requests: Vec<_> = requests.into_values().collect();
        let unassigned_request_ids: Vec<_> = unassigned.into_iter().collect();
        let (known_cost_micro_usd, unpriced_requests) = totals(&requests)?;
        if !unassigned_request_ids.is_empty() {
            coverage.reasons.push("unassigned_requests".into());
        }
        if unpriced_requests != 0 {
            coverage.reasons.push("request_cost_unknown".into());
        }
        coverage.reasons.sort();
        coverage.reasons.dedup();
        let metering_complete = coverage.reasons.is_empty();
        let mut observation = ResourceObservation {
            membership_version: Some(RESOURCE_MEMBERSHIP_VERSION.into()),
            observation_id: String::new(),
            checkpoint_id: id.into(),
            revision: 1,
            previous_observation_id: None,
            observed_at: chrono::Utc::now().to_rfc3339(),
            requests,
            unassigned_request_ids,
            known_cost_micro_usd,
            unpriced_requests,
            metering_complete,
            gateway_coverage: Some(coverage.clone()),
        };
        let content_digest = observation_digest(&observation)?;
        let tx = self.db.begin().await?;
        for (id, expected) in &inventory.connections {
            let actual = lock_connection(&tx, id).await?;
            ensure!(
                actual.state == expected.state,
                "capture health changed during resource observation; retry"
            );
        }
        let store = EvolutionStore::new(self.db.clone(), &identity.owner)?;
        ensure!(
            coverage_matches(&store, &tx, &coverage.revisions).await?,
            "gateway requests changed during resource observation; retry"
        );
        let keys: BTreeSet<_> = cp
            .segments
            .iter()
            .map(|s| s.identity.key())
            .collect::<Result<_>>()?;
        for key in keys {
            lock_session(&tx, &key).await?;
        }
        manifest(&tx, identity, id).await?;
        if let Some(existing) = resources::Entity::find()
            .filter(resources::Column::CheckpointId.eq(id))
            .order_by_desc(resources::Column::Revision)
            .one(&tx)
            .await?
        {
            let previous: ResourceObservation = serde_json::from_str(&existing.observation_json)?;
            if observation_digest(&previous)? == content_digest {
                tx.commit().await?;
                return Ok(previous);
            }
            observation.revision = previous
                .revision
                .checked_add(1)
                .context("resource revision overflow")?;
            observation.previous_observation_id = Some(previous.observation_id);
        }
        observation.observation_id = digest(&(id, observation.revision, &content_digest))?;
        resources::ActiveModel {
            observation_id: Set(observation.observation_id.clone()),
            checkpoint_id: Set(id.into()),
            revision: Set(observation.revision),
            observed_at: Set(observation.observed_at.clone()),
            observation_json: Set(serde_json::to_string(&observation)?),
        }
        .insert(&tx)
        .await?;
        tx.commit().await?;
        Ok(observation)
    }

    pub async fn checkpoint_resource_history(
        &self,
        identity: &SessionIdentity,
        id: &str,
    ) -> Result<Vec<ResourceObservation>> {
        manifest(&self.db, identity, id).await?;
        let rows = resources::Entity::find()
            .filter(resources::Column::CheckpointId.eq(id))
            .order_by_asc(resources::Column::Revision)
            .all(&self.db)
            .await?;
        let result = rows
            .into_iter()
            .map(|r| serde_json::from_str(&r.observation_json).map_err(Into::into))
            .collect();
        manifest(&self.db, identity, id).await?;
        result
    }
}
