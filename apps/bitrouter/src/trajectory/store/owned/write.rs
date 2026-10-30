use super::*;
use sea_orm::ActiveModelTrait;

impl OwnedStore {
    async fn insert_event(
        &self,
        tx: &DatabaseTransaction,
        event: &TrajectoryEvent,
        budget: &mut io::Budget<'_>,
    ) -> Result<()> {
        let row = self.encode_event(event)?;
        io::charge_model(budget, &row)?;
        io::insert(
            tx,
            event_entity::Entity::insert(row.into_active_model()).into_query(),
            budget,
        )
        .await?;
        let winner = io::one(
            tx,
            self.events().filter(
                event_entity::Column::EventId
                    .eq(self.key(KeyDomain::TrajectoryEvent, &event.event_id)?),
            ),
            budget,
        )
        .await?
        .context("trajectory event physical-key collision")?;
        ensure!(
            self.decode_event(winner)? == *event,
            "immutable trajectory event conflict"
        );
        Ok(())
    }
    async fn insert_episode(
        &self,
        tx: &DatabaseTransaction,
        episode: &old_episode::Model,
        budget: &mut io::Budget<'_>,
    ) -> Result<()> {
        let row = self.encode_episode(episode, &self.provenance)?;
        io::charge_model(budget, &row)?;
        io::insert(
            tx,
            episode_entity::Entity::insert(row.into_active_model()).into_query(),
            budget,
        )
        .await?;
        let (winner, _) = self
            .episode_row(tx, &episode.episode_id, budget)
            .await?
            .context("trajectory episode physical-key collision")?;
        ensure!(winner == *episode, "immutable trajectory episode conflict");
        Ok(())
    }
    async fn save_episode(
        &self,
        tx: &DatabaseTransaction,
        episode: &old_episode::Model,
        provenance: &Provenance,
        expected: i64,
        budget: &mut io::Budget<'_>,
    ) -> Result<()> {
        let row = self.encode_episode(episode, provenance)?;
        io::charge_model(budget, &row)?;
        let changed = episode_entity::Entity::update_many()
            .set(row.into_active_model().reset_all())
            .filter(self.scope.predicate(
                episode_entity::Column::InstallationId,
                episode_entity::Column::TenantId,
            ))
            .filter(
                episode_entity::Column::EpisodeId
                    .eq(self.key(KeyDomain::TrajectoryEpisode, &episode.episode_id)?),
            )
            .filter(episode_entity::Column::NextSequence.eq(expected))
            .exec(tx)
            .await?;
        budget.check()?;
        ensure!(
            changed.rows_affected == 1,
            "trajectory sequence reservation conflict"
        );
        Ok(())
    }
    async fn insert_request(
        &self,
        tx: &DatabaseTransaction,
        request: &old_request::Model,
        publish_prefix: bool,
        budget: &mut io::Budget<'_>,
    ) -> Result<()> {
        let row = self.encode_request(request, &self.provenance)?;
        io::charge_model(budget, &row)?;
        io::insert(
            tx,
            request_entity::Entity::insert(row.into_active_model()).into_query(),
            budget,
        )
        .await?;
        let (winner, _) = self
            .request_row(tx, &request.request_id, budget)
            .await?
            .context("trajectory request physical-key collision")?;
        ensure!(winner == *request, "immutable trajectory request conflict");
        if publish_prefix {
            self.record_prefix(tx, &request.full_input_digest, &request.episode_id, budget)
                .await?;
        }
        Ok(())
    }
    async fn record_prefix(
        &self,
        tx: &DatabaseTransaction,
        digest: &str,
        episode: &str,
        budget: &mut io::Budget<'_>,
    ) -> Result<()> {
        KeyedDigest::parse(digest.to_owned())?;
        let logical = old_prefix::Model {
            owner_user_id: self.owner()?,
            full_input_digest: digest.into(),
            episode_id: episode.into(),
            ambiguous: false,
        };
        let row = self.encode_prefix(&logical, &self.provenance)?;
        io::charge_model(budget, &row)?;
        io::insert(
            tx,
            prefix_entity::Entity::insert(row.into_active_model()).into_query(),
            budget,
        )
        .await?;
        let row = io::one(
            tx,
            self.prefixes()
                .filter(prefix_entity::Column::OwnerUserId.eq(self.owner()?))
                .filter(prefix_entity::Column::FullInputDigest.eq(digest)),
            budget,
        )
        .await?
        .context("trajectory prefix physical-key collision")?;
        let (mut winner, provenance) = self.decode_prefix(row)?;
        ensure!(
            winner.full_input_digest == digest,
            "trajectory prefix digest mismatch"
        );
        if winner.episode_id != episode && !winner.ambiguous {
            winner.ambiguous = true;
            let row = self.encode_prefix(&winner, &provenance)?;
            io::charge_model(budget, &row)?;
            let changed = prefix_entity::Entity::update_many()
                .set(row.into_active_model().reset_all())
                .filter(self.scope.predicate(
                    prefix_entity::Column::InstallationId,
                    prefix_entity::Column::TenantId,
                ))
                .filter(prefix_entity::Column::OwnerUserId.eq(self.owner()?))
                .filter(prefix_entity::Column::FullInputDigest.eq(digest))
                .filter(prefix_entity::Column::Ambiguous.eq(false))
                .exec(tx)
                .await?;
            budget.check()?;
            ensure!(
                changed.rows_affected == 1,
                "trajectory prefix changed during mutation"
            );
        }
        Ok(())
    }

    /// Append a validated start to a known/new scoped episode. No commit is performed.
    pub(super) async fn begin_with_prefix_in(
        &self,
        tx: &DatabaseTransaction,
        input: &BeginRequest,
        publish_prefix: bool,
        budget: &mut io::Budget<'_>,
    ) -> Result<()> {
        super::super::validate_begin(&self.owner()?, input)?;
        ensure!(
            input.native_parent_id.is_none(),
            "owned native parent requires a continuation proof"
        );
        let request_id = input
            .event
            .request_id
            .as_deref()
            .context("trajectory start lacks request")?;
        if let Some((existing, _)) = self.request_row(tx, request_id, budget).await? {
            let ((episode, _), events) = self.history(tx, &existing.episode_id, budget).await?;
            self.validate_request_edges(&existing, &events)?;
            let start = events
                .iter()
                .find(|e| e.event_id == existing.start_event_id)
                .context("missing start")?;
            ensure!(
                super::super::begin_matches(&existing, &episode, start, input),
                "trajectory start replay conflict"
            );
            return Ok(());
        }
        let existing = self
            .episode_row(tx, &input.episode.episode_id, budget)
            .await?;
        if let Some((mut episode, provenance)) = existing {
            let (_, events) = self.history(tx, &episode.episode_id, budget).await?;
            ensure!(episode.closed_at.is_none(), "trajectory episode is closed");
            super::super::validate_existing_episode(&episode, &input.episode, &input.event)?;
            super::super::validate_candidate_history(&events, &input.event)?;
            self.terminal_headroom(tx, &episode.episode_id, events.len(), 1, budget)
                .await?;
            self.insert_event(tx, &input.event, budget).await?;
            let expected = episode.next_sequence;
            advance(&mut episode, &input.event)?;
            self.save_episode(tx, &episode, &provenance, expected, budget)
                .await?;
        } else {
            super::super::validate_new_episode_start(input)?;
            super::super::validate_candidate_history(&[], &input.event)?;
            ensure!(
                input.event.sequence == 1,
                "first trajectory sequence must be one"
            );
            self.insert_event(tx, &input.event, budget).await?;
            let episode = old_episode::Model {
                episode_id: input.episode.episode_id.clone(),
                owner_user_id: self.owner()?,
                correlation_digest: input.episode.correlation_digest.as_str().into(),
                correlation_key_id: input.episode.correlation_key_id.clone(),
                correlation_source: input.episode.correlation_source.clone(),
                history_completeness: super::super::completeness_name(input.episode.completeness)
                    .into(),
                next_sequence: 2,
                first_captured_at: input.event.captured_at.clone(),
                last_captured_at: input.event.captured_at.clone(),
                closed_at: None,
                latest_request_id: input.event.request_id.clone(),
            };
            self.insert_episode(tx, &episode, budget).await?;
        }
        self.insert_request(
            tx,
            &old_request::Model {
                request_id: request_id.into(),
                owner_user_id: self.owner()?,
                episode_id: input.episode.episode_id.clone(),
                start_event_id: input.event.event_id.clone(),
                settlement_event_id: None,
                settlement_outbox_id: None,
                full_input_digest: input.full_input_digest.as_str().into(),
                native_parent_id: None,
                protocol: input.protocol.clone(),
                status: "started".into(),
            },
            publish_prefix,
            budget,
        )
        .await
    }

    async fn terminal_headroom(
        &self,
        tx: &DatabaseTransaction,
        episode: &str,
        events: usize,
        extra_starts: usize,
        budget: &mut io::Budget<'_>,
    ) -> Result<()> {
        let rows = io::load(
            tx,
            self.requests()
                .filter(
                    request_entity::Column::EpisodeId
                        .eq(self.key(KeyDomain::TrajectoryEpisode, episode)?),
                )
                .filter(request_entity::Column::Status.eq("started")),
            budget,
        )
        .await?;
        for row in &rows {
            self.decode_request(row.clone())?;
        }
        let required = events
            .checked_add(rows.len())
            .and_then(|n| n.checked_add(extra_starts * 4))
            .context("trajectory headroom overflow")?;
        ensure!(
            required <= MAX_EVENTS,
            crate::storage::operation_budget::Error::Capacity
        );
        Ok(())
    }

    /// A route intent and its guard activation are one semantic batch. Validate
    /// the complete candidate before writing either event or advancing the head.
    pub(super) async fn append_route_in(
        &self,
        tx: &DatabaseTransaction,
        batch: &[TrajectoryEvent],
        budget: &mut io::Budget<'_>,
    ) -> Result<()> {
        ensure!(
            (1..=2).contains(&batch.len()),
            "invalid trajectory route batch size"
        );
        let event = &batch[0];
        ensure!(
            event.kind == TrajectoryEventKind::RouteIntentRecorded,
            "trajectory route batch must begin with route intent"
        );
        if let Some(guard) = batch.get(1) {
            ensure!(
                guard.kind == TrajectoryEventKind::GuardActivated,
                "trajectory route batch must end with guard activation"
            );
        }
        let request = event
            .request_id
            .as_deref()
            .context("trajectory append lacks request")?;
        let (request, _) = self
            .request_row(tx, request, budget)
            .await?
            .context("unknown trajectory request")?;
        let ((mut episode, provenance), events) =
            self.history(tx, &request.episode_id, budget).await?;
        self.validate_request_edges(&request, &events)?;
        for candidate in batch {
            ensure!(
                candidate.episode_id == request.episode_id
                    && candidate.request_id.as_deref() == Some(request.request_id.as_str())
                    && candidate.owner_user_id == self.owner()?,
                "trajectory route batch changed identity"
            );
        }
        let persisted = events
            .iter()
            .filter(|candidate| {
                candidate.request_id.as_deref() == Some(request.request_id.as_str())
                    && matches!(
                        candidate.kind,
                        TrajectoryEventKind::RouteIntentRecorded
                            | TrajectoryEventKind::GuardActivated
                    )
            })
            .collect::<Vec<_>>();
        if !persisted.is_empty() {
            ensure!(
                persisted.len() == batch.len()
                    && persisted
                        .iter()
                        .zip(batch)
                        .all(|(stored, supplied)| *stored == supplied),
                "trajectory route batch replay conflict"
            );
            return Ok(());
        }
        ensure!(
            request.status == "started" && episode.closed_at.is_none(),
            "trajectory request is terminal"
        );
        super::super::validate_sequence(&episode, event)?;
        let mut candidate = events.clone();
        candidate.extend_from_slice(batch);
        super::super::reduce(&candidate, &Default::default())?;
        self.terminal_headroom(tx, &episode.episode_id, candidate.len(), 0, budget)
            .await?;
        let expected = episode.next_sequence;
        for event in batch {
            self.insert_event(tx, event, budget).await?;
            advance(&mut episode, event)?;
        }
        self.save_episode(tx, &episode, &provenance, expected, budget)
            .await
    }

    /// Builder sees the immutable original prefix on retry. Caller commits only after
    /// its Eval/outbox obligations are complete; any Err requires H-owned rollback.
    pub(crate) async fn settle_current_in<F>(
        &self,
        tx: &DatabaseTransaction,
        request_id: &str,
        build: F,
        budget: &mut io::Budget<'_>,
    ) -> Result<Settlement>
    where
        F: FnOnce(&StoredRequest, &[TrajectoryEvent], u64) -> Result<Settlement>,
    {
        let (mut request, creator) = self
            .request_row(tx, request_id, budget)
            .await?
            .context("unknown trajectory request")?;
        let ((mut episode, provenance), events) =
            self.history(tx, &request.episode_id, budget).await?;
        self.validate_request_edges(&request, &events)?;
        let old = request
            .settlement_event_id
            .as_deref()
            .map(|id| {
                events
                    .iter()
                    .position(|e| e.event_id == id)
                    .context("lost trajectory settlement")
            })
            .transpose()?;
        let (prefix, sequence) = match old {
            Some(n) => (&events[..n], events[n].sequence),
            None => (&events[..], u64::try_from(episode.next_sequence)?),
        };
        let settlement = build(
            &super::super::stored_request(request.clone())?,
            prefix,
            sequence,
        )?;
        super::super::validate_settlement(&self.owner()?, &settlement)?;
        let start = prefix
            .iter()
            .find(|event| event.event_id == request.start_event_id)
            .context("terminal lost reserved start")?;
        headroom::Reservation::read(start)?.validate(&settlement)?;
        if settlement.outbox.is_some() {
            let canonical = crate::trajectory::settlement::build_from_evidence(
                &self.owner()?,
                &super::super::stored_request(request.clone())?,
                prefix,
                sequence,
                settlement.event.evidence.clone(),
                settlement.event.captured_at.clone(),
                settlement.status.clone(),
            )?;
            ensure!(
                canonical.event == settlement.event && canonical.outbox == settlement.outbox,
                "owned trajectory outbox does not bind the exact terminal closure"
            );
        }
        ensure!(
            settlement.event.request_id.as_deref() == Some(request_id)
                && settlement.event.episode_id == request.episode_id,
            "trajectory settlement changed identity"
        );
        if let Some(n) = old {
            ensure!(
                events[n] == settlement.event
                    && request.status
                        == super::super::request_status_name(settlement.status.clone()),
                "trajectory settlement replay conflict"
            );
            self.check_outbox(
                tx,
                request.settlement_outbox_id.as_deref(),
                settlement.outbox.as_ref(),
                budget,
            )
            .await?;
            return Ok(settlement);
        }
        ensure!(
            episode.closed_at.is_none() && events.len() < MAX_EVENTS,
            "trajectory terminal capacity exhausted"
        );
        super::super::validate_candidate_history(&events, &settlement.event)?;
        self.insert_event(tx, &settlement.event, budget).await?;
        request.settlement_event_id = Some(settlement.event.event_id.clone());
        request.settlement_outbox_id = settlement.outbox.as_ref().map(|o| o.outbox_id.clone());
        request.status = super::super::request_status_name(settlement.status.clone()).into();
        let row = self.encode_request(&request, &creator)?;
        io::charge_model(budget, &row)?;
        let changed = request_entity::Entity::update_many()
            .set(row.into_active_model().reset_all())
            .filter(self.scope.predicate(
                request_entity::Column::InstallationId,
                request_entity::Column::TenantId,
            ))
            .filter(
                request_entity::Column::RequestId
                    .eq(self.key(KeyDomain::TrajectoryRequest, request_id)?),
            )
            .filter(request_entity::Column::Status.eq("started"))
            .exec(tx)
            .await?;
        budget.check()?;
        ensure!(
            changed.rows_affected == 1,
            "trajectory terminal CAS conflict"
        );
        let expected = episode.next_sequence;
        advance(&mut episode, &settlement.event)?;
        self.save_episode(tx, &episode, &provenance, expected, budget)
            .await?;
        if let Some(outbox) = &settlement.outbox {
            super::super::validate_outbox(outbox)?;
            io::json_size(&outbox.payload, 60 * 1024)?;
            let payload_json = serde_json::to_string(&outbox.payload)?;
            let logical = old_outbox::Model {
                outbox_id: outbox.outbox_id.clone(),
                owner_user_id: self.owner()?,
                topic: outbox.topic.clone(),
                payload_json,
                payload_digest: canonical_digest(&outbox.payload)?,
                attempts: 0,
                created_at: outbox.created_at.clone(),
                delivered_at: None,
            };
            let row = self.encode_outbox(&logical, &self.provenance)?;
            io::charge_model(budget, &row)?;
            io::insert(
                tx,
                outbox_entity::Entity::insert(row.into_active_model()).into_query(),
                budget,
            )
            .await?;
            self.check_outbox(tx, Some(&outbox.outbox_id), Some(outbox), budget)
                .await?;
        }
        Ok(settlement)
    }
    async fn check_outbox(
        &self,
        tx: &DatabaseTransaction,
        id: Option<&str>,
        supplied: Option<&OutboxWrite>,
        budget: &mut io::Budget<'_>,
    ) -> Result<()> {
        match (id, supplied) {
            (None, None) => Ok(()),
            (Some(id), Some(value)) if id == value.outbox_id => {
                let (row, _) = self
                    .outbox_row(tx, id, budget)
                    .await?
                    .context("missing scoped trajectory outbox")?;
                ensure!(
                    row.topic == value.topic
                        && row.payload_json == serde_json::to_string(&value.payload)?
                        && row.payload_digest == canonical_digest(&value.payload)?
                        && row.created_at == value.created_at,
                    "trajectory outbox replay conflict"
                );
                Ok(())
            }
            _ => anyhow::bail!("trajectory settlement outbox mismatch"),
        }
    }
    pub(crate) async fn check_delivered_in(
        &self,
        tx: &DatabaseTransaction,
        outbox: &OutboxWrite,
        budget: &mut io::Budget<'_>,
    ) -> Result<()> {
        self.check_outbox(tx, Some(&outbox.outbox_id), Some(outbox), budget)
            .await?;
        let (row, _) = self
            .outbox_row(tx, &outbox.outbox_id, budget)
            .await?
            .context("lost trajectory outbox")?;
        ensure!(
            row.delivered_at.as_deref() == Some(&outbox.created_at) && row.attempts == 1,
            "terminal owned outbox lacks atomic delivery"
        );
        Ok(())
    }
    pub(crate) async fn mark_delivered_in(
        &self,
        tx: &DatabaseTransaction,
        outbox: &OutboxWrite,
        budget: &mut io::Budget<'_>,
    ) -> Result<()> {
        self.check_outbox(tx, Some(&outbox.outbox_id), Some(outbox), budget)
            .await?;
        let (mut existing, creator) = self
            .outbox_row(tx, &outbox.outbox_id, budget)
            .await?
            .context("lost trajectory outbox")?;
        existing.delivered_at = Some(outbox.created_at.clone());
        existing.attempts = 1;
        let row = self.encode_outbox(&existing, &creator)?;
        io::charge_model(budget, &row)?;
        let changed = outbox_entity::Entity::update_many()
            .set(row.into_active_model().reset_all())
            .filter(self.scope.predicate(
                outbox_entity::Column::InstallationId,
                outbox_entity::Column::TenantId,
            ))
            .filter(
                outbox_entity::Column::OutboxId
                    .eq(self.key(KeyDomain::TrajectoryOutbox, &outbox.outbox_id)?),
            )
            .filter(outbox_entity::Column::DeliveredAt.is_null())
            .exec(tx)
            .await?;
        budget.check()?;
        ensure!(
            changed.rows_affected <= 1,
            "trajectory outbox update cardinality"
        );
        let (row, _) = self
            .outbox_row(tx, &outbox.outbox_id, budget)
            .await?
            .context("lost trajectory outbox")?;
        ensure!(
            row.delivered_at.as_deref() == Some(&outbox.created_at) && row.attempts == 1,
            "trajectory delivery conflict"
        );
        Ok(())
    }
}

fn advance(episode: &mut old_episode::Model, event: &TrajectoryEvent) -> Result<()> {
    episode.next_sequence = event
        .sequence
        .checked_add(1)
        .context("trajectory sequence overflow")?
        .try_into()?;
    episode.last_captured_at = event.captured_at.clone();
    if event.kind == TrajectoryEventKind::RequestStarted {
        episode.latest_request_id = event.request_id.clone();
    }
    Ok(())
}
