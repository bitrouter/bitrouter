//! Mutations join the caller's authority/receipt transaction; errors require rollback.

use super::*;
use crate::trajectory::types::{
    BeginRequest, OutboxWrite, RequestStatus, Settlement, TrajectoryEventKind, canonical_digest,
    validate_event,
};

impl SharedTrajectory {
    /// Raw start persistence. Guarded exact retries must reconstruct their
    /// original decision separately; this helper only accepts a Started retry.
    pub async fn begin_in(
        &self,
        tx: &DatabaseTransaction,
        owner: &str,
        input: &BeginRequest,
    ) -> Result<()> {
        check_transaction(tx).await?;
        store::validate_owner(owner)?;
        store::validate_begin(owner, input)?;
        let request_id = input
            .event
            .request_id
            .as_deref()
            .context("missing trajectory request")?;
        if let Some(parent) = &input.native_parent_id {
            let row = tx.query_one(self.statement(tx,
                "SELECT namespace, owner_user_id FROM gateway_shared_trajectory_requests WHERE namespace=$1 AND request_id=$2",
                vec![parent.clone().into()])?).await?;
            if let Some(row) = row {
                self.check_namespace(&row)?;
                ensure!(
                    row.try_get::<String>("", "owner_user_id")? == owner,
                    "native parent belongs to another owner"
                );
            }
        }
        if let Some(request) = self.request_row(tx, owner, request_id).await? {
            let episode = self
                .episode_row(tx, owner, &request.episode_id)
                .await?
                .context("shared request lost its episode")?;
            let history = self.history_in(tx, owner, &request.episode_id).await?;
            let start = history
                .iter()
                .find(|event| event.event_id == request.start_event_id)
                .context("shared request lost its start")?;
            ensure!(
                store::begin_matches(&request, &episode, start, input),
                "conflicting shared trajectory start"
            );
            return Ok(());
        }
        let id = &input.episode.episode_id;
        if let Some(episode) = self.episode_row(tx, owner, id).await? {
            let history = self.history_in(tx, owner, id).await?;
            ensure!(
                episode.closed_at.is_none(),
                "shared trajectory episode is closed"
            );
            store::validate_existing_episode(&episode, &input.episode, &input.event)?;
            self.validate_append(&history, std::slice::from_ref(&input.event))?;
            self.insert_event(tx, &input.event).await?;
            self.advance(tx, &episode, &input.event, Some(request_id))
                .await?;
        } else {
            ensure!(
                input.event.sequence == 1,
                "new shared trajectory must start at sequence one"
            );
            store::validate_new_episode_start(input)?;
            self.validate_append(&[], std::slice::from_ref(&input.event))?;
            self.insert_event(tx, &input.event).await?;
            tx.execute(self.statement(tx,
                "INSERT INTO gateway_shared_trajectory_episodes
                 (namespace,episode_id,owner_user_id,correlation_digest,correlation_key_id,correlation_source,
                  history_completeness,next_sequence,first_captured_at,last_captured_at,closed_at,latest_request_id)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,2,$8,$8,NULL,$9)",
                vec![id.clone().into(), owner.into(), input.episode.correlation_digest.as_str().into(),
                     input.episode.correlation_key_id.clone().into(), input.episode.correlation_source.clone().into(),
                     store::completeness_name(input.episode.completeness).into(), input.event.captured_at.clone().into(), request_id.into()])?).await?;
        }
        tx.execute(self.statement(tx,
            "INSERT INTO gateway_shared_trajectory_requests
             (namespace,request_id,owner_user_id,episode_id,start_event_id,full_input_digest,native_parent_id,protocol,status)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
            vec![request_id.into(), owner.into(), id.clone().into(), input.event.event_id.clone().into(),
                 input.full_input_digest.as_str().into(), input.native_parent_id.clone().into(), input.protocol.clone().into(),
                 store::request_status_name(RequestStatus::Started).into()])?).await?;
        // Ambiguity is monotonic: an earlier episode mapping is never replaced.
        // https://www.postgresql.org/docs/current/sql-insert.html#SQL-ON-CONFLICT
        tx.execute(self.statement(tx,
            "INSERT INTO gateway_shared_trajectory_prefix_index (namespace,owner_user_id,full_input_digest,episode_id,ambiguous)
             VALUES ($1,$2,$3,$4,false) ON CONFLICT (namespace,owner_user_id,full_input_digest)
             DO UPDATE SET ambiguous=gateway_shared_trajectory_prefix_index.ambiguous
                OR gateway_shared_trajectory_prefix_index.episode_id <> EXCLUDED.episode_id",
            vec![owner.into(), input.full_input_digest.as_str().into(), id.clone().into()])?).await?;
        Ok(())
    }

    /// Route intent and its optional guard activation form one semantic batch.
    pub async fn append_route_in(
        &self,
        tx: &DatabaseTransaction,
        owner: &str,
        events: &[TrajectoryEvent],
    ) -> Result<()> {
        check_transaction(tx).await?;
        ensure!(
            (1..=2).contains(&events.len()),
            "invalid shared route batch size"
        );
        let first = &events[0];
        ensure!(
            first.kind == TrajectoryEventKind::RouteIntentRecorded,
            "shared route batch must start with route intent"
        );
        if let Some(guard) = events.get(1) {
            ensure!(
                guard.kind == TrajectoryEventKind::GuardActivated,
                "shared route batch must end with guard activation"
            );
        }
        let request_id = first
            .request_id
            .as_deref()
            .context("route batch has no request")?;
        let request = self
            .request_row(tx, owner, request_id)
            .await?
            .context("unknown owner-scoped shared request")?;
        let episode = self
            .episode_row(tx, owner, &request.episode_id)
            .await?
            .context("shared request lost its episode")?;
        let history = self.history_in(tx, owner, &episode.episode_id).await?;
        for event in events {
            ensure!(
                event.owner_user_id == owner
                    && event.episode_id == episode.episode_id
                    && event.request_id.as_deref() == Some(request_id),
                "shared route batch changed identity"
            );
        }
        let persisted = history
            .iter()
            .filter(|event| {
                event.request_id.as_deref() == Some(request_id)
                    && matches!(
                        event.kind,
                        TrajectoryEventKind::RouteIntentRecorded
                            | TrajectoryEventKind::GuardActivated
                    )
            })
            .collect::<Vec<_>>();
        if !persisted.is_empty() {
            ensure!(
                persisted.len() == events.len()
                    && persisted
                        .iter()
                        .zip(events)
                        .all(|(stored, supplied)| *stored == supplied),
                "conflicting shared route batch"
            );
            return Ok(());
        }
        ensure!(
            episode.closed_at.is_none()
                && request.status == store::request_status_name(RequestStatus::Started),
            "shared request cannot accept route events"
        );
        self.validate_append(&history, events)?;
        for event in events {
            self.insert_event(tx, event).await?;
        }
        self.advance(
            tx,
            &episode,
            events.last().context("missing route batch")?,
            None,
        )
        .await
    }

    pub async fn settle_in<F>(
        &self,
        tx: &DatabaseTransaction,
        owner: &str,
        request_id: &str,
        build: F,
    ) -> Result<Settlement>
    where
        F: Fn(&StoredRequest, &[TrajectoryEvent], u64) -> Result<Settlement>,
    {
        check_transaction(tx).await?;
        let request = self
            .request_row(tx, owner, request_id)
            .await?
            .context("unknown owner-scoped shared request")?;
        let episode = self
            .episode_row(tx, owner, &request.episode_id)
            .await?
            .context("shared request lost its episode")?;
        let history = self.history_in(tx, owner, &episode.episode_id).await?;
        let stored = store::stored_request(request.clone())?;
        if let Some(id) = &request.settlement_event_id {
            let position = history
                .iter()
                .position(|event| &event.event_id == id)
                .context("shared request lost its settlement")?;
            let persisted = &history[position];
            ensure!(
                persisted.kind == TrajectoryEventKind::RequestSettled
                    && persisted.request_id.as_deref() == Some(request_id)
                    && persisted.episode_id == request.episode_id,
                "shared settlement pointer changed identity"
            );
            let rebuilt = build(&stored, &history[..position], persisted.sequence)?;
            store::validate_settlement(owner, &rebuilt)?;
            ensure!(
                rebuilt.event == *persisted
                    && request.status == store::request_status_name(rebuilt.status.clone())
                    && self
                        .outbox_matches(
                            tx,
                            owner,
                            request.settlement_outbox_id.as_deref(),
                            rebuilt.outbox.as_ref()
                        )
                        .await?,
                "conflicting shared trajectory settlement"
            );
            return Ok(rebuilt);
        }
        ensure!(
            episode.closed_at.is_none(),
            "shared trajectory episode is closed"
        );
        ensure!(
            request.status == store::request_status_name(RequestStatus::Started)
                && request.settlement_outbox_id.is_none(),
            "inconsistent shared request terminal state"
        );
        let settlement = build(&stored, &history, u64::try_from(episode.next_sequence)?)?;
        store::validate_settlement(owner, &settlement)?;
        ensure!(
            settlement.event.request_id.as_deref() == Some(request_id)
                && settlement.event.episode_id == episode.episode_id,
            "shared settlement builder changed identity"
        );
        self.validate_append(&history, std::slice::from_ref(&settlement.event))?;
        self.insert_event(tx, &settlement.event).await?;
        let result = tx.execute(self.statement(tx,
            "UPDATE gateway_shared_trajectory_requests SET settlement_event_id=$4,settlement_outbox_id=$5,status=$6
             WHERE namespace=$1 AND owner_user_id=$2 AND request_id=$3 AND settlement_event_id IS NULL",
            vec![owner.into(), request_id.into(), settlement.event.event_id.clone().into(),
                 settlement.outbox.as_ref().map(|outbox| outbox.outbox_id.clone()).into(),
                 store::request_status_name(settlement.status.clone()).into()])?).await?;
        ensure!(
            result.rows_affected() == 1,
            "shared settlement request changed"
        );
        self.advance(tx, &episode, &settlement.event, None).await?;
        if let Some(outbox) = &settlement.outbox {
            self.insert_outbox(tx, owner, outbox).await?;
        }
        Ok(settlement)
    }

    fn validate_append(
        &self,
        history: &[TrajectoryEvent],
        events: &[TrajectoryEvent],
    ) -> Result<()> {
        ensure!(
            history
                .len()
                .checked_add(events.len())
                .context("shared history count overflow")?
                <= MAX_HISTORY_EVENTS,
            "shared trajectory history event limit exceeded"
        );
        let mut bytes = 0usize;
        for event in history.iter().chain(events) {
            validate_event(event)?;
            bytes = bytes
                .checked_add(serde_json::to_vec(event)?.len())
                .context("shared history size overflow")?;
            ensure!(
                bytes <= MAX_HISTORY_BYTES,
                "shared trajectory history byte limit exceeded"
            );
        }
        let all = history.iter().chain(events).cloned().collect::<Vec<_>>();
        super::super::health::reduce(&all, &Default::default())?;
        Ok(())
    }

    async fn insert_event(&self, tx: &DatabaseTransaction, event: &TrajectoryEvent) -> Result<()> {
        tx.execute(self.statement(tx,
            "INSERT INTO gateway_shared_trajectory_events
             (namespace,event_id,owner_user_id,episode_id,request_id,sequence,kind,event_json,content_digest,captured_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
            vec![event.event_id.clone().into(), event.owner_user_id.clone().into(), event.episode_id.clone().into(),
                 event.request_id.clone().into(), i64::try_from(event.sequence)?.into(), store::event_kind_name(event.kind).into(),
                 serde_json::to_string(event)?.into(), event.content_digest.clone().into(), event.captured_at.clone().into()])?).await?;
        Ok(())
    }

    async fn advance(
        &self,
        tx: &DatabaseTransaction,
        episode: &episode_entity::Model,
        event: &TrajectoryEvent,
        latest: Option<&str>,
    ) -> Result<()> {
        let next = i64::try_from(event.sequence)?
            .checked_add(1)
            .context("shared trajectory sequence overflow")?;
        let result = tx.execute(self.statement(tx,
            "UPDATE gateway_shared_trajectory_episodes SET next_sequence=$5,last_captured_at=$6,
                latest_request_id=COALESCE($7,latest_request_id)
             WHERE namespace=$1 AND owner_user_id=$2 AND episode_id=$3 AND next_sequence=$4 AND closed_at IS NULL",
            vec![episode.owner_user_id.clone().into(), episode.episode_id.clone().into(), episode.next_sequence.into(),
                 next.into(), event.captured_at.clone().into(), latest.map(str::to_owned).into()])?).await?;
        ensure!(
            result.rows_affected() == 1,
            "shared trajectory head changed"
        );
        Ok(())
    }

    async fn insert_outbox(
        &self,
        tx: &DatabaseTransaction,
        owner: &str,
        outbox: &OutboxWrite,
    ) -> Result<()> {
        store::validate_outbox(outbox)?;
        let body = serde_json::to_string(&outbox.payload)?;
        ensure!(
            body.len() <= MAX_OUTBOX_BYTES,
            "shared trajectory outbox byte limit exceeded"
        );
        tx.execute(self.statement(tx,
            "INSERT INTO gateway_shared_trajectory_outbox
             (namespace,outbox_id,owner_user_id,topic,payload_json,payload_digest,attempts,created_at,delivered_at)
             VALUES ($1,$2,$3,$4,$5,$6,0,$7,NULL)",
            vec![outbox.outbox_id.clone().into(), owner.into(), outbox.topic.clone().into(), body.into(),
                 canonical_digest(&outbox.payload)?.into(), outbox.created_at.clone().into()])?).await?;
        Ok(())
    }

    async fn outbox_matches(
        &self,
        tx: &DatabaseTransaction,
        owner: &str,
        id: Option<&str>,
        supplied: Option<&OutboxWrite>,
    ) -> Result<bool> {
        match (id, supplied) {
            (None, None) => Ok(true),
            (Some(id), Some(supplied)) if id == supplied.outbox_id => {
                let Some(stored) = self.outbox_in(tx, owner, id).await? else {
                    return Ok(false);
                };
                Ok(stored.topic == supplied.topic
                    && stored.payload_json == serde_json::to_string(&supplied.payload)?
                    && stored.created_at == supplied.created_at)
            }
            _ => Ok(false),
        }
    }
}
