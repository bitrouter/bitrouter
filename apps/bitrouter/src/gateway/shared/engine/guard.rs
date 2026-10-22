//! Shared guard decisions join execution binding and projection catch-up.

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use bitrouter_ai::types::{ApiProtocol, Prompt};
use sea_orm::DatabaseTransaction;

use super::{Engine, Execution, Rejection, TrajectoryBinding};
use crate::gateway::shared::authority::Revision;
use crate::gateway::shared::store::{Store, Stored};
use crate::trajectory::canonical::Canonicalizer;
use crate::trajectory::correlation::{CorrelatedRequest, CorrelationEvidence, stable_id};
use crate::trajectory::shared::{PrefixMatch, SharedTrajectory};
use crate::trajectory::store::{CorrelateAndBegin, GuardedRouteInput, GuardedRouteResult};

pub(in crate::gateway::shared) struct Input<'a> {
    pub owner: &'a str,
    pub protocol: &'a ApiProtocol,
    pub prompt: &'a Prompt,
    pub captured_at: &'a str,
    pub route: GuardedRouteInput,
}

struct Prepared {
    revision: Revision,
    epochs: Vec<CorrelateAndBegin>,
}

impl Engine {
    pub async fn guarded_route(
        &self,
        key_hash: &str,
        execution_id: &str,
        input: &Input<'_>,
    ) -> Result<(CorrelatedRequest, GuardedRouteResult)> {
        let prepared = self.prepare_guard(execution_id, input).await?;
        self.store()
            .retry(|| async {
                let tx = self.store().begin().await?;
                let head = self.authority.fence(&tx).await?;
                ensure!(
                    head.value.revision() == prepared.revision,
                    Rejection::AuthorityChanged
                );
                let (_, execution, _) = self
                    .authorized_execution(
                        &tx,
                        key_hash,
                        execution_id,
                        super::attempts::TenantLock::Exclusive,
                    )
                    .await?;
                ensure!(
                    execution.value.tenant_id == input.owner,
                    Rejection::Forbidden
                );
                let inputs = execution
                    .value
                    .projection
                    .as_ref()
                    .context("missing original guard contract")?;
                inputs.check_identity(
                    execution_id,
                    &execution.value.tenant_id,
                    execution.value.view.operation,
                )?;
                inputs.check_guard_policy(&input.route.policy_name, &input.route.policy_digest)?;
                let trajectory = SharedTrajectory::new(self.store().namespace())?;
                let (epoch, episode_id) =
                    select_epoch(&trajectory, &tx, input.owner, &execution, &prepared.epochs)
                        .await?;
                let selected = &prepared.epochs[epoch];
                let binding = TrajectoryBinding {
                    owner_user_id: input.owner.into(),
                    episode_id,
                    request_id: selected.request_id.clone(),
                    correlation_epoch: selected.correlation_key_id.clone(),
                };
                let mut fence = self
                    .episode_fence(&tx, &binding, Store::now(&tx).await?)
                    .await?;
                // An existing binding is verified by select_epoch and the
                // historical retry below. Its committed decision does not
                // depend on later requests becoming projectable. Fresh starts
                // still catch up before choosing any new decision.
                if execution.value.trajectory.is_none() {
                    self.catch_up_in(&tx, &mut fence).await?;
                    // A row lock alone does not refresh a Serializable snapshot.
                    // Advance the fence even without catch-up so a waiting new
                    // start aborts with 40001 before allocating a stale sequence.
                    // https://www.postgresql.org/docs/current/transaction-iso.html
                    self.store()
                        .save(&tx, &mut fence, "ready", None, Store::now(&tx).await?)
                        .await?;
                }
                self.bind_trajectory(&tx, execution_id, &binding).await?;
                let result = trajectory
                    .guarded_start_in(&tx, input.owner, selected)
                    .await?;
                ensure!(
                    result.episode_id == binding.episode_id,
                    "guard resolved outside its episode fence"
                );
                let guard = result
                    .guarded_route
                    .context("shared guard lost its original decision")?;
                let correlated = CorrelatedRequest {
                    request_id: selected.request_id.clone(),
                    episode_id: result.episode_id,
                    source: result.source,
                    completeness: result.completeness,
                    prior_events: result.prior_events,
                    evidence: CorrelationEvidence {
                        native_parent_id: selected.native_parent_id.clone(),
                        full_input_digest: selected.full_input_digest.as_str().into(),
                        ancestor_prefix_digests: selected
                            .ancestor_prefix_digests
                            .iter()
                            .map(|d| d.as_str().into())
                            .collect(),
                        ancestor_prefixes_truncated: selected.ancestor_prefixes_truncated,
                        starts_with_prior_turns: selected.starts_with_prior_turns,
                        canonical_input_bytes: selected.canonical_input_bytes,
                    },
                };
                tx.commit().await?;
                Ok((correlated, guard))
            })
            .await
    }

    async fn prepare_guard(&self, execution_id: &str, input: &Input<'_>) -> Result<Prepared> {
        let head = self.authority.head().await?;
        let keys = self.authority.keys(&head).await?.correlations()?;
        let external_parent =
            crate::trajectory::correlation::native_parent_id(input.protocol, input.prompt)?;
        let parent = external_parent
            .as_deref()
            .map(|parent| {
                crate::gateway::model::protocol::responses::decode_gateway_continuation_id(parent)
                    .map(|decoded| decoded.unwrap_or_else(|| parent.into()))
            })
            .transpose()?;
        let mut epochs = Vec::with_capacity(keys.len());
        for key in keys {
            let canonicalizer = Canonicalizer::new(key);
            let canonical = canonicalizer.canonicalize(input.prompt)?;
            let request_id = canonicalizer.request_identity(input.owner, execution_id)?;
            let mut route = input.route.clone();
            route.route_event_id = stable_id("route-intent", input.owner, &request_id);
            route.guard_event_id = stable_id("guard-activation", input.owner, &request_id);
            epochs.push(CorrelateAndBegin {
                new_episode_id: stable_id("episode", input.owner, &request_id),
                event_id: stable_id("request-start", input.owner, &request_id),
                request_id,
                correlation_key_id: canonicalizer.key_id().into(),
                native_parent_id: parent
                    .as_deref()
                    .map(|id| canonicalizer.request_identity(input.owner, id))
                    .transpose()?,
                native_parent_digest: external_parent
                    .as_deref()
                    .map(|id| canonicalizer.native_parent_digest(id))
                    .transpose()?,
                full_input_digest: canonical.full_input_digest,
                ancestor_prefix_digests: canonical.ancestor_prefix_digests,
                ancestor_prefixes_truncated: canonical.ancestor_prefixes_truncated,
                starts_with_prior_turns: canonical.starts_with_prior_turns,
                canonical_input_bytes: canonical.canonical_input_bytes,
                protocol: crate::trajectory::correlation::protocol_name(input.protocol).into(),
                captured_at: input.captured_at.into(),
                guarded_route: Some(route),
            });
        }
        Ok(Prepared {
            revision: head.revision(),
            epochs,
        })
    }
}

async fn select_epoch(
    trajectory: &SharedTrajectory,
    tx: &DatabaseTransaction,
    owner: &str,
    execution: &Stored<Execution>,
    epochs: &[CorrelateAndBegin],
) -> Result<(usize, String)> {
    ensure!(
        !epochs.is_empty() && epochs.len() <= 16,
        "invalid retained guard epochs"
    );
    if let Some(binding) = &execution.value.trajectory {
        ensure!(
            binding.owner_user_id == owner,
            "guard binding owner changed"
        );
        let index = epochs
            .iter()
            .position(|epoch| epoch.correlation_key_id == binding.correlation_epoch)
            .context("original guard epoch is no longer retained")?;
        ensure!(
            epochs[index].request_id == binding.request_id,
            "original guard request identity changed"
        );
        let episode = trajectory
            .episode_in(tx, owner, &binding.episode_id)
            .await?
            .context("bound guard episode is missing")?;
        ensure!(
            episode.correlation_key_id == binding.correlation_epoch,
            "bound guard episode epoch changed"
        );
        ensure!(
            trajectory
                .request_in(tx, owner, &binding.request_id)
                .await?
                .is_some(),
            "guard binding lost its atomic start"
        );
        return Ok((index, binding.episode_id.clone()));
    }
    let ids = epochs
        .iter()
        .flat_map(|epoch| {
            std::iter::once(epoch.request_id.clone()).chain(epoch.native_parent_id.clone())
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let requests = trajectory.requests_in(tx, owner, &ids).await?;
    let prefixes = epochs
        .iter()
        .flat_map(|epoch| epoch.ancestor_prefix_digests.iter().cloned())
        .collect::<Vec<_>>();
    let prefixes = trajectory.prefixes_in(tx, owner, &prefixes).await?;
    let mut matches = BTreeMap::<String, usize>::new();
    let mut native_found = false;
    for (index, epoch) in epochs.iter().enumerate() {
        ensure!(
            !requests.contains_key(&epoch.request_id),
            "guard start exists without its atomic execution binding"
        );
        if let Some(parent) = epoch
            .native_parent_id
            .as_ref()
            .and_then(|id| requests.get(id))
        {
            ensure!(
                parent.full_input_digest.key_id() == epoch.correlation_key_id,
                "native parent epoch changed"
            );
            retain_match(&mut matches, &parent.episode_id, index)?;
            native_found = true;
        }
        for digest in &epoch.ancestor_prefix_digests {
            match prefixes.get(digest.as_str()) {
                None => {}
                #[cfg(test)]
                Some(PrefixMatch::Missing) => {}
                Some(PrefixMatch::Ambiguous) => anyhow::bail!("guard prefix is ambiguous"),
                Some(PrefixMatch::Unique(id)) => retain_match(&mut matches, id, index)?,
            }
        }
    }
    let active = epochs.len() - 1;
    ensure!(
        epochs[active].native_parent_id.is_none() || native_found,
        "guard native parent is missing"
    );
    if let Some((id, index)) = matches.into_iter().next() {
        let episode = trajectory
            .episode_in(tx, owner, &id)
            .await?
            .context("guard correlation lost its episode")?;
        ensure!(
            episode.correlation_key_id == epochs[index].correlation_key_id,
            "guard correlation epoch changed"
        );
        Ok((index, id))
    } else {
        let root = &epochs[active];
        ensure!(
            !root.starts_with_prior_turns
                && !root.ancestor_prefixes_truncated
                && root.native_parent_id.is_none(),
            "guard ancestry is unresolved"
        );
        Ok((active, root.new_episode_id.clone()))
    }
}

fn retain_match(matches: &mut BTreeMap<String, usize>, id: &str, epoch: usize) -> Result<()> {
    if let Some(original) = matches.insert(id.into(), epoch) {
        ensure!(
            original == epoch,
            "episode matched different correlation epochs"
        );
    }
    ensure!(
        matches.len() <= 1,
        "guard correlation matches different episodes"
    );
    Ok(())
}
