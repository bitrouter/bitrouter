//! Guarded starts join the caller's fenced, caught-up Serializable transaction.
//! Epoch authority and episode locking belong to the gateway caller, not here.

mod headroom;

#[cfg(test)]
mod tests;

use anyhow::{Context, Result, ensure};
use sea_orm::DatabaseTransaction;

use super::{
    MAX_GUARDED_SETTLEMENT_BYTES, MAX_HISTORY_BYTES, MAX_HISTORY_EVENTS, PrefixMatch,
    SharedTrajectory, check_transaction,
};
use crate::trajectory::{
    canonical::MAX_ANCESTOR_PREFIX_DIGESTS,
    correlation::CorrelationSource,
    health::reduce,
    store::{
        self, CorrelateAndBegin, CorrelateAndBeginResult, GuardedRouteInput, StartFacts,
        episode_entity, request_entity,
    },
    types::{
        BeginRequest, EpisodeStart, HistoryCompleteness, KeyedDigest, StoredRequest,
        TrajectoryEvent, TrajectoryEventKind,
    },
};

impl SharedTrajectory {
    /// The caller has selected one retained key epoch, acquired the resulting
    /// episode fence and caught up earlier durable settlements in this same
    /// Serializable transaction. On any error it must roll back the whole TX.
    /// Exact retries are read-only and do not re-evaluate mutable prefix indexes.
    pub async fn guarded_start_in(
        &self,
        tx: &DatabaseTransaction,
        owner: &str,
        input: &CorrelateAndBegin,
    ) -> Result<CorrelateAndBeginResult> {
        validate_input(owner, input)?;
        check_transaction(tx).await?;
        if let Some(existing) = self.request_row(tx, owner, &input.request_id).await? {
            let episode = self
                .episode_row(tx, owner, &existing.episode_id)
                .await?
                .context("shared guarded retry lost its episode")?;
            let history = self.history_in(tx, owner, &existing.episode_id).await?;
            if let Some(parent_id) = &existing.native_parent_id {
                let parent = self
                    .request_in(tx, owner, parent_id)
                    .await?
                    .context("shared guarded retry lost its native parent")?;
                ensure!(
                    parent.episode_id == existing.episode_id
                        && parent.full_input_digest.key_id() == input.correlation_key_id,
                    "shared guarded retry native parent changed epoch or episode"
                );
                let parent_start = validate_parent_history(owner, &parent, &history)?;
                let start = history
                    .iter()
                    .find(|event| event.event_id == existing.start_event_id)
                    .context("shared guarded retry lost its start")?;
                ensure!(
                    parent_start.sequence < start.sequence,
                    "invalid native parent order"
                );
            }
            return retry_from_history(owner, &episode, &existing, input, &history);
        }

        let native_parent = match &input.native_parent_id {
            Some(parent_id) => {
                let parent = self
                    .request_in(tx, owner, parent_id)
                    .await?
                    .context("shared guarded start requires its owner-scoped native parent")?;
                ensure!(
                    parent.full_input_digest.key_id() == input.correlation_key_id,
                    "shared native parent uses another correlation epoch"
                );
                Some(parent)
            }
            None => None,
        };
        let mut prefix_episode = None;
        // One bounded metadata query, with no per-prefix episode/history reads.
        // Check every returned match before validating the selected episode.
        let matches = self
            .prefixes_in(tx, owner, &input.ancestor_prefix_digests)
            .await?;
        for matched in matches.into_values() {
            match matched {
                #[cfg(test)]
                PrefixMatch::Missing => {}
                PrefixMatch::Ambiguous => anyhow::bail!("ambiguous shared trajectory prefix"),
                PrefixMatch::Unique(id) => merge_prefix(&mut prefix_episode, id)?,
            }
        }
        let (selected, source) = select_episode(
            input,
            native_parent
                .as_ref()
                .map(|parent| parent.episode_id.clone()),
            prefix_episode,
        )?;
        let existing_episode = match &selected {
            Some(id) => Some(
                self.episode_row(tx, owner, id)
                    .await?
                    .context("shared correlation lost its owner-scoped episode")?,
            ),
            None => {
                // Never use the proposed root ID as an alternate correlation path.
                ensure!(
                    self.episode_row(tx, owner, &input.new_episode_id)
                        .await?
                        .is_none(),
                    "shared root identity already names an episode"
                );
                None
            }
        };
        let history = match &existing_episode {
            Some(episode) => {
                validate_open_epoch(owner, episode, input)?;
                self.history_in(tx, owner, &episode.episode_id).await?
            }
            None => Vec::new(),
        };
        if let Some(parent) = &native_parent {
            validate_parent_history(owner, parent, &history)?;
        }
        let episode = match &existing_episode {
            Some(row) => EpisodeStart {
                episode_id: row.episode_id.clone(),
                correlation_digest: KeyedDigest::parse(row.correlation_digest.clone())?,
                correlation_key_id: row.correlation_key_id.clone(),
                correlation_source: row.correlation_source.clone(),
                completeness: store::stored_episode(row.clone())?.completeness,
            },
            None => EpisodeStart {
                episode_id: input.new_episode_id.clone(),
                correlation_digest: input.full_input_digest.clone(),
                correlation_key_id: input.correlation_key_id.clone(),
                correlation_source: source.as_str().into(),
                completeness: HistoryCompleteness::Complete,
            },
        };
        let sequence = match history.last() {
            Some(head) => head
                .sequence
                .checked_add(1)
                .context("shared start sequence overflow")?,
            None => 1,
        };
        let captured_at = store::episode_monotonic_captured_at(&history, &input.captured_at)?;
        let start = store::build_start_event(
            owner,
            input,
            &episode.episode_id,
            sequence,
            &captured_at,
            StartFacts {
                source,
                completeness: episode.completeness,
                prefix_conflict: false,
                key_epoch_conflict: false,
            },
        )?;
        let route = input
            .guarded_route
            .as_ref()
            .context("shared guard input missing")?;
        let batch =
            store::build_guarded_route_batch(&history, &start, episode.completeness, route)?;
        let mut appended = vec![start.clone(), batch.route_event];
        appended.extend(batch.guard_event);
        reserve_settlements(&history, &appended, route)?;
        let begin = BeginRequest {
            episode,
            event: start,
            full_input_digest: input.full_input_digest.clone(),
            native_parent_id: input.native_parent_id.clone(),
            protocol: input.protocol.clone(),
        };
        self.begin_in(tx, owner, &begin).await?;
        self.append_route_in(tx, owner, &appended[1..]).await?;
        Ok(CorrelateAndBeginResult {
            episode_id: begin.episode.episode_id,
            source,
            completeness: begin.episode.completeness,
            prior_events: history,
            guarded_route: Some(batch.result),
        })
    }
}

fn merge_prefix(selected: &mut Option<String>, id: String) -> Result<()> {
    ensure!(
        selected.as_ref().is_none_or(|previous| previous == &id),
        "shared trajectory prefixes name conflicting episodes"
    );
    *selected = Some(id);
    Ok(())
}

fn select_episode(
    input: &CorrelateAndBegin,
    native: Option<String>,
    prefix: Option<String>,
) -> Result<(Option<String>, CorrelationSource)> {
    if let Some(native) = native {
        ensure!(
            prefix.as_ref().is_none_or(|prefix| prefix == &native),
            "shared native parent conflicts with canonical prefix"
        );
        ensure!(
            !input.ancestor_prefixes_truncated || prefix.is_some(),
            "shared truncated ancestry has no retained prefix proof"
        );
        return Ok((Some(native), CorrelationSource::NativeParentId));
    }
    if let Some(prefix) = prefix {
        return Ok((Some(prefix), CorrelationSource::CanonicalPrefix));
    }
    ensure!(
        input.native_parent_id.is_none()
            && !input.starts_with_prior_turns
            && !input.ancestor_prefixes_truncated,
        "shared guarded start cannot invent a root for unresolved ancestry"
    );
    Ok((None, CorrelationSource::ExplicitRoot))
}

fn validate_parent_history<'a>(
    owner: &str,
    parent: &StoredRequest,
    history: &'a [TrajectoryEvent],
) -> Result<&'a TrajectoryEvent> {
    let start = history
        .iter()
        .find(|event| event.event_id == parent.start_event_id)
        .context("shared native parent lost its start")?;
    ensure!(
        start.owner_user_id == owner
            && start.episode_id == parent.episode_id
            && start.request_id.as_deref() == Some(parent.request_id.as_str())
            && start.kind == TrajectoryEventKind::RequestStarted,
        "shared native parent start identity mismatch"
    );
    Ok(start)
}

fn validate_open_epoch(
    owner: &str,
    row: &episode_entity::Model,
    input: &CorrelateAndBegin,
) -> Result<()> {
    validate_epoch(owner, row, input)?;
    ensure!(
        row.closed_at.is_none(),
        "shared trajectory episode is closed"
    );
    Ok(())
}

fn validate_epoch(
    owner: &str,
    row: &episode_entity::Model,
    input: &CorrelateAndBegin,
) -> Result<()> {
    ensure!(
        row.owner_user_id == owner
            && row.correlation_key_id == input.correlation_key_id
            && KeyedDigest::parse(row.correlation_digest.clone())?.key_id()
                == input.correlation_key_id,
        "shared trajectory episode uses another owner or correlation epoch"
    );
    ensure!(
        matches!(
            row.correlation_source.as_str(),
            "explicit_root" | "canonical_prefix" | "native_parent_id"
        ),
        "shared trajectory episode has unresolved correlation"
    );
    Ok(())
}

fn retry_from_history(
    owner: &str,
    episode: &episode_entity::Model,
    existing: &request_entity::Model,
    input: &CorrelateAndBegin,
    history: &[TrajectoryEvent],
) -> Result<CorrelateAndBeginResult> {
    validate_epoch(owner, episode, input)?;
    ensure!(
        existing.owner_user_id == owner
            && existing.request_id == input.request_id
            && existing.episode_id == episode.episode_id
            && existing.native_parent_id == input.native_parent_id,
        "shared guarded retry changed request identity"
    );
    store::stored_request(existing.clone())?;
    let start = history
        .iter()
        .find(|event| event.event_id == existing.start_event_id)
        .context("shared guarded retry lost its start")?;
    let result = store::exact_retry_from_history(owner, existing, input, history, start)?;
    ensure!(
        result.source != CorrelationSource::Unresolved,
        "shared guarded retry has unresolved ancestry"
    );
    if result.source == CorrelationSource::ExplicitRoot {
        ensure!(
            input.new_episode_id == episode.episode_id
                && start.sequence == 1
                && input.native_parent_id.is_none()
                && !input.starts_with_prior_turns
                && !input.ancestor_prefixes_truncated,
            "shared guarded retry changed its root"
        );
    }
    let captured_at =
        store::episode_monotonic_captured_at(&result.prior_events, &input.captured_at)?;
    let expected = store::build_start_event(
        owner,
        input,
        &existing.episode_id,
        start.sequence,
        &captured_at,
        StartFacts {
            source: result.source,
            completeness: result.completeness,
            prefix_conflict: false,
            key_epoch_conflict: false,
        },
    )?;
    ensure!(
        &expected == start,
        "shared guarded retry changed immutable start facts"
    );
    // A later head, settlement or closure does not rewrite an exact old result.
    Ok(result)
}

fn validate_input(owner: &str, input: &CorrelateAndBegin) -> Result<()> {
    ensure!(
        input.ancestor_prefix_digests.len() <= MAX_ANCESTOR_PREFIX_DIGESTS,
        "shared ancestor prefix limit exceeded"
    );
    store::validate_correlation_input(owner, input)?;
    for value in [
        &input.request_id,
        &input.new_episode_id,
        &input.event_id,
        &input.protocol,
        &input.captured_at,
    ] {
        store::validate_opaque(value, "shared guarded start field")?;
    }
    chrono::DateTime::parse_from_rfc3339(&input.captured_at)
        .context("invalid shared start timestamp")?;
    if let Some(parent) = &input.native_parent_id {
        store::validate_opaque(parent, "shared native parent")?;
    }
    let route = input
        .guarded_route
        .as_ref()
        .context("shared guarded start requires a guard")?;
    // Bound caller-owned collections before the existing pure builder clones or
    // serializes them. Publication and the event validators remain authoritative
    // for the narrower semantic tier/measurement shapes.
    ensure!(
        route.tier_efforts.len() <= MAX_HISTORY_EVENTS
            && route.policy.protected_tiers.len() <= MAX_HISTORY_EVENTS
            && route.tool_safe_tiers.len() <= MAX_HISTORY_EVENTS,
        "shared guard tier input limit exceeded"
    );
    for value in [
        &route.route_event_id,
        &route.guard_event_id,
        &route.policy_name,
        &route.route_projection,
        &route.request_key,
        &route.policy_digest,
        &route.policy.escalation_tier,
    ]
    .into_iter()
    .chain(route.baseline_tier.iter())
    .chain(route.preset.iter())
    .chain(route.candidate_tier.iter())
    .chain(route.tool_use_tier.iter())
    .chain(route.tier_efforts.keys())
    .chain(route.policy.protected_tiers.iter())
    .chain(route.tool_safe_tiers.iter())
    {
        store::validate_opaque(value, "shared guard input field")?;
    }
    if let Some(experiment) = &route.experiment {
        store::validate_opaque(&experiment.experiment_id, "shared guard experiment")?;
        store::validate_opaque(&experiment.assignment_id_digest, "shared guard assignment")?;
    }
    if let Some(measurement) = &route.route_measurement {
        crate::storage::evaluation::validate_route_measurement(measurement)?;
    }
    Ok(())
}

fn reserve_settlements(
    history: &[TrajectoryEvent],
    appended: &[TrajectoryEvent],
    route: &GuardedRouteInput,
) -> Result<()> {
    let count = history
        .len()
        .checked_add(appended.len())
        .context("shared history count overflow")?;
    ensure!(
        count <= MAX_HISTORY_EVENTS,
        "shared guarded history event limit exceeded"
    );
    let mut bytes = 0usize;
    for event in history.iter().chain(appended) {
        crate::trajectory::types::validate_event(event)?;
        bytes = bytes
            .checked_add(serde_json::to_vec(event)?.len())
            .context("shared history byte overflow")?;
        ensure!(
            bytes <= MAX_HISTORY_BYTES,
            "shared guarded history byte limit exceeded"
        );
    }
    let events = history.iter().chain(appended).cloned().collect::<Vec<_>>();
    let snapshot = reduce(&events, &route.policy.protected_tiers)?;
    reserve_capacity(count, bytes, snapshot.health.unsettled_request_count)?;
    headroom::reserve(&events, snapshot.health.unsettled_request_count)
}

fn reserve_capacity(count: usize, bytes: usize, unsettled: u64) -> Result<()> {
    let unsettled = usize::try_from(unsettled).context("shared unsettled count overflow")?;
    ensure!(
        count
            .checked_add(unsettled)
            .context("shared settlement count overflow")?
            <= MAX_HISTORY_EVENTS,
        "shared trajectory lacks settlement event headroom"
    );
    let reserved = unsettled
        .checked_mul(MAX_GUARDED_SETTLEMENT_BYTES)
        .context("shared settlement byte reserve overflow")?;
    ensure!(
        bytes
            .checked_add(reserved)
            .context("shared settlement byte overflow")?
            <= MAX_HISTORY_BYTES,
        "shared trajectory lacks settlement byte headroom"
    );
    Ok(())
}
