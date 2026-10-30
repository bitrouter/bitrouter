use super::super::{CorrelateAndBegin, CorrelateAndBeginResult, StartFacts};
use super::*;
use crate::trajectory::correlation::CorrelationSource;

impl OwnedStore {
    #[cfg(test)]
    pub(crate) async fn correlate_and_begin_in(
        &self,
        tx: &DatabaseTransaction,
        input: &CorrelateAndBegin,
        budget: &mut io::Budget<'_>,
    ) -> Result<CorrelateAndBeginResult> {
        self.correlate_and_begin_reserved_in(
            tx,
            input,
            headroom::Reservation::new(16 * 1024, input.guarded_route.is_some())?,
            budget,
        )
        .await
    }

    pub(crate) async fn correlate_and_begin_reserved_in(
        &self,
        tx: &DatabaseTransaction,
        input: &CorrelateAndBegin,
        reservation: headroom::Reservation,
        budget: &mut io::Budget<'_>,
    ) -> Result<CorrelateAndBeginResult> {
        self.begin_correlated_in(tx, input, reservation, None, budget)
            .await
    }

    #[cfg(feature = "harness")]
    pub(crate) async fn begin_bound_experiment_in(
        &self,
        tx: &DatabaseTransaction,
        bound: &crate::tenancy::experiment::BoundEpisode,
        input: &CorrelateAndBegin,
        reservation: headroom::Reservation,
        budget: &mut io::Budget<'_>,
    ) -> Result<CorrelateAndBeginResult> {
        ensure!(
            input.new_episode_id == bound.episode(),
            "bound native episode changed"
        );
        let result = self
            .begin_correlated_in(tx, input, reservation, Some(bound.episode()), budget)
            .await?;
        bound.check_selected(
            result
                .guarded_route
                .as_ref()
                .and_then(|route| route.intent.selected_tier.as_deref()),
        )?;
        Ok(result)
    }

    async fn begin_correlated_in(
        &self,
        tx: &DatabaseTransaction,
        input: &CorrelateAndBegin,
        reservation: headroom::Reservation,
        bound: Option<&str>,
        budget: &mut io::Budget<'_>,
    ) -> Result<CorrelateAndBeginResult> {
        let owner = self.owner()?;
        super::super::validate_correlation_input(&owner, input)?;
        ensure!(
            input.ancestor_prefix_digests.len() <= 256,
            crate::storage::operation_budget::Error::Capacity
        );
        ensure!(
            input.native_parent_id.is_none() && input.native_parent_digest.is_none(),
            "owned native parent requires a qualified continuation proof"
        );
        if let Some((request, _)) = self.request_row(tx, &input.request_id, budget).await? {
            ensure!(
                bound.is_none_or(|episode| request.episode_id == episode),
                "native request replay belongs to another bound episode"
            );
            let (_, events) = self.history(tx, &request.episode_id, budget).await?;
            self.validate_request_edges(&request, &events)?;
            let start = events
                .iter()
                .find(|e| e.event_id == request.start_event_id)
                .context("trajectory retry lost start")?;
            ensure!(
                headroom::Reservation::read(start)? == reservation,
                "trajectory reservation replay conflict"
            );
            return super::super::exact_retry_from_history(&owner, &request, input, &events, start);
        }
        let mut resolved = match bound {
            Some(episode) => {
                ensure!(
                    input.ancestor_prefix_digests.is_empty(),
                    "bound native history cannot consume prefix aliases"
                );
                self.episode_row(tx, episode, budget)
                    .await?
                    .map(|(row, _)| row)
            }
            None => None,
        };
        let mut conflict = false;
        if bound.is_none() && !input.ancestor_prefix_digests.is_empty() {
            let digests: std::collections::BTreeSet<String> = input
                .ancestor_prefix_digests
                .iter()
                .map(|d| d.as_str().into())
                .collect();
            let rows = io::load(
                tx,
                self.prefixes()
                    .filter(prefix_entity::Column::OwnerUserId.eq(&owner))
                    .filter(prefix_entity::Column::FullInputDigest.is_in(digests)),
                budget,
            )
            .await?;
            let mut prefixes = std::collections::BTreeMap::new();
            for row in rows {
                let (row, _) = self.decode_prefix(row)?;
                let digest = KeyedDigest::parse(row.full_input_digest.clone())?;
                ensure!(
                    digest.key_id() == input.correlation_key_id,
                    "trajectory prefix key epoch mismatch"
                );
                ensure!(
                    prefixes
                        .insert(row.full_input_digest.clone(), row)
                        .is_none(),
                    "duplicate scoped trajectory prefix"
                );
            }
            for digest in input.ancestor_prefix_digests.iter().rev() {
                if let Some(prefix) = prefixes.get(digest.as_str()) {
                    if prefix.ambiguous {
                        conflict = true;
                    } else {
                        let (episode, _) = self
                            .episode_row(tx, &prefix.episode_id, budget)
                            .await?
                            .context("trajectory prefix has foreign or missing episode")?;
                        ensure!(
                            episode.correlation_key_id == input.correlation_key_id,
                            "trajectory episode key epoch mismatch"
                        );
                        resolved = Some(episode);
                    }
                    break;
                }
            }
        }
        let (source, completeness) = if let Some(episode) = &resolved {
            (
                if bound.is_some() {
                    match episode.correlation_source.as_str() {
                        "explicit_root" => CorrelationSource::ExplicitRoot,
                        "unresolved" => CorrelationSource::Unresolved,
                        _ => anyhow::bail!("bound native episode has ambient correlation"),
                    }
                } else {
                    CorrelationSource::CanonicalPrefix
                },
                super::super::parse_completeness(&episode.history_completeness)?,
            )
        } else if conflict || input.starts_with_prior_turns {
            (
                CorrelationSource::Unresolved,
                HistoryCompleteness::Incomplete,
            )
        } else {
            (
                CorrelationSource::ExplicitRoot,
                HistoryCompleteness::Complete,
            )
        };
        let prior_events = if let Some(episode) = &resolved {
            self.history(tx, &episode.episode_id, budget).await?.1
        } else {
            Vec::new()
        };
        let captured =
            super::super::episode_monotonic_captured_at(&prior_events, &input.captured_at)?;
        let (episode, sequence) = match resolved {
            Some(row) => {
                ensure!(row.closed_at.is_none(), "trajectory episode is closed");
                let sequence = u64::try_from(row.next_sequence)?;
                (
                    EpisodeStart {
                        episode_id: row.episode_id,
                        correlation_digest: KeyedDigest::parse(row.correlation_digest)?,
                        correlation_key_id: row.correlation_key_id,
                        correlation_source: row.correlation_source,
                        completeness: super::super::parse_completeness(&row.history_completeness)?,
                    },
                    sequence,
                )
            }
            None => (
                EpisodeStart {
                    episode_id: input.new_episode_id.clone(),
                    correlation_digest: input.full_input_digest.clone(),
                    correlation_key_id: input.correlation_key_id.clone(),
                    correlation_source: source.as_str().into(),
                    completeness,
                },
                1,
            ),
        };
        let episode_id = episode.episode_id.clone();
        let mut start = super::super::build_start_event(
            &owner,
            input,
            &episode_id,
            sequence,
            &captured,
            StartFacts {
                source,
                completeness,
                prefix_conflict: conflict,
                key_epoch_conflict: false,
            },
        )?;
        reservation.retain(&mut start)?;
        // Build and validate the entire guarded candidate before any write.
        let batch = input
            .guarded_route
            .as_ref()
            .map(|guard| {
                super::super::build_guarded_route_batch(&prior_events, &start, completeness, guard)
            })
            .transpose()?;
        if let Some(batch) = &batch {
            self.encode_event(&batch.route_event)?;
            if let Some(event) = &batch.guard_event {
                self.encode_event(event)?;
            }
            let mut candidate = prior_events.clone();
            candidate.push(start.clone());
            candidate.push(batch.route_event.clone());
            candidate.extend(batch.guard_event.iter().cloned());
            ensure!(
                candidate.len() < MAX_EVENTS,
                crate::storage::operation_budget::Error::Capacity
            );
            let guard = input
                .guarded_route
                .as_ref()
                .context("missing guarded input")?;
            super::super::reduce(&candidate, &guard.policy.protected_tiers)?;
        }
        let mut candidate = prior_events.clone();
        candidate.push(start.clone());
        if let Some(batch) = &batch {
            candidate.push(batch.route_event.clone());
            candidate.extend(batch.guard_event.iter().cloned());
        }
        headroom::reserve(&candidate)?;
        self.begin_with_prefix_in(
            tx,
            &BeginRequest {
                episode,
                event: start,
                full_input_digest: input.full_input_digest.clone(),
                native_parent_id: None,
                protocol: input.protocol.clone(),
            },
            bound.is_none(),
            budget,
        )
        .await?;
        let guarded_route = if let Some(batch) = batch {
            let mut events = vec![batch.route_event];
            events.extend(batch.guard_event);
            self.append_route_in(tx, &events, budget).await?;
            Some(batch.result)
        } else {
            None
        };
        Ok(CorrelateAndBeginResult {
            episode_id,
            source,
            completeness,
            prior_events,
            guarded_route,
        })
    }
}
