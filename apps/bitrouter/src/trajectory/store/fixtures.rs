//! Test setup for ledger invariants using the production transaction primitives.

use super::*;

impl TrajectoryStore {
    pub(crate) async fn events_for_episode(
        &self,
        owner_user_id: &str,
        episode_id: &str,
    ) -> Result<Vec<TrajectoryEvent>> {
        validate_owner(owner_user_id)?;
        let rows = event_entity::Entity::find()
            .filter(legacy_scope("trajectory_events"))
            .filter(event_entity::Column::OwnerUserId.eq(owner_user_id))
            .filter(event_entity::Column::EpisodeId.eq(episode_id))
            .order_by_asc(event_entity::Column::Sequence)
            .all(&self.db)
            .await?;
        let mut events = Vec::with_capacity(rows.len());
        for (index, row) in rows.into_iter().enumerate() {
            let event = stored_event(row)?;
            let expected = u64::try_from(index)
                .context("trajectory event count exceeds sequence range")?
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("trajectory event sequence overflow"))?;
            if event.sequence != expected {
                anyhow::bail!("trajectory event history has a sequence gap at {expected}")
            }
            events.push(event);
        }
        Ok(events)
    }

    pub(crate) async fn pending_outbox(&self, owner_user_id: &str) -> Result<Vec<PendingOutbox>> {
        validate_owner(owner_user_id)?;
        outbox_entity::Entity::find()
            .filter(legacy_scope("trajectory_outbox"))
            .filter(outbox_entity::Column::OwnerUserId.eq(owner_user_id))
            .filter(outbox_entity::Column::DeliveredAt.is_null())
            .order_by_asc(outbox_entity::Column::Attempts)
            .order_by_asc(outbox_entity::Column::CreatedAt)
            .order_by_asc(outbox_entity::Column::OutboxId)
            .all(&self.db)
            .await?
            .into_iter()
            .map(stored_outbox)
            .collect()
    }

    pub(crate) async fn episode(
        &self,
        owner_user_id: &str,
        episode_id: &str,
    ) -> Result<Option<StoredEpisode>> {
        validate_owner(owner_user_id)?;
        validate_opaque(episode_id, "episode_id")?;
        episode_entity::Entity::find()
            .filter(legacy_scope("trajectory_episodes"))
            .filter(episode_entity::Column::OwnerUserId.eq(owner_user_id))
            .filter(episode_entity::Column::EpisodeId.eq(episode_id))
            .one(&self.db)
            .await?
            .map(stored_episode)
            .transpose()
    }

    pub(crate) async fn begin_request(
        &self,
        owner_user_id: &str,
        input: BeginRequest,
    ) -> Result<()> {
        validate_owner(owner_user_id)?;
        validate_begin(owner_user_id, &input)?;
        let request_id = input
            .event
            .request_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("request start event must identify its request"))?;
        let txn = self.db.begin().await?;
        begin_request_in_tx(&txn, owner_user_id, &request_id, input).await?;
        txn.commit().await?;
        Ok(())
    }
    pub(crate) async fn append_route_intent(
        &self,
        owner_user_id: &str,
        event: TrajectoryEvent,
    ) -> Result<()> {
        self.append_request_event(
            owner_user_id,
            event,
            TrajectoryEventKind::RouteIntentRecorded,
            "route intent",
        )
        .await
    }
    async fn append_request_event(
        &self,
        owner_user_id: &str,
        event: TrajectoryEvent,
        expected_kind: TrajectoryEventKind,
        label: &str,
    ) -> Result<()> {
        validate_owner(owner_user_id)?;
        validate_event(&event)?;
        if event.kind != expected_kind || event.owner_user_id != owner_user_id {
            anyhow::bail!("{label} event must belong to its owner")
        }
        for attempt in 0..MAX_LEDGER_MUTATION_ATTEMPTS {
            match self.append_request_event_once(owner_user_id, &event).await {
                Ok(()) => return Ok(()),
                Err(error) if mutation_error_is_retryable(&error) => {
                    if self.persisted_event_matches(&event).await? {
                        return Ok(());
                    }
                    tokio::time::sleep(contention_backoff(attempt)).await;
                }
                Err(error) => return Err(error),
            }
        }
        anyhow::bail!(
            "trajectory {label} contention exhausted after {MAX_LEDGER_MUTATION_ATTEMPTS} attempts"
        )
    }
    async fn append_request_event_once(
        &self,
        owner_user_id: &str,
        event: &TrajectoryEvent,
    ) -> Result<()> {
        let txn = self.db.begin().await?;
        if event_matches_existing(&txn, event).await? {
            txn.commit().await?;
            return Ok(());
        }
        let episode = owned_episode(&txn, owner_user_id, &event.episode_id).await?;
        let events = events_for_episode_in_tx(&txn, owner_user_id, &episode.episode_id).await?;
        validate_episode_head(&episode, &events)?;
        if episode.closed_at.is_some() {
            anyhow::bail!("trajectory episode is closed and cannot accept new events")
        }
        validate_sequence(&episode, event)?;
        validate_event_request(&txn, owner_user_id, event).await?;
        validate_candidate_history(&events, event)?;
        append_event(&txn, event).await?;
        update_episode_head(&txn, episode, event, None).await?;
        txn.commit().await?;
        Ok(())
    }
    async fn persisted_event_matches(&self, event: &TrajectoryEvent) -> Result<bool> {
        let row = event_entity::Entity::find()
            .filter(legacy_scope("trajectory_events"))
            .filter(event_entity::Column::OwnerUserId.eq(&event.owner_user_id))
            .filter(event_entity::Column::EventId.eq(&event.event_id))
            .one(&self.db)
            .await?;
        let Some(row) = row else {
            return Ok(false);
        };
        let stored = stored_event(row)?;
        if stored == *event {
            return Ok(true);
        }
        anyhow::bail!(
            "trajectory event '{}' already exists with different content",
            event.event_id
        )
    }
    pub(crate) async fn settle_request(&self, owner: &str, settlement: Settlement) -> Result<()> {
        validate_owner(owner)?;
        validate_settlement(owner, &settlement)?;
        let request_id = settlement
            .event
            .request_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("settlement must identify its request"))?;
        self.settle_request_from_current_head(owner, request_id, |_, _, _| Ok(settlement.clone()))
            .await?;
        Ok(())
    }
}

async fn event_matches_existing(
    txn: &DatabaseTransaction,
    event: &TrajectoryEvent,
) -> Result<bool> {
    let existing = event_entity::Entity::find()
        .filter(legacy_scope("trajectory_events"))
        .filter(event_entity::Column::OwnerUserId.eq(&event.owner_user_id))
        .filter(event_entity::Column::EventId.eq(&event.event_id))
        .one(txn)
        .await?;
    if let Some(existing) = existing {
        let existing = stored_event(existing)?;
        if existing == *event {
            return Ok(true);
        }
        anyhow::bail!(
            "trajectory event '{}' already exists with different content",
            event.event_id
        )
    }
    if event_entity::Entity::find_by_id(&event.event_id)
        .filter(legacy_scope("trajectory_events"))
        .one(txn)
        .await?
        .is_some()
    {
        anyhow::bail!("trajectory event id is already owned by another user")
    }
    Ok(false)
}
async fn validate_event_request(
    txn: &DatabaseTransaction,
    owner_user_id: &str,
    event: &TrajectoryEvent,
) -> Result<()> {
    let Some(request_id) = &event.request_id else {
        return Ok(());
    };
    let request = request_entity::Entity::find()
        .filter(legacy_scope("trajectory_requests"))
        .filter(request_entity::Column::OwnerUserId.eq(owner_user_id))
        .filter(request_entity::Column::RequestId.eq(request_id))
        .one(txn)
        .await?
        .ok_or_else(|| anyhow::anyhow!("unknown owner-scoped trajectory request '{request_id}'"))?;
    if request.episode_id != event.episode_id {
        anyhow::bail!("trajectory event request belongs to a different episode")
    }
    Ok(())
}
