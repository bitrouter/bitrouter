//! Borrowed, scoped maintenance primitives. No worker or scope-from-row factory.
use super::*;

impl OwnedStore {
    pub(crate) async fn pending_in(
        &self,
        tx: &DatabaseTransaction,
        budget: &mut io::Budget<'_>,
    ) -> Result<Vec<PendingOutbox>> {
        let rows = io::load(
            tx,
            self.outboxes()
                .filter(outbox_entity::Column::DeliveredAt.is_null()),
            budget,
        )
        .await?;
        let mut result = Vec::with_capacity(rows.len());
        for row in rows {
            result.push(super::super::stored_outbox(self.decode_outbox(row)?.0)?);
        }
        result.sort_by(|a, b| (&a.created_at, &a.outbox_id).cmp(&(&b.created_at, &b.outbox_id)));
        Ok(result)
    }

    /// No automatic caller is installed. Read all candidates with an operation
    /// budget, validate relationships, then delete only complete delivered history.
    /// Any Err requires the actor to roll back the entire borrowed transaction.
    pub(crate) async fn prune_in(
        &self,
        tx: &DatabaseTransaction,
        before: &str,
        dry_run: bool,
        budget: &mut io::Budget<'_>,
    ) -> Result<super::super::PruneSummary> {
        let before = chrono::DateTime::parse_from_rfc3339(before)?;
        let candidates = io::load(tx, self.episodes(), budget).await?;
        let mut summary = super::super::PruneSummary::default();
        for row in candidates {
            let (episode, _) = self.decode_episode(row)?;
            if !super::super::timestamp_is_before(
                &episode.last_captured_at,
                before,
                "episode timestamp",
            )? {
                continue;
            }
            if let Some(closed) = &episode.closed_at
                && !super::super::timestamp_is_before(closed, before, "closed timestamp")?
            {
                continue;
            }
            let (_, events) = self.history(tx, &episode.episode_id, budget).await?;
            let requests = io::load(
                tx,
                self.requests().filter(
                    request_entity::Column::EpisodeId
                        .eq(self.key(KeyDomain::TrajectoryEpisode, &episode.episode_id)?),
                ),
                budget,
            )
            .await?;
            let mut logical = Vec::new();
            let mut ready = !requests.is_empty();
            for row in requests {
                let (request, _) = self.decode_request(row)?;
                self.validate_request_edges(&request, &events)?;
                if !matches!(request.status.as_str(), "settled" | "failed") {
                    ready = false;
                }
                if let Some(id) = &request.settlement_outbox_id {
                    let (outbox, _) = self
                        .outbox_row(tx, id, budget)
                        .await?
                        .context("trajectory prune lost outbox")?;
                    if outbox.delivered_at.is_none() {
                        ready = false;
                    }
                    let terminal = events
                        .iter()
                        .find(|e| Some(&e.event_id) == request.settlement_event_id.as_ref())
                        .context("prune lost terminal event")?;
                    let payload: OutboxPayload = serde_json::from_str(&outbox.payload_json)?;
                    ensure!(
                        payload.digests.get("trajectory.settlement_event")
                            == Some(&terminal.content_digest),
                        "trajectory prune outbox has foreign terminal"
                    );
                }
                logical.push(request);
            }
            if !ready {
                continue;
            }
            summary.episode_rows = summary
                .episode_rows
                .checked_add(1)
                .context("prune count overflow")?;
            summary.event_rows = summary
                .event_rows
                .checked_add(events.len().try_into()?)
                .context("prune count overflow")?;
            summary.request_rows = summary
                .request_rows
                .checked_add(logical.len().try_into()?)
                .context("prune count overflow")?;
            summary.delivered_outbox_rows = summary
                .delivered_outbox_rows
                .checked_add(
                    logical
                        .iter()
                        .filter(|r| r.settlement_outbox_id.is_some())
                        .count()
                        .try_into()?,
                )
                .context("prune count overflow")?;
            if dry_run {
                continue;
            }
            // Preserve ambiguous summary rows while any other episode still names
            // that exact scoped digest. Neither the probe nor delete is global.
            for request in &logical {
                let others = io::load(
                    tx,
                    self.requests()
                        .filter(
                            request_entity::Column::FullInputDigest.eq(&request.full_input_digest),
                        )
                        .filter(
                            request_entity::Column::EpisodeId
                                .ne(self.key(KeyDomain::TrajectoryEpisode, &episode.episode_id)?),
                        ),
                    budget,
                )
                .await?;
                for row in &others {
                    self.decode_request(row.clone())?;
                }
                let prefixes = io::load(
                    tx,
                    self.prefixes()
                        .filter(prefix_entity::Column::OwnerUserId.eq(self.owner()?))
                        .filter(
                            prefix_entity::Column::FullInputDigest.eq(&request.full_input_digest),
                        ),
                    budget,
                )
                .await?;
                for row in prefixes {
                    self.decode_prefix(row)?;
                }
                if others.is_empty() {
                    budget.check()?;
                    prefix_entity::Entity::delete_many()
                        .filter(self.scope.predicate(
                            prefix_entity::Column::InstallationId,
                            prefix_entity::Column::TenantId,
                        ))
                        .filter(prefix_entity::Column::OwnerUserId.eq(self.owner()?))
                        .filter(
                            prefix_entity::Column::FullInputDigest.eq(&request.full_input_digest),
                        )
                        .exec(tx)
                        .await?;
                    budget.check()?;
                }
                if let Some(id) = &request.settlement_outbox_id {
                    budget.check()?;
                    let changed = outbox_entity::Entity::delete_many()
                        .filter(self.scope.predicate(
                            outbox_entity::Column::InstallationId,
                            outbox_entity::Column::TenantId,
                        ))
                        .filter(
                            outbox_entity::Column::OutboxId
                                .eq(self.key(KeyDomain::TrajectoryOutbox, id)?),
                        )
                        .filter(outbox_entity::Column::DeliveredAt.is_not_null())
                        .exec(tx)
                        .await?;
                    budget.check()?;
                    ensure!(
                        changed.rows_affected == 1,
                        "trajectory outbox changed during prune"
                    );
                }
            }
            budget.check()?;
            let deleted = request_entity::Entity::delete_many()
                .filter(self.scope.predicate(
                    request_entity::Column::InstallationId,
                    request_entity::Column::TenantId,
                ))
                .filter(
                    request_entity::Column::EpisodeId
                        .eq(self.key(KeyDomain::TrajectoryEpisode, &episode.episode_id)?),
                )
                .exec(tx)
                .await?;
            budget.check()?;
            ensure!(
                deleted.rows_affected == logical.len() as u64,
                "trajectory request changed during prune"
            );
            budget.check()?;
            let deleted = event_entity::Entity::delete_many()
                .filter(self.scope.predicate(
                    event_entity::Column::InstallationId,
                    event_entity::Column::TenantId,
                ))
                .filter(
                    event_entity::Column::EpisodeId
                        .eq(self.key(KeyDomain::TrajectoryEpisode, &episode.episode_id)?),
                )
                .exec(tx)
                .await?;
            budget.check()?;
            ensure!(
                deleted.rows_affected == events.len() as u64,
                "trajectory events changed during prune"
            );
            budget.check()?;
            let deleted = episode_entity::Entity::delete_many()
                .filter(self.scope.predicate(
                    episode_entity::Column::InstallationId,
                    episode_entity::Column::TenantId,
                ))
                .filter(
                    episode_entity::Column::EpisodeId
                        .eq(self.key(KeyDomain::TrajectoryEpisode, &episode.episode_id)?),
                )
                .filter(episode_entity::Column::NextSequence.eq(episode.next_sequence))
                .exec(tx)
                .await?;
            budget.check()?;
            ensure!(
                deleted.rows_affected == 1,
                "trajectory episode changed during prune"
            );
        }
        Ok(summary)
    }
}
