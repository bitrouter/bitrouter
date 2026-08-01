use anyhow::{Context, Result};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DatabaseTransaction, EntityTrait,
    IntoActiveModel, QueryFilter, QueryOrder, Set, TransactionTrait,
};

use super::correlation::CorrelationSource;
use super::types::{
    BeginRequest, EpisodeStart, HistoryCompleteness, KeyedDigest, OutboxWrite, PendingOutbox,
    RequestStatus, Settlement, StoredRequest, TRAJECTORY_SCHEMA_VERSION, TrajectoryEvent,
    TrajectoryEventKind, TrajectoryEvidence, canonical_digest, validate_event,
    validate_keyed_component, validate_outbox_payload,
};

mod episode_entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "trajectory_episodes")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub episode_id: String,
        pub owner_user_id: String,
        pub correlation_digest: String,
        pub correlation_key_id: String,
        pub correlation_source: String,
        pub history_completeness: String,
        pub next_sequence: i64,
        pub first_captured_at: String,
        pub last_captured_at: String,
        pub closed_at: Option<String>,
        pub latest_request_id: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

mod event_entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "trajectory_events")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub event_id: String,
        pub owner_user_id: String,
        pub episode_id: String,
        pub request_id: Option<String>,
        pub sequence: i64,
        pub kind: String,
        pub event_json: String,
        pub content_digest: String,
        pub captured_at: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

mod request_entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "trajectory_requests")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub request_id: String,
        pub owner_user_id: String,
        pub episode_id: String,
        pub start_event_id: String,
        pub settlement_event_id: Option<String>,
        pub settlement_outbox_id: Option<String>,
        pub full_input_digest: String,
        pub native_parent_id: Option<String>,
        pub protocol: String,
        pub status: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

mod outbox_entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "trajectory_outbox")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub outbox_id: String,
        pub owner_user_id: String,
        pub topic: String,
        pub payload_json: String,
        pub payload_digest: String,
        pub attempts: i64,
        pub created_at: String,
        pub delivered_at: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

#[derive(Clone)]
pub struct TrajectoryStore {
    db: DatabaseConnection,
}

pub(crate) struct CorrelateAndBegin {
    pub request_id: String,
    pub new_episode_id: String,
    pub event_id: String,
    pub correlation_key_id: String,
    pub native_parent_id: Option<String>,
    pub full_input_digest: KeyedDigest,
    pub ancestor_prefix_digests: Vec<KeyedDigest>,
    pub starts_with_prior_turns: bool,
    pub protocol: String,
    pub captured_at: String,
}

pub(crate) struct CorrelateAndBeginResult {
    pub episode_id: String,
    pub source: CorrelationSource,
    pub completeness: HistoryCompleteness,
    pub prior_events: Vec<TrajectoryEvent>,
}

impl TrajectoryStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn begin_request(&self, owner_user_id: &str, input: BeginRequest) -> Result<()> {
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

    pub(crate) async fn correlate_and_begin(
        &self,
        owner_user_id: &str,
        input: CorrelateAndBegin,
    ) -> Result<CorrelateAndBeginResult> {
        validate_owner(owner_user_id)?;
        validate_keyed_component(&input.correlation_key_id, "correlation_key_id")?;
        if input.correlation_key_id != input.full_input_digest.key_id() {
            anyhow::bail!("correlation key id must match the full input digest")
        }
        if input
            .ancestor_prefix_digests
            .iter()
            .any(|digest| digest.key_id() != input.correlation_key_id)
        {
            anyhow::bail!("ancestor prefix digest uses a different correlation key")
        }

        let txn = self.db.begin().await?;
        let mut trusted_native_parent_id = None;
        let mut resolved_episode = None;
        let (source, completeness) = if let Some(native_parent_id) = &input.native_parent_id {
            match request_entity::Entity::find_by_id(native_parent_id)
                .one(&txn)
                .await?
            {
                Some(parent) if parent.owner_user_id == owner_user_id => {
                    trusted_native_parent_id = Some(native_parent_id.clone());
                    let episode = owned_episode(&txn, owner_user_id, &parent.episode_id).await?;
                    let completeness = parse_completeness(&episode.history_completeness)?;
                    resolved_episode = Some(episode);
                    (CorrelationSource::NativeParentId, completeness)
                }
                Some(_) | None => (
                    CorrelationSource::Unresolved,
                    HistoryCompleteness::Incomplete,
                ),
            }
        } else if let Some(episode) =
            find_prefix_episode(&txn, owner_user_id, &input.ancestor_prefix_digests).await?
        {
            let completeness = parse_completeness(&episode.history_completeness)?;
            resolved_episode = Some(episode);
            (CorrelationSource::CanonicalPrefix, completeness)
        } else if input.starts_with_prior_turns {
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

        let prior_events = match &resolved_episode {
            Some(episode) => {
                events_for_episode_in_tx(&txn, owner_user_id, &episode.episode_id).await?
            }
            None => Vec::new(),
        };
        let (episode_id, episode_start, sequence) = match resolved_episode {
            Some(episode) => {
                let episode_id = episode.episode_id.clone();
                let sequence = u64::try_from(episode.next_sequence)
                    .context("trajectory episode sequence is negative")?;
                let episode_start = EpisodeStart {
                    episode_id: episode_id.clone(),
                    correlation_digest: KeyedDigest::parse(episode.correlation_digest)?,
                    correlation_key_id: episode.correlation_key_id,
                    correlation_source: episode.correlation_source,
                    completeness: parse_completeness(&episode.history_completeness)?,
                };
                (episode_id, episode_start, sequence)
            }
            None => {
                let episode_id = input.new_episode_id.clone();
                let episode_start = EpisodeStart {
                    episode_id: episode_id.clone(),
                    correlation_digest: input.full_input_digest.clone(),
                    correlation_key_id: input.correlation_key_id.clone(),
                    correlation_source: source.as_str().to_owned(),
                    completeness,
                };
                (episode_id, episode_start, 1)
            }
        };
        let mut event = TrajectoryEvent {
            schema_version: TRAJECTORY_SCHEMA_VERSION,
            event_id: input.event_id,
            owner_user_id: owner_user_id.to_owned(),
            episode_id: episode_id.clone(),
            request_id: Some(input.request_id.clone()),
            sequence,
            kind: TrajectoryEventKind::RequestStarted,
            evidence: TrajectoryEvidence {
                structural: std::collections::BTreeMap::from([
                    (
                        "correlation.ancestor_prefix_count".to_owned(),
                        u64::try_from(input.ancestor_prefix_digests.len())
                            .context("too many ancestor prefix digests")?,
                    ),
                    (
                        "correlation.starts_with_prior_turns".to_owned(),
                        u64::from(input.starts_with_prior_turns),
                    ),
                ]),
                categorical: std::collections::BTreeMap::from([
                    ("correlation.source".to_owned(), source.as_str().to_owned()),
                    (
                        "history.completeness".to_owned(),
                        completeness_name(completeness).to_owned(),
                    ),
                ]),
                digests: std::collections::BTreeMap::new(),
            },
            captured_at: input.captured_at,
            content_digest: String::new(),
        };
        event.content_digest = event.semantic_digest()?;
        let begin = BeginRequest {
            episode: episode_start,
            event,
            full_input_digest: input.full_input_digest,
            native_parent_id: trusted_native_parent_id,
            protocol: input.protocol,
        };
        validate_begin(owner_user_id, &begin)?;
        begin_request_in_tx(&txn, owner_user_id, &input.request_id, begin).await?;
        txn.commit().await?;
        Ok(CorrelateAndBeginResult {
            episode_id,
            source,
            completeness,
            prior_events,
        })
    }

    pub async fn append_route_intent(
        &self,
        owner_user_id: &str,
        event: TrajectoryEvent,
    ) -> Result<()> {
        validate_owner(owner_user_id)?;
        validate_event(&event)?;
        if event.kind != TrajectoryEventKind::RouteIntentRecorded
            || event.owner_user_id != owner_user_id
        {
            anyhow::bail!("route intent event must belong to its owner")
        }
        let txn = self.db.begin().await?;
        if event_matches_existing(&txn, &event).await? {
            txn.commit().await?;
            return Ok(());
        }
        let episode = owned_episode(&txn, owner_user_id, &event.episode_id).await?;
        validate_sequence(&episode, &event)?;
        validate_event_request(&txn, owner_user_id, &event).await?;
        append_event(&txn, &event).await?;
        update_episode_head(&txn, episode, &event, None).await?;
        txn.commit().await?;
        Ok(())
    }

    pub async fn settle_request(&self, owner_user_id: &str, settlement: Settlement) -> Result<()> {
        validate_owner(owner_user_id)?;
        validate_settlement(owner_user_id, &settlement)?;
        let txn = self.db.begin().await?;
        let request_id = settlement.event.request_id.as_deref().unwrap_or_default();
        let request = request_entity::Entity::find()
            .filter(request_entity::Column::OwnerUserId.eq(owner_user_id))
            .filter(request_entity::Column::RequestId.eq(request_id))
            .one(&txn)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("unknown owner-scoped trajectory request '{request_id}'")
            })?;
        if request.settlement_event_id.is_some() {
            if request.settlement_event_id.as_deref() == Some(settlement.event.event_id.as_str())
                && event_matches_existing(&txn, &settlement.event).await?
                && request.status == request_status_name(settlement.status)
                && settlement_outbox_matches(
                    &txn,
                    owner_user_id,
                    request.settlement_outbox_id.as_deref(),
                    settlement.outbox.as_ref(),
                )
                .await?
            {
                txn.commit().await?;
                return Ok(());
            }
            anyhow::bail!("trajectory request '{request_id}' already has a different settlement")
        }
        if request.episode_id != settlement.event.episode_id {
            anyhow::bail!("settlement event belongs to a different episode")
        }
        let episode = owned_episode(&txn, owner_user_id, &settlement.event.episode_id).await?;
        validate_sequence(&episode, &settlement.event)?;
        append_event(&txn, &settlement.event).await?;

        let mut active = request.into_active_model();
        active.settlement_event_id = Set(Some(settlement.event.event_id.clone()));
        active.settlement_outbox_id = Set(settlement
            .outbox
            .as_ref()
            .map(|outbox| outbox.outbox_id.clone()));
        active.status = Set(request_status_name(settlement.status).into());
        active.update(&txn).await?;
        update_episode_head(
            &txn,
            episode,
            &settlement.event,
            settlement.event.request_id.clone(),
        )
        .await?;
        if let Some(outbox) = settlement.outbox {
            insert_outbox(&txn, owner_user_id, outbox).await?;
        }
        txn.commit().await?;
        Ok(())
    }

    pub async fn events_for_episode(
        &self,
        owner_user_id: &str,
        episode_id: &str,
    ) -> Result<Vec<TrajectoryEvent>> {
        validate_owner(owner_user_id)?;
        let rows = event_entity::Entity::find()
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

    pub async fn request(
        &self,
        owner_user_id: &str,
        request_id: &str,
    ) -> Result<Option<StoredRequest>> {
        validate_owner(owner_user_id)?;
        request_entity::Entity::find()
            .filter(request_entity::Column::OwnerUserId.eq(owner_user_id))
            .filter(request_entity::Column::RequestId.eq(request_id))
            .one(&self.db)
            .await?
            .map(stored_request)
            .transpose()
    }

    pub async fn pending_outbox(&self, owner_user_id: &str) -> Result<Vec<PendingOutbox>> {
        validate_owner(owner_user_id)?;
        outbox_entity::Entity::find()
            .filter(outbox_entity::Column::OwnerUserId.eq(owner_user_id))
            .filter(outbox_entity::Column::DeliveredAt.is_null())
            .order_by_asc(outbox_entity::Column::CreatedAt)
            .all(&self.db)
            .await?
            .into_iter()
            .map(stored_outbox)
            .collect()
    }

    pub async fn mark_outbox_delivered(
        &self,
        owner_user_id: &str,
        outbox_id: &str,
        delivered_at: &str,
    ) -> Result<()> {
        validate_owner(owner_user_id)?;
        validate_timestamp(delivered_at, "delivered_at")?;
        let Some(row) = outbox_entity::Entity::find()
            .filter(outbox_entity::Column::OwnerUserId.eq(owner_user_id))
            .filter(outbox_entity::Column::OutboxId.eq(outbox_id))
            .one(&self.db)
            .await?
        else {
            anyhow::bail!("unknown owner-scoped trajectory outbox '{outbox_id}'")
        };
        let mut active = row.into_active_model();
        active.delivered_at = Set(Some(delivered_at.to_owned()));
        active.update(&self.db).await?;
        Ok(())
    }
}

async fn begin_request_in_tx(
    txn: &DatabaseTransaction,
    owner_user_id: &str,
    request_id: &str,
    input: BeginRequest,
) -> Result<()> {
    validate_native_parent(txn, owner_user_id, input.native_parent_id.as_deref()).await?;
    if let Some(existing) = request_entity::Entity::find()
        .filter(request_entity::Column::OwnerUserId.eq(owner_user_id))
        .filter(request_entity::Column::RequestId.eq(request_id))
        .one(txn)
        .await?
    {
        let episode = owned_episode(txn, owner_user_id, &existing.episode_id).await?;
        let start_event = owned_event(txn, owner_user_id, &existing.start_event_id).await?;
        if begin_matches(&existing, &episode, &start_event, &input) {
            return Ok(());
        }
        anyhow::bail!(
            "trajectory request '{}' already exists with different content",
            existing.request_id
        )
    }
    if request_entity::Entity::find_by_id(request_id)
        .one(txn)
        .await?
        .is_some()
    {
        anyhow::bail!("trajectory request id is already owned by another user")
    }

    let episode = episode_entity::Entity::find()
        .filter(episode_entity::Column::OwnerUserId.eq(owner_user_id))
        .filter(episode_entity::Column::EpisodeId.eq(&input.episode.episode_id))
        .one(txn)
        .await?;
    match episode {
        Some(existing) => {
            validate_existing_episode(&existing, &input.episode, &input.event)?;
            append_event(txn, &input.event).await?;
            update_episode_head(txn, existing, &input.event, input.event.request_id.clone())
                .await?;
        }
        None => {
            if episode_entity::Entity::find_by_id(&input.episode.episode_id)
                .one(txn)
                .await?
                .is_some()
            {
                anyhow::bail!("trajectory episode is owned by another user")
            }
            if input.event.sequence != 1 {
                anyhow::bail!("first trajectory event must use sequence 1")
            }
            append_event(txn, &input.event).await?;
            episode_entity::ActiveModel {
                episode_id: Set(input.episode.episode_id.clone()),
                owner_user_id: Set(owner_user_id.to_owned()),
                correlation_digest: Set(input.episode.correlation_digest.as_str().to_owned()),
                correlation_key_id: Set(input.episode.correlation_key_id.clone()),
                correlation_source: Set(input.episode.correlation_source.clone()),
                history_completeness: Set(completeness_name(input.episode.completeness).into()),
                next_sequence: Set(2),
                first_captured_at: Set(input.event.captured_at.clone()),
                last_captured_at: Set(input.event.captured_at.clone()),
                closed_at: Set(None),
                latest_request_id: Set(input.event.request_id.clone()),
            }
            .insert(txn)
            .await?;
        }
    }
    request_entity::ActiveModel {
        request_id: Set(request_id.to_owned()),
        owner_user_id: Set(owner_user_id.to_owned()),
        episode_id: Set(input.episode.episode_id),
        start_event_id: Set(input.event.event_id),
        settlement_event_id: Set(None),
        settlement_outbox_id: Set(None),
        full_input_digest: Set(input.full_input_digest.as_str().to_owned()),
        native_parent_id: Set(input.native_parent_id),
        protocol: Set(input.protocol),
        status: Set(request_status_name(RequestStatus::Started).into()),
    }
    .insert(txn)
    .await?;
    Ok(())
}

async fn find_prefix_episode(
    txn: &DatabaseTransaction,
    owner_user_id: &str,
    ancestor_prefix_digests: &[KeyedDigest],
) -> Result<Option<episode_entity::Model>> {
    for digest in ancestor_prefix_digests.iter().rev() {
        let request = request_entity::Entity::find()
            .filter(request_entity::Column::OwnerUserId.eq(owner_user_id))
            .filter(request_entity::Column::FullInputDigest.eq(digest.as_str()))
            .order_by_desc(request_entity::Column::RequestId)
            .one(txn)
            .await?;
        if let Some(request) = request {
            return owned_episode(txn, owner_user_id, &request.episode_id)
                .await
                .map(Some);
        }
    }
    Ok(None)
}

async fn events_for_episode_in_tx(
    txn: &DatabaseTransaction,
    owner_user_id: &str,
    episode_id: &str,
) -> Result<Vec<TrajectoryEvent>> {
    let rows = event_entity::Entity::find()
        .filter(event_entity::Column::OwnerUserId.eq(owner_user_id))
        .filter(event_entity::Column::EpisodeId.eq(episode_id))
        .order_by_asc(event_entity::Column::Sequence)
        .all(txn)
        .await?;
    decode_event_history(rows)
}

fn decode_event_history(rows: Vec<event_entity::Model>) -> Result<Vec<TrajectoryEvent>> {
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

async fn owned_episode(
    txn: &DatabaseTransaction,
    owner_user_id: &str,
    episode_id: &str,
) -> Result<episode_entity::Model> {
    episode_entity::Entity::find()
        .filter(episode_entity::Column::OwnerUserId.eq(owner_user_id))
        .filter(episode_entity::Column::EpisodeId.eq(episode_id))
        .one(txn)
        .await?
        .ok_or_else(|| anyhow::anyhow!("unknown owner-scoped trajectory episode '{episode_id}'"))
}

async fn append_event(txn: &DatabaseTransaction, event: &TrajectoryEvent) -> Result<()> {
    if event_entity::Entity::find()
        .filter(event_entity::Column::OwnerUserId.eq(&event.owner_user_id))
        .filter(event_entity::Column::EventId.eq(&event.event_id))
        .one(txn)
        .await?
        .is_some()
    {
        anyhow::bail!("trajectory event '{}' already exists", event.event_id)
    }
    if event_entity::Entity::find_by_id(&event.event_id)
        .one(txn)
        .await?
        .is_some()
    {
        anyhow::bail!("trajectory event id is already owned by another user")
    }
    event_entity::ActiveModel {
        event_id: Set(event.event_id.clone()),
        owner_user_id: Set(event.owner_user_id.clone()),
        episode_id: Set(event.episode_id.clone()),
        request_id: Set(event.request_id.clone()),
        sequence: Set(
            i64::try_from(event.sequence).context("trajectory sequence exceeds database range")?
        ),
        kind: Set(event_kind_name(event.kind).into()),
        event_json: Set(serde_json::to_string(event)?),
        content_digest: Set(event.content_digest.clone()),
        captured_at: Set(event.captured_at.clone()),
    }
    .insert(txn)
    .await
    .context("appending immutable trajectory event")?;
    Ok(())
}

async fn event_matches_existing(
    txn: &DatabaseTransaction,
    event: &TrajectoryEvent,
) -> Result<bool> {
    let existing = event_entity::Entity::find()
        .filter(event_entity::Column::OwnerUserId.eq(&event.owner_user_id))
        .filter(event_entity::Column::EventId.eq(&event.event_id))
        .one(txn)
        .await?;
    if let Some(existing) = existing {
        if existing.content_digest == event.content_digest {
            return Ok(true);
        }
        anyhow::bail!(
            "trajectory event '{}' already exists with different content",
            event.event_id
        )
    }
    if event_entity::Entity::find_by_id(&event.event_id)
        .one(txn)
        .await?
        .is_some()
    {
        anyhow::bail!("trajectory event id is already owned by another user")
    }
    Ok(false)
}

fn validate_sequence(episode: &episode_entity::Model, event: &TrajectoryEvent) -> Result<()> {
    let expected =
        u64::try_from(episode.next_sequence).context("stored trajectory sequence is negative")?;
    if event.sequence != expected {
        anyhow::bail!(
            "trajectory event sequence must be {expected} for episode '{}'",
            event.episode_id
        )
    }
    Ok(())
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

async fn validate_native_parent(
    txn: &DatabaseTransaction,
    owner_user_id: &str,
    native_parent_id: Option<&str>,
) -> Result<()> {
    let Some(native_parent_id) = native_parent_id else {
        return Ok(());
    };
    if request_entity::Entity::find()
        .filter(request_entity::Column::OwnerUserId.eq(owner_user_id))
        .filter(request_entity::Column::RequestId.eq(native_parent_id))
        .one(txn)
        .await?
        .is_some()
    {
        return Ok(());
    }
    if request_entity::Entity::find_by_id(native_parent_id)
        .one(txn)
        .await?
        .is_some()
    {
        anyhow::bail!("native parent belongs to another owner")
    }
    Ok(())
}

async fn update_episode_head(
    txn: &DatabaseTransaction,
    episode: episode_entity::Model,
    event: &TrajectoryEvent,
    latest_request_id: Option<String>,
) -> Result<()> {
    let mut active = episode.into_active_model();
    active.next_sequence = Set(i64::try_from(
        event
            .sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("trajectory sequence overflow"))?,
    )
    .context("trajectory sequence exceeds database range")?);
    active.last_captured_at = Set(event.captured_at.clone());
    if latest_request_id.is_some() {
        active.latest_request_id = Set(latest_request_id);
    }
    active.update(txn).await?;
    Ok(())
}

fn validate_begin(owner_user_id: &str, input: &BeginRequest) -> Result<()> {
    validate_event(&input.event)?;
    if input.event.owner_user_id != owner_user_id
        || input.event.episode_id != input.episode.episode_id
        || input.event.kind != TrajectoryEventKind::RequestStarted
        || input.event.request_id.is_none()
    {
        anyhow::bail!(
            "request start event must be owner-scoped and identify its episode and request"
        )
    }
    validate_episode_start(&input.episode)?;
    if let Some(native_parent_id) = &input.native_parent_id {
        validate_opaque(native_parent_id, "native_parent_id")?;
    }
    validate_opaque(&input.protocol, "protocol")?;
    Ok(())
}

fn validate_settlement(owner_user_id: &str, settlement: &Settlement) -> Result<()> {
    validate_event(&settlement.event)?;
    if settlement.event.owner_user_id != owner_user_id
        || settlement.event.kind != TrajectoryEventKind::RequestSettled
        || settlement.event.request_id.is_none()
        || settlement.status == RequestStatus::Started
    {
        anyhow::bail!("request settlement event must be owner-scoped and identify its request")
    }
    if let Some(outbox) = &settlement.outbox {
        validate_outbox(outbox)?;
    }
    Ok(())
}

fn validate_existing_episode(
    existing: &episode_entity::Model,
    episode: &EpisodeStart,
    event: &TrajectoryEvent,
) -> Result<()> {
    if existing.correlation_digest != episode.correlation_digest.as_str()
        || existing.correlation_key_id != episode.correlation_key_id
        || existing.correlation_source != episode.correlation_source
    {
        anyhow::bail!("trajectory episode correlation cannot be replaced")
    }
    validate_sequence(existing, event)
}

fn validate_episode_start(episode: &EpisodeStart) -> Result<()> {
    validate_opaque(&episode.episode_id, "episode_id")?;
    validate_keyed_component(&episode.correlation_key_id, "correlation_key_id")?;
    if episode.correlation_key_id != episode.correlation_digest.key_id() {
        anyhow::bail!("correlation key id must match the keyed correlation digest")
    }
    validate_opaque(&episode.correlation_source, "correlation_source")
}

fn validate_outbox(outbox: &OutboxWrite) -> Result<()> {
    validate_opaque(&outbox.outbox_id, "outbox_id")?;
    validate_opaque(&outbox.topic, "outbox.topic")?;
    validate_timestamp(&outbox.created_at, "outbox.created_at")?;
    validate_outbox_payload(&outbox.payload)?;
    Ok(())
}

async fn insert_outbox(
    txn: &DatabaseTransaction,
    owner_user_id: &str,
    outbox: OutboxWrite,
) -> Result<()> {
    let payload_json = serde_json::to_string(&outbox.payload)?;
    let payload_digest = canonical_digest(&outbox.payload)?;
    outbox_entity::ActiveModel {
        outbox_id: Set(outbox.outbox_id),
        owner_user_id: Set(owner_user_id.to_owned()),
        topic: Set(outbox.topic),
        payload_json: Set(payload_json),
        payload_digest: Set(payload_digest),
        attempts: Set(0),
        created_at: Set(outbox.created_at),
        delivered_at: Set(None),
    }
    .insert(txn)
    .await
    .context("inserting trajectory outbox item")?;
    Ok(())
}

fn begin_matches(
    existing: &request_entity::Model,
    episode: &episode_entity::Model,
    start_event: &TrajectoryEvent,
    input: &BeginRequest,
) -> bool {
    existing.episode_id == input.episode.episode_id
        && existing.start_event_id == input.event.event_id
        && existing.full_input_digest == input.full_input_digest.as_str()
        && existing.native_parent_id == input.native_parent_id
        && existing.protocol == input.protocol
        && existing.status == request_status_name(RequestStatus::Started)
        && episode.correlation_digest == input.episode.correlation_digest.as_str()
        && episode.correlation_key_id == input.episode.correlation_key_id
        && episode.correlation_source == input.episode.correlation_source
        && episode.history_completeness == completeness_name(input.episode.completeness)
        && start_event == &input.event
}

fn stored_event(row: event_entity::Model) -> Result<TrajectoryEvent> {
    let event: TrajectoryEvent =
        serde_json::from_str(&row.event_json).context("decoding stored trajectory event")?;
    validate_event(&event)?;
    if event.content_digest != row.content_digest {
        anyhow::bail!("stored trajectory event digest disagrees with indexed digest")
    }
    if event.event_id != row.event_id
        || event.owner_user_id != row.owner_user_id
        || event.episode_id != row.episode_id
        || event.request_id != row.request_id
        || event.sequence
            != u64::try_from(row.sequence).context("stored trajectory sequence is negative")?
        || event_kind_name(event.kind) != row.kind
        || event.captured_at != row.captured_at
    {
        anyhow::bail!("stored trajectory event index disagrees with canonical event")
    }
    Ok(event)
}

async fn owned_event(
    txn: &DatabaseTransaction,
    owner_user_id: &str,
    event_id: &str,
) -> Result<TrajectoryEvent> {
    let row = event_entity::Entity::find()
        .filter(event_entity::Column::OwnerUserId.eq(owner_user_id))
        .filter(event_entity::Column::EventId.eq(event_id))
        .one(txn)
        .await?
        .ok_or_else(|| anyhow::anyhow!("unknown owner-scoped trajectory event '{event_id}'"))?;
    stored_event(row)
}

async fn settlement_outbox_matches(
    txn: &DatabaseTransaction,
    owner_user_id: &str,
    stored_outbox_id: Option<&str>,
    supplied_outbox: Option<&OutboxWrite>,
) -> Result<bool> {
    match (stored_outbox_id, supplied_outbox) {
        (None, None) => Ok(true),
        (Some(_), None) | (None, Some(_)) => Ok(false),
        (Some(stored_outbox_id), Some(supplied_outbox))
            if stored_outbox_id == supplied_outbox.outbox_id =>
        {
            let row = outbox_entity::Entity::find()
                .filter(outbox_entity::Column::OwnerUserId.eq(owner_user_id))
                .filter(outbox_entity::Column::OutboxId.eq(stored_outbox_id))
                .one(txn)
                .await?;
            let Some(row) = row else {
                return Ok(false);
            };
            Ok(row.topic == supplied_outbox.topic
                && row.payload_json == serde_json::to_string(&supplied_outbox.payload)?
                && row.payload_digest == canonical_digest(&supplied_outbox.payload)?
                && row.created_at == supplied_outbox.created_at)
        }
        (Some(_), Some(_)) => Ok(false),
    }
}

fn stored_request(row: request_entity::Model) -> Result<StoredRequest> {
    Ok(StoredRequest {
        request_id: row.request_id,
        episode_id: row.episode_id,
        start_event_id: row.start_event_id,
        settlement_event_id: row.settlement_event_id,
        full_input_digest: KeyedDigest::parse(row.full_input_digest)
            .context("stored trajectory request has invalid keyed full-input digest")?,
        native_parent_id: row.native_parent_id,
        protocol: row.protocol,
        status: parse_request_status(&row.status)?,
    })
}

fn stored_outbox(row: outbox_entity::Model) -> Result<PendingOutbox> {
    Ok(PendingOutbox {
        outbox_id: row.outbox_id,
        topic: row.topic,
        payload_json: row.payload_json,
        payload_digest: row.payload_digest,
        attempts: u64::try_from(row.attempts).context("stored outbox attempts are negative")?,
        created_at: row.created_at,
    })
}

fn validate_owner(owner_user_id: &str) -> Result<()> {
    validate_opaque(owner_user_id, "owner_user_id")
}

fn validate_opaque(value: &str, field: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        anyhow::bail!("{field} must be a non-empty bounded value")
    }
    Ok(())
}

fn validate_timestamp(value: &str, field: &str) -> Result<()> {
    chrono::DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("{field} must be RFC3339"))?;
    Ok(())
}

fn event_kind_name(kind: TrajectoryEventKind) -> &'static str {
    match kind {
        TrajectoryEventKind::RequestStarted => "request_started",
        TrajectoryEventKind::RouteIntentRecorded => "route_intent_recorded",
        TrajectoryEventKind::RequestSettled => "request_settled",
        TrajectoryEventKind::GuardActivated => "guard_activated",
        TrajectoryEventKind::EpisodeClosed => "episode_closed",
    }
}

fn completeness_name(completeness: HistoryCompleteness) -> &'static str {
    match completeness {
        HistoryCompleteness::Complete => "complete",
        HistoryCompleteness::Incomplete => "incomplete",
        HistoryCompleteness::Unknown => "unknown",
    }
}

fn parse_completeness(value: &str) -> Result<HistoryCompleteness> {
    match value {
        "complete" => Ok(HistoryCompleteness::Complete),
        "incomplete" => Ok(HistoryCompleteness::Incomplete),
        "unknown" => Ok(HistoryCompleteness::Unknown),
        _ => anyhow::bail!("stored trajectory episode has invalid history completeness"),
    }
}

fn request_status_name(status: RequestStatus) -> &'static str {
    match status {
        RequestStatus::Started => "started",
        RequestStatus::Settled => "settled",
        RequestStatus::Failed => "failed",
    }
}

fn parse_request_status(value: &str) -> Result<RequestStatus> {
    match value {
        "started" => Ok(RequestStatus::Started),
        "settled" => Ok(RequestStatus::Settled),
        "failed" => Ok(RequestStatus::Failed),
        _ => anyhow::bail!("stored trajectory request has invalid status"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

    use super::*;
    use crate::trajectory::types::*;

    #[tokio::test]
    async fn store_is_owner_scoped_and_rejects_cross_owner_episode_parentage() -> anyhow::Result<()>
    {
        let store = store().await?;
        store
            .begin_request("owner-a", begin("episode-1", "request-1"))
            .await?;

        assert!(
            store
                .events_for_episode("owner-b", "episode-1")
                .await?
                .is_empty()
        );
        let mut cross_owner = begin("episode-1", "request-2");
        cross_owner.event.owner_user_id = "owner-b".into();
        cross_owner.event.content_digest = cross_owner.event.semantic_digest()?;
        assert!(store.begin_request("owner-b", cross_owner).await.is_err());
        assert!(store.request("owner-b", "request-1").await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn store_rejects_duplicate_sequences_and_mutable_event_replacements() -> anyhow::Result<()>
    {
        let store = store().await?;
        store
            .begin_request("owner-a", begin("episode-1", "request-1"))
            .await?;

        let duplicate_sequence = event(
            "event-route-1",
            "episode-1",
            Some("request-1"),
            1,
            TrajectoryEventKind::RouteIntentRecorded,
        );
        assert!(
            store
                .append_route_intent("owner-a", duplicate_sequence)
                .await
                .is_err()
        );

        let original = event(
            "event-route-2",
            "episode-1",
            Some("request-1"),
            2,
            TrajectoryEventKind::RouteIntentRecorded,
        );
        store.append_route_intent("owner-a", original).await?;
        let mut replacement = event(
            "event-route-2",
            "episode-1",
            Some("request-1"),
            3,
            TrajectoryEventKind::RouteIntentRecorded,
        );
        replacement
            .evidence
            .structural
            .insert("route.changed".into(), 1);
        replacement.content_digest = replacement.semantic_digest()?;
        assert!(
            store
                .append_route_intent("owner-a", replacement)
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn identical_starts_and_settlements_are_idempotent_and_outbox_is_owner_scoped()
    -> anyhow::Result<()> {
        let store = store().await?;
        let start = begin("episode-1", "request-1");
        store.begin_request("owner-a", start.clone()).await?;
        store.begin_request("owner-a", start).await?;
        assert_eq!(
            store
                .events_for_episode("owner-a", "episode-1")
                .await?
                .len(),
            1
        );

        let settlement = settlement("episode-1", "request-1", "event-settle-1", "outbox-1");
        store.settle_request("owner-a", settlement.clone()).await?;
        store.settle_request("owner-a", settlement).await?;
        assert_eq!(
            store
                .events_for_episode("owner-a", "episode-1")
                .await?
                .len(),
            2
        );
        assert_eq!(store.pending_outbox("owner-a").await?.len(), 1);
        assert!(store.pending_outbox("owner-b").await?.is_empty());
        store
            .mark_outbox_delivered("owner-a", "outbox-1", "2026-08-01T00:02:00Z")
            .await?;
        assert!(store.pending_outbox("owner-a").await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn outbox_insert_failure_rolls_back_request_settlement_and_episode_head()
    -> anyhow::Result<()> {
        let store = store().await?;
        store
            .begin_request("owner-a", begin("episode-1", "request-1"))
            .await?;
        store
            .begin_request("owner-a", begin("episode-2", "request-2"))
            .await?;
        store
            .settle_request(
                "owner-a",
                settlement("episode-2", "request-2", "event-settle-2", "outbox-1"),
            )
            .await?;

        assert!(
            store
                .settle_request(
                    "owner-a",
                    settlement("episode-1", "request-1", "event-settle-1", "outbox-1")
                )
                .await
                .is_err()
        );
        let request = store
            .request("owner-a", "request-1")
            .await?
            .unwrap_or_else(|| unreachable!());
        assert_eq!(request.status, RequestStatus::Started);
        assert_eq!(
            store
                .events_for_episode("owner-a", "episode-1")
                .await?
                .len(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_start_requires_identical_event_and_episode_inputs() -> anyhow::Result<()> {
        let store = store().await?;
        let start = begin("episode-1", "request-1");
        store.begin_request("owner-a", start.clone()).await?;

        let mut changed_evidence = start.clone();
        changed_evidence
            .event
            .evidence
            .structural
            .insert("request.retry_count".into(), 2);
        changed_evidence.event.content_digest = changed_evidence.event.semantic_digest()?;
        assert!(
            store
                .begin_request("owner-a", changed_evidence)
                .await
                .is_err()
        );

        let mut changed_completeness = start;
        changed_completeness.episode.completeness = HistoryCompleteness::Unknown;
        assert!(
            store
                .begin_request("owner-a", changed_completeness)
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn duplicate_settlement_rejects_changed_outbox_or_started_status() -> anyhow::Result<()> {
        let first_store = store().await?;
        first_store
            .begin_request("owner-a", begin("episode-1", "request-1"))
            .await?;
        let settled = settlement("episode-1", "request-1", "event-settle-1", "outbox-1");
        first_store
            .settle_request("owner-a", settled.clone())
            .await?;

        let mut changed_outbox = settled.clone();
        if let Some(outbox) = &mut changed_outbox.outbox {
            outbox.topic = "trajectory.changed".into();
        }
        assert!(
            first_store
                .settle_request("owner-a", changed_outbox)
                .await
                .is_err()
        );

        Ok(())
    }

    #[tokio::test]
    async fn settlement_rejects_started_status() -> anyhow::Result<()> {
        let store = store().await?;
        store
            .begin_request("owner-a", begin("episode-1", "request-1"))
            .await?;
        let mut invalid_status = settlement("episode-1", "request-1", "event-settle-1", "outbox-1");
        invalid_status.status = RequestStatus::Started;
        assert!(
            store
                .settle_request("owner-a", invalid_status)
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn native_parent_cannot_reference_another_owners_request() -> anyhow::Result<()> {
        let store = store().await?;
        let mut parent = begin("episode-parent", "request-parent");
        parent.event.owner_user_id = "owner-b".into();
        parent.event.content_digest = parent.event.semantic_digest()?;
        store.begin_request("owner-b", parent).await?;

        let mut child = begin("episode-child", "request-child");
        child.native_parent_id = Some("request-parent".into());
        assert!(store.begin_request("owner-a", child).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn event_history_rejects_corrupt_index_columns_and_sequence_gaps() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = TrajectoryStore::new(db.clone());
        store
            .begin_request("owner-a", begin("episode-1", "request-1"))
            .await?;
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "UPDATE trajectory_events SET sequence = 2 WHERE event_id = 'event-start-request-1'"
                .to_owned(),
        ))
        .await?;

        assert!(
            store
                .events_for_episode("owner-a", "episode-1")
                .await
                .is_err()
        );
        Ok(())
    }

    async fn store() -> anyhow::Result<TrajectoryStore> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        Ok(TrajectoryStore::new(db))
    }

    fn begin(episode_id: &str, request_id: &str) -> BeginRequest {
        BeginRequest {
            episode: EpisodeStart {
                episode_id: episode_id.into(),
                correlation_digest: keyed_digest("key-1", "1"),
                correlation_key_id: "key-1".into(),
                correlation_source: "explicit_root".into(),
                completeness: HistoryCompleteness::Complete,
            },
            event: event(
                &format!("event-start-{request_id}"),
                episode_id,
                Some(request_id),
                1,
                TrajectoryEventKind::RequestStarted,
            ),
            full_input_digest: keyed_digest("key-1", "2"),
            native_parent_id: None,
            protocol: "responses".into(),
        }
    }

    fn settlement(
        episode_id: &str,
        request_id: &str,
        event_id: &str,
        outbox_id: &str,
    ) -> Settlement {
        Settlement {
            event: event(
                event_id,
                episode_id,
                Some(request_id),
                2,
                TrajectoryEventKind::RequestSettled,
            ),
            status: RequestStatus::Settled,
            outbox: Some(OutboxWrite {
                outbox_id: outbox_id.into(),
                topic: "trajectory.settled".into(),
                payload: OutboxPayload {
                    structural: BTreeMap::from([("trajectory.event_count".into(), 1)]),
                    digests: BTreeMap::from([(
                        "trajectory.event".into(),
                        event(
                            event_id,
                            episode_id,
                            Some(request_id),
                            2,
                            TrajectoryEventKind::RequestSettled,
                        )
                        .content_digest,
                    )]),
                },
                created_at: "2026-08-01T00:01:00Z".into(),
            }),
        }
    }

    fn event(
        event_id: &str,
        episode_id: &str,
        request_id: Option<&str>,
        sequence: u64,
        kind: TrajectoryEventKind,
    ) -> TrajectoryEvent {
        let mut event = TrajectoryEvent {
            schema_version: TRAJECTORY_SCHEMA_VERSION,
            event_id: event_id.into(),
            owner_user_id: "owner-a".into(),
            episode_id: episode_id.into(),
            request_id: request_id.map(ToOwned::to_owned),
            sequence,
            kind,
            evidence: TrajectoryEvidence {
                structural: BTreeMap::from([("request.input_count".into(), 1)]),
                categorical: BTreeMap::new(),
                digests: BTreeMap::new(),
            },
            captured_at: "2026-08-01T00:00:00Z".into(),
            content_digest: String::new(),
        };
        event.content_digest = event.semantic_digest().unwrap_or_default();
        event
    }

    fn keyed_digest(key_id: &str, digit: &str) -> KeyedDigest {
        KeyedDigest::parse(format!("hmac-sha256:{key_id}:{}", digit.repeat(64)))
            .unwrap_or_else(|_| unreachable!())
    }
}
