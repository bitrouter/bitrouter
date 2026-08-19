use anyhow::{Context, Result};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DatabaseTransaction, DbErr, EntityTrait,
    IntoActiveModel, PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, RuntimeErr, Set,
    TransactionTrait,
    sea_query::{Cond, Expr, OnConflict, Query, SelectStatement},
};

use super::correlation::CorrelationSource;
use super::evaluation::{TRAJECTORY_EVAL_TOPIC, build_operational_evaluation};
use super::guard::{
    ProgressGuardInput, ProgressGuardPolicy, RouteIntent, RouteIntentClause,
    RouteIntentClauseDisposition, evaluate, validate_persisted_route_intent,
};
use super::health::{PrefixReduction, reduce, reduce_prefix};
use super::types::{
    BeginRequest, EpisodeStart, HistoryCompleteness, KeyedDigest, OutboxWrite, PendingOutbox,
    RequestStatus, Settlement, StoredEpisode, StoredRequest, TRAJECTORY_SCHEMA_VERSION,
    TrajectoryEvent, TrajectoryEventKind, TrajectoryEvidence, TrajectorySnapshot, canonical_digest,
    validate_event, validate_keyed_component, validate_outbox_payload,
};
use crate::eval::types::EvalExperimentRef;
use crate::workflow_state::ir::RouteProjection;

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

mod prefix_entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "trajectory_prefix_index")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub owner_user_id: String,
        #[sea_orm(primary_key, auto_increment = false)]
        pub full_input_digest: String,
        pub episode_id: String,
        pub ambiguous: bool,
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
    pub native_parent_digest: Option<KeyedDigest>,
    pub full_input_digest: KeyedDigest,
    pub ancestor_prefix_digests: Vec<KeyedDigest>,
    pub ancestor_prefixes_truncated: bool,
    pub starts_with_prior_turns: bool,
    pub canonical_input_bytes: u64,
    pub protocol: String,
    pub captured_at: String,
    pub guarded_route: Option<GuardedRouteInput>,
}

pub(crate) struct CorrelateAndBeginResult {
    pub episode_id: String,
    pub source: CorrelationSource,
    pub completeness: HistoryCompleteness,
    pub prior_events: Vec<TrajectoryEvent>,
    pub guarded_route: Option<GuardedRouteResult>,
}

pub(crate) struct OutboxBatchItem {
    pub outbox_id: String,
    pub owner_user_id: String,
    pub topic: String,
    pub payload: Option<super::types::OutboxPayload>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct PruneSummary {
    pub delivered_outbox_rows: u64,
    pub episode_rows: u64,
    pub event_rows: u64,
    pub request_rows: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EpisodeAudit {
    Valid {
        episode: StoredEpisode,
        events: Vec<TrajectoryEvent>,
        snapshot: TrajectorySnapshot,
    },
    Corrupt {
        episode: StoredEpisode,
        event_id: Option<String>,
        sequence: Option<u64>,
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct GuardedRouteInput {
    pub route_event_id: String,
    pub guard_event_id: String,
    pub policy_name: String,
    pub request_key: String,
    pub baseline_tier: Option<String>,
    pub baseline_effort: Option<bitrouter_sdk::language_model::types::ReasoningEffort>,
    pub tier_efforts:
        std::collections::BTreeMap<String, bitrouter_sdk::language_model::types::ReasoningEffort>,
    pub preset: Option<String>,
    pub projection: RouteProjection,
    pub candidate_tier: Option<String>,
    pub policy_digest: String,
    pub experiment: Option<EvalExperimentRef>,
    pub policy: ProgressGuardPolicy,
    pub carries_tools: bool,
    pub tool_use_tier: Option<String>,
    pub tool_safe_tiers: std::collections::BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuardedRouteResult {
    pub snapshot: super::types::TrajectorySnapshot,
    pub intent: RouteIntent,
    pub guard_activated: bool,
    pub tool_floor_applied: bool,
    pub route_sequence: u64,
    pub causal_completeness: HistoryCompleteness,
}

struct GuardedRouteBatch {
    route_event: TrajectoryEvent,
    guard_event: Option<TrajectoryEvent>,
    result: GuardedRouteResult,
}

enum CorrelateAttempt {
    Complete(Box<CorrelateAndBeginResult>),
    RetrySequenceReservation,
    RetryDeterministicStart,
}

enum PrefixResolution {
    None,
    Unique(Box<episode_entity::Model>),
    Ambiguous,
}

const MAX_SEQUENCE_RESERVATION_ATTEMPTS: usize = 32;
const MAX_LEDGER_MUTATION_ATTEMPTS: usize = 32;
const MAX_AUDIT_READ_ATTEMPTS: usize = 32;
pub(crate) const MAX_OUTBOX_BATCH_SIZE: usize =
    bitrouter_sdk::config::MAX_TRAJECTORY_OUTBOX_BATCH_SIZE;

#[cfg(test)]
#[derive(Clone, Default)]
struct AuditReadProbe {
    first_read_barriers: Option<(
        std::sync::Arc<tokio::sync::Barrier>,
        std::sync::Arc<tokio::sync::Barrier>,
    )>,
    force_contention: bool,
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
        if input.native_parent_id.is_some() != input.native_parent_digest.is_some() {
            anyhow::bail!("native parent id and digest must be present together")
        }
        if input
            .native_parent_digest
            .as_ref()
            .is_some_and(|digest| digest.key_id() != input.correlation_key_id)
        {
            anyhow::bail!("native parent digest uses a different correlation key")
        }

        for attempt in 0..MAX_SEQUENCE_RESERVATION_ATTEMPTS {
            match self.correlate_and_begin_once(owner_user_id, &input).await {
                Ok(CorrelateAttempt::Complete(result)) => return Ok(*result),
                Ok(CorrelateAttempt::RetrySequenceReservation)
                | Ok(CorrelateAttempt::RetryDeterministicStart) => {
                    tokio::time::sleep(contention_backoff(attempt)).await;
                }
                Err(error) if classify_database_error(&error).retryable_contention => {
                    tokio::time::sleep(contention_backoff(attempt)).await;
                }
                Err(error) => return Err(error),
            }
        }
        anyhow::bail!(
            "trajectory episode sequence contention exhausted after {MAX_SEQUENCE_RESERVATION_ATTEMPTS} attempts"
        )
    }

    async fn correlate_and_begin_once(
        &self,
        owner_user_id: &str,
        input: &CorrelateAndBegin,
    ) -> Result<CorrelateAttempt> {
        let txn = self.db.begin().await?;
        if let Some(existing) = request_entity::Entity::find()
            .filter(request_entity::Column::OwnerUserId.eq(owner_user_id))
            .filter(request_entity::Column::RequestId.eq(&input.request_id))
            .one(&txn)
            .await?
        {
            let result = exact_retry_result(&txn, owner_user_id, &existing, input).await?;
            txn.commit().await?;
            return Ok(CorrelateAttempt::Complete(Box::new(result)));
        }
        if request_entity::Entity::find_by_id(&input.request_id)
            .one(&txn)
            .await?
            .is_some()
        {
            anyhow::bail!("trajectory request id is already owned by another user")
        }

        let mut trusted_native_parent_id = None;
        let mut resolved_episode = None;
        let mut prefix_conflict = false;
        let mut key_epoch_conflict = false;
        let (source, completeness) = if let Some(native_parent_id) = &input.native_parent_id {
            match native_parent_request(&txn, owner_user_id, native_parent_id).await? {
                Some(parent) => {
                    trusted_native_parent_id = Some(parent.request_id.clone());
                    let episode = owned_episode(&txn, owner_user_id, &parent.episode_id).await?;
                    if episode.correlation_key_id == input.correlation_key_id {
                        let prefix_resolution = find_prefix_episode(
                            &txn,
                            owner_user_id,
                            &input.ancestor_prefix_digests,
                        )
                        .await?;
                        prefix_conflict = match &prefix_resolution {
                            PrefixResolution::Unique(prefix_episode) => {
                                prefix_episode.episode_id != episode.episode_id
                            }
                            PrefixResolution::Ambiguous => true,
                            PrefixResolution::None => false,
                        };
                        let omitted_ancestry_unresolved = input.ancestor_prefixes_truncated
                            && matches!(prefix_resolution, PrefixResolution::None);
                        let completeness = if prefix_conflict || omitted_ancestry_unresolved {
                            HistoryCompleteness::Incomplete
                        } else {
                            parse_completeness(&episode.history_completeness)?
                        };
                        resolved_episode = Some(episode);
                        (CorrelationSource::NativeParentId, completeness)
                    } else {
                        key_epoch_conflict = true;
                        (
                            CorrelationSource::NativeParentId,
                            HistoryCompleteness::Incomplete,
                        )
                    }
                }
                None => (
                    CorrelationSource::Unresolved,
                    HistoryCompleteness::Incomplete,
                ),
            }
        } else {
            match find_prefix_episode(&txn, owner_user_id, &input.ancestor_prefix_digests).await? {
                PrefixResolution::Unique(episode) => {
                    let completeness = parse_completeness(&episode.history_completeness)?;
                    resolved_episode = Some(*episode);
                    (CorrelationSource::CanonicalPrefix, completeness)
                }
                PrefixResolution::Ambiguous => {
                    prefix_conflict = true;
                    (
                        CorrelationSource::Unresolved,
                        HistoryCompleteness::Incomplete,
                    )
                }
                PrefixResolution::None if input.starts_with_prior_turns => (
                    CorrelationSource::Unresolved,
                    HistoryCompleteness::Incomplete,
                ),
                PrefixResolution::None => (
                    CorrelationSource::ExplicitRoot,
                    HistoryCompleteness::Complete,
                ),
            }
        };

        let prior_events = match &resolved_episode {
            Some(episode) => {
                let events =
                    events_for_episode_in_tx(&txn, owner_user_id, &episode.episode_id).await?;
                validate_episode_head(episode, &events)?;
                if episode.closed_at.is_some() {
                    anyhow::bail!("trajectory episode is closed and cannot accept new events")
                }
                events
            }
            None => Vec::new(),
        };
        let captured_at = episode_monotonic_captured_at(&prior_events, &input.captured_at)?;
        let extends_existing_episode = resolved_episode.is_some();
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
            event_id: input.event_id.clone(),
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
                    (
                        "correlation.ancestor_prefixes_truncated".to_owned(),
                        u64::from(input.ancestor_prefixes_truncated),
                    ),
                    (
                        "correlation.prefix_conflict".to_owned(),
                        u64::from(prefix_conflict),
                    ),
                    (
                        "correlation.native_parent_present".to_owned(),
                        u64::from(input.native_parent_digest.is_some()),
                    ),
                    (
                        "correlation.key_epoch_conflict".to_owned(),
                        u64::from(key_epoch_conflict),
                    ),
                    (
                        "request.canonical_input_bytes".to_owned(),
                        input.canonical_input_bytes,
                    ),
                ]),
                categorical: std::collections::BTreeMap::from([
                    ("correlation.source".to_owned(), source.as_str().to_owned()),
                    (
                        "history.completeness".to_owned(),
                        completeness_name(completeness).to_owned(),
                    ),
                ]),
                digests: input
                    .native_parent_digest
                    .as_ref()
                    .map(|digest| {
                        std::collections::BTreeMap::from([(
                            "correlation.native_parent".to_owned(),
                            digest.as_str().to_owned(),
                        )])
                    })
                    .unwrap_or_default(),
            },
            captured_at: captured_at.clone(),
            content_digest: String::new(),
        };
        event.content_digest = event.semantic_digest()?;
        let start_event = event.clone();
        let begin = BeginRequest {
            episode: episode_start,
            event,
            full_input_digest: input.full_input_digest.clone(),
            native_parent_id: trusted_native_parent_id,
            protocol: input.protocol.clone(),
        };
        validate_begin(owner_user_id, &begin)?;
        if extends_existing_episode {
            validate_candidate_history(&prior_events, &begin.event)?;
            let reserved = reserve_episode_head(
                &txn,
                owner_user_id,
                &episode_id,
                sequence,
                &input.request_id,
                &captured_at,
                completeness,
            )
            .await?;
            if !reserved {
                txn.rollback().await?;
                return Ok(CorrelateAttempt::RetrySequenceReservation);
            }
            append_event(&txn, &begin.event).await?;
            insert_request(&txn, owner_user_id, &input.request_id, begin).await?;
        } else {
            if let Err(error) =
                begin_request_in_tx(&txn, owner_user_id, &input.request_id, begin).await
            {
                let unique_start_conflict = classify_database_error(&error).unique_violation;
                if unique_start_conflict {
                    txn.rollback().await?;
                    if request_entity::Entity::find_by_id(&input.request_id)
                        .one(&self.db)
                        .await?
                        .is_some()
                    {
                        return Ok(CorrelateAttempt::RetryDeterministicStart);
                    }
                }
                return Err(error);
            }
        }
        let guarded_route = match &input.guarded_route {
            Some(route) => Some(
                append_guarded_route_in_tx(
                    &txn,
                    owner_user_id,
                    &prior_events,
                    &start_event,
                    completeness,
                    route,
                )
                .await?,
            ),
            None => None,
        };
        txn.commit().await?;
        Ok(CorrelateAttempt::Complete(Box::new(
            CorrelateAndBeginResult {
                episode_id,
                source,
                completeness,
                prior_events,
                guarded_route,
            },
        )))
    }

    pub async fn append_route_intent(
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

    pub async fn append_guard_activation(
        &self,
        owner_user_id: &str,
        event: TrajectoryEvent,
    ) -> Result<()> {
        self.append_request_event(
            owner_user_id,
            event,
            TrajectoryEventKind::GuardActivated,
            "guard activation",
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

    pub async fn settle_request(&self, owner_user_id: &str, settlement: Settlement) -> Result<()> {
        validate_owner(owner_user_id)?;
        validate_settlement(owner_user_id, &settlement)?;
        for attempt in 0..MAX_LEDGER_MUTATION_ATTEMPTS {
            match self.settle_request_once(owner_user_id, &settlement).await {
                Ok(()) => return Ok(()),
                Err(error) if mutation_error_is_retryable(&error) => {
                    if self.persisted_event_matches(&settlement.event).await? {
                        return self.settle_request_once(owner_user_id, &settlement).await;
                    }
                    tokio::time::sleep(contention_backoff(attempt)).await;
                }
                Err(error) => return Err(error),
            }
        }
        anyhow::bail!(
            "trajectory settlement contention exhausted after {MAX_LEDGER_MUTATION_ATTEMPTS} attempts"
        )
    }

    /// Build and append a settlement against the transaction's current episode
    /// head. The builder is invoked again after a contention retry, so its event
    /// sequence and any sequence-addressed outbox payload cannot become stale.
    /// For an already-settled request, the builder receives the immutable prefix
    /// immediately before the original settlement and must rebuild the exact
    /// persisted event and outbox batch.
    pub(crate) async fn settle_request_from_current_head<F>(
        &self,
        owner_user_id: &str,
        request_id: &str,
        build: F,
    ) -> Result<Settlement>
    where
        F: Fn(&StoredRequest, &[TrajectoryEvent], u64) -> Result<Settlement>,
    {
        validate_owner(owner_user_id)?;
        validate_opaque(request_id, "request_id")?;
        for attempt in 0..MAX_LEDGER_MUTATION_ATTEMPTS {
            match self
                .settle_request_from_current_head_once(owner_user_id, request_id, &build)
                .await
            {
                Ok(settlement) => return Ok(settlement),
                Err(error) if mutation_error_is_retryable(&error) => {
                    tokio::time::sleep(contention_backoff(attempt)).await;
                }
                Err(error) => return Err(error),
            }
        }
        anyhow::bail!(
            "trajectory settlement contention exhausted after {MAX_LEDGER_MUTATION_ATTEMPTS} attempts"
        )
    }

    async fn settle_request_from_current_head_once<F>(
        &self,
        owner_user_id: &str,
        request_id: &str,
        build: &F,
    ) -> Result<Settlement>
    where
        F: Fn(&StoredRequest, &[TrajectoryEvent], u64) -> Result<Settlement>,
    {
        let txn = self.db.begin().await?;
        let request = request_entity::Entity::find()
            .filter(request_entity::Column::OwnerUserId.eq(owner_user_id))
            .filter(request_entity::Column::RequestId.eq(request_id))
            .one(&txn)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("unknown owner-scoped trajectory request '{request_id}'")
            })?;
        let episode = owned_episode(&txn, owner_user_id, &request.episode_id).await?;
        let events = events_for_episode_in_tx(&txn, owner_user_id, &episode.episode_id).await?;
        validate_episode_head(&episode, &events)?;
        let stored = stored_request(request.clone())?;

        if let Some(settlement_event_id) = request.settlement_event_id.as_deref() {
            let position = events
                .iter()
                .position(|event| event.event_id == settlement_event_id)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "settled trajectory request '{request_id}' lost its settlement event"
                    )
                })?;
            let persisted = &events[position];
            let rebuilt = build(&stored, &events[..position], persisted.sequence)?;
            validate_settlement(owner_user_id, &rebuilt)?;
            if rebuilt.event == *persisted
                && request.status == request_status_name(rebuilt.status.clone())
                && settlement_outbox_matches(
                    &txn,
                    owner_user_id,
                    request.settlement_outbox_id.as_deref(),
                    rebuilt.outbox.as_ref(),
                )
                .await?
            {
                txn.commit().await?;
                return Ok(rebuilt);
            }
            anyhow::bail!("trajectory request '{request_id}' already has a different settlement")
        }

        if episode.closed_at.is_some() {
            anyhow::bail!("trajectory episode is closed and cannot accept new events")
        }
        let sequence = u64::try_from(episode.next_sequence)
            .context("stored trajectory sequence is negative")?;
        let settlement = build(&stored, &events, sequence)?;
        validate_settlement(owner_user_id, &settlement)?;
        if settlement.event.request_id.as_deref() != Some(request_id)
            || settlement.event.episode_id != request.episode_id
        {
            anyhow::bail!("transactional settlement builder changed request identity")
        }
        validate_sequence(&episode, &settlement.event)?;
        validate_candidate_history(&events, &settlement.event)?;
        append_event(&txn, &settlement.event).await?;

        let mut active = request.into_active_model();
        active.settlement_event_id = Set(Some(settlement.event.event_id.clone()));
        active.settlement_outbox_id = Set(settlement
            .outbox
            .as_ref()
            .map(|outbox| outbox.outbox_id.clone()));
        active.status = Set(request_status_name(settlement.status.clone()).into());
        active.update(&txn).await?;
        update_episode_head(&txn, episode, &settlement.event, None).await?;
        if let Some(outbox) = &settlement.outbox {
            insert_outbox(&txn, owner_user_id, outbox.clone()).await?;
        }
        txn.commit().await?;
        Ok(settlement)
    }

    async fn settle_request_once(
        &self,
        owner_user_id: &str,
        settlement: &Settlement,
    ) -> Result<()> {
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
                && request.status == request_status_name(settlement.status.clone())
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
        let events = events_for_episode_in_tx(&txn, owner_user_id, &episode.episode_id).await?;
        validate_episode_head(&episode, &events)?;
        if episode.closed_at.is_some() {
            anyhow::bail!("trajectory episode is closed and cannot accept new events")
        }
        validate_sequence(&episode, &settlement.event)?;
        validate_candidate_history(&events, &settlement.event)?;
        append_event(&txn, &settlement.event).await?;

        let mut active = request.into_active_model();
        active.settlement_event_id = Set(Some(settlement.event.event_id.clone()));
        active.settlement_outbox_id = Set(settlement
            .outbox
            .as_ref()
            .map(|outbox| outbox.outbox_id.clone()));
        active.status = Set(request_status_name(settlement.status.clone()).into());
        active.update(&txn).await?;
        update_episode_head(&txn, episode, &settlement.event, None).await?;
        if let Some(outbox) = &settlement.outbox {
            insert_outbox(&txn, owner_user_id, outbox.clone()).await?;
        }
        txn.commit().await?;
        Ok(())
    }

    async fn persisted_event_matches(&self, event: &TrajectoryEvent) -> Result<bool> {
        let row = event_entity::Entity::find()
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

    pub(crate) async fn validate_reusable_terminal_settlement(
        &self,
        owner_user_id: &str,
        request_id: &str,
    ) -> Result<()> {
        validate_owner(owner_user_id)?;
        validate_opaque(request_id, "request_id")?;
        let txn = self.db.begin().await?;
        let request = request_entity::Entity::find()
            .filter(request_entity::Column::OwnerUserId.eq(owner_user_id))
            .filter(request_entity::Column::RequestId.eq(request_id))
            .one(&txn)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("unknown owner-scoped trajectory request '{request_id}'")
            })?;
        let stored = stored_request(request.clone())?;
        let expected_outcome = match stored.status {
            RequestStatus::Settled => "settled",
            RequestStatus::Failed => "failed",
            RequestStatus::Started => {
                anyhow::bail!("trajectory request '{request_id}' is not terminal")
            }
        };
        let settlement_event_id = request.settlement_event_id.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "terminal trajectory request '{request_id}' lost its settlement event index"
            )
        })?;
        let episode = owned_episode(&txn, owner_user_id, &request.episode_id).await?;
        let events = events_for_episode_in_tx(&txn, owner_user_id, &episode.episode_id).await?;
        validate_episode_head(&episode, &events)?;
        let start = events
            .iter()
            .find(|event| event.event_id == request.start_event_id)
            .ok_or_else(|| {
                anyhow::anyhow!("trajectory request '{request_id}' lost its start event")
            })?;
        if start.kind != TrajectoryEventKind::RequestStarted
            || start.request_id.as_deref() != Some(request_id)
        {
            anyhow::bail!("trajectory request '{request_id}' has an inconsistent start event")
        }
        let settlement_position = events
            .iter()
            .position(|event| event.event_id == settlement_event_id)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "settled trajectory request '{request_id}' lost its settlement event"
                )
            })?;
        let settlement = &events[settlement_position];
        if settlement.kind != TrajectoryEventKind::RequestSettled
            || settlement.request_id.as_deref() != Some(request_id)
            || settlement.episode_id != request.episode_id
            || settlement
                .evidence
                .categorical
                .get("settlement.outcome")
                .map(String::as_str)
                != Some(expected_outcome)
            || events
                .iter()
                .filter(|event| {
                    event.kind == TrajectoryEventKind::RequestSettled
                        && event.request_id.as_deref() == Some(request_id)
                })
                .count()
                != 1
        {
            anyhow::bail!("trajectory request '{request_id}' has an inconsistent settlement event")
        }
        let expected_evaluation = build_operational_evaluation(&events[..=settlement_position])?;
        let settlement_outbox_id = request.settlement_outbox_id.as_deref().ok_or_else(|| {
            anyhow::anyhow!("terminal trajectory request '{request_id}' lost its outbox index")
        })?;
        let outbox = outbox_entity::Entity::find()
            .filter(outbox_entity::Column::OwnerUserId.eq(owner_user_id))
            .filter(outbox_entity::Column::OutboxId.eq(settlement_outbox_id))
            .one(&txn)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!("terminal trajectory request '{request_id}' lost its outbox")
            })?;
        let outbox = stored_outbox(outbox)?;
        if outbox.topic != TRAJECTORY_EVAL_TOPIC
            || outbox.created_at != settlement.captured_at
            || outbox
                .payload
                .structural
                .get("trajectory.through_sequence")
                .copied()
                != Some(settlement.sequence)
            || outbox
                .payload
                .digests
                .get("trajectory.settlement_event")
                .map(String::as_str)
                != Some(settlement.content_digest.as_str())
            || outbox.payload.evaluation.as_deref() != Some(&expected_evaluation)
        {
            anyhow::bail!("trajectory request '{request_id}' has an inconsistent outbox")
        }
        txn.commit().await?;
        Ok(())
    }

    pub async fn pending_outbox(&self, owner_user_id: &str) -> Result<Vec<PendingOutbox>> {
        validate_owner(owner_user_id)?;
        outbox_entity::Entity::find()
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

    pub(crate) async fn pending_outbox_batch(&self, limit: usize) -> Result<Vec<OutboxBatchItem>> {
        if limit == 0 || limit > MAX_OUTBOX_BATCH_SIZE {
            anyhow::bail!(
                "trajectory outbox batch size must be between 1 and {MAX_OUTBOX_BATCH_SIZE}"
            )
        }
        let rows = outbox_entity::Entity::find()
            .filter(outbox_entity::Column::DeliveredAt.is_null())
            .order_by_asc(outbox_entity::Column::Attempts)
            .order_by_asc(outbox_entity::Column::CreatedAt)
            .order_by_asc(outbox_entity::Column::OutboxId)
            .limit(u64::try_from(limit).context("outbox batch size exceeds u64")?)
            .all(&self.db)
            .await?;
        Ok(rows.into_iter().map(stored_outbox_batch_item).collect())
    }

    pub(crate) async fn pending_outbox_count(&self) -> Result<u64> {
        outbox_entity::Entity::find()
            .filter(outbox_entity::Column::DeliveredAt.is_null())
            .count(&self.db)
            .await
            .map_err(Into::into)
    }

    pub(crate) async fn record_outbox_attempt(
        &self,
        owner_user_id: &str,
        outbox_id: &str,
    ) -> Result<()> {
        validate_owner(owner_user_id)?;
        let Some(row) = outbox_entity::Entity::find()
            .filter(outbox_entity::Column::OwnerUserId.eq(owner_user_id))
            .filter(outbox_entity::Column::OutboxId.eq(outbox_id))
            .one(&self.db)
            .await?
        else {
            anyhow::bail!("unknown owner-scoped trajectory outbox '{outbox_id}'")
        };
        if row.delivered_at.is_some() {
            return Ok(());
        }
        let next_attempt = row
            .attempts
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("trajectory outbox attempts overflow"))?;
        let mut active = row.into_active_model();
        active.attempts = Set(next_attempt);
        active.update(&self.db).await?;
        Ok(())
    }

    pub async fn mark_outbox_delivered(
        &self,
        owner_user_id: &str,
        outbox_id: &str,
        delivered_at: &str,
    ) -> Result<()> {
        validate_owner(owner_user_id)?;
        validate_timestamp(delivered_at, "delivered_at")?;
        let updated = outbox_entity::Entity::update_many()
            .col_expr(
                outbox_entity::Column::DeliveredAt,
                Expr::value(Some(delivered_at.to_owned())),
            )
            .filter(outbox_entity::Column::OwnerUserId.eq(owner_user_id))
            .filter(outbox_entity::Column::OutboxId.eq(outbox_id))
            .filter(outbox_entity::Column::DeliveredAt.is_null())
            .exec(&self.db)
            .await?;
        if updated.rows_affected == 1 {
            return Ok(());
        }
        if updated.rows_affected > 1 {
            anyhow::bail!("owner-scoped trajectory outbox delivery updated multiple rows")
        }
        match outbox_entity::Entity::find()
            .filter(outbox_entity::Column::OwnerUserId.eq(owner_user_id))
            .filter(outbox_entity::Column::OutboxId.eq(outbox_id))
            .one(&self.db)
            .await?
        {
            Some(row) if row.delivered_at.is_some() => Ok(()),
            Some(_) => anyhow::bail!("trajectory outbox '{outbox_id}' was not marked delivered"),
            None => anyhow::bail!("unknown owner-scoped trajectory outbox '{outbox_id}'"),
        }
    }

    /// Resolve the owner of a globally unique episode for the local operator
    /// CLI. All subsequent reads remain owner-filtered.
    pub async fn resolve_episode_owner(&self, episode_id: &str) -> Result<Option<String>> {
        validate_opaque(episode_id, "episode_id")?;
        Ok(episode_entity::Entity::find_by_id(episode_id)
            .one(&self.db)
            .await?
            .map(|episode| episode.owner_user_id))
    }

    pub async fn episode(
        &self,
        owner_user_id: &str,
        episode_id: &str,
    ) -> Result<Option<StoredEpisode>> {
        validate_owner(owner_user_id)?;
        validate_opaque(episode_id, "episode_id")?;
        episode_entity::Entity::find()
            .filter(episode_entity::Column::OwnerUserId.eq(owner_user_id))
            .filter(episode_entity::Column::EpisodeId.eq(episode_id))
            .one(&self.db)
            .await?
            .map(stored_episode)
            .transpose()
    }

    pub async fn audit_episode(
        &self,
        owner_user_id: &str,
        episode_id: &str,
    ) -> Result<EpisodeAudit> {
        #[cfg(test)]
        {
            return self
                .audit_episode_inner(owner_user_id, episode_id, None)
                .await;
        }
        #[cfg(not(test))]
        self.audit_episode_inner(owner_user_id, episode_id).await
    }

    #[cfg(test)]
    async fn audit_episode_with_probe(
        &self,
        owner_user_id: &str,
        episode_id: &str,
        probe: &AuditReadProbe,
    ) -> Result<EpisodeAudit> {
        self.audit_episode_inner(owner_user_id, episode_id, Some(probe))
            .await
    }

    async fn audit_episode_inner(
        &self,
        owner_user_id: &str,
        episode_id: &str,
        #[cfg(test)] probe: Option<&AuditReadProbe>,
    ) -> Result<EpisodeAudit> {
        validate_owner(owner_user_id)?;
        validate_opaque(episode_id, "episode_id")?;
        for attempt in 0..MAX_AUDIT_READ_ATTEMPTS {
            let episode_row = self
                .owned_episode_row(owner_user_id, episode_id)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!("unknown owner-scoped trajectory episode '{episode_id}'")
                })?;
            #[cfg(test)]
            if attempt == 0
                && let Some((head_read, writer_done)) =
                    probe.and_then(|probe| probe.first_read_barriers.as_ref())
            {
                head_read.wait().await;
                writer_done.wait().await;
            }
            let rows = self.raw_event_rows(owner_user_id, episode_id).await?;
            let second_episode_row = self.owned_episode_row(owner_user_id, episode_id).await?;
            #[cfg(test)]
            let force_contention = probe.is_some_and(|probe| probe.force_contention);
            #[cfg(not(test))]
            let force_contention = false;
            if second_episode_row.as_ref() == Some(&episode_row) && !force_contention {
                return audit_stable_episode(episode_row, rows);
            }
            tokio::time::sleep(contention_backoff(attempt)).await;
        }
        anyhow::bail!(
            "trajectory episode audit contention exhausted after {MAX_AUDIT_READ_ATTEMPTS} attempts"
        )
    }

    async fn owned_episode_row(
        &self,
        owner_user_id: &str,
        episode_id: &str,
    ) -> Result<Option<episode_entity::Model>> {
        episode_entity::Entity::find()
            .filter(episode_entity::Column::OwnerUserId.eq(owner_user_id))
            .filter(episode_entity::Column::EpisodeId.eq(episode_id))
            .one(&self.db)
            .await
            .map_err(Into::into)
    }

    async fn raw_event_rows(
        &self,
        owner_user_id: &str,
        episode_id: &str,
    ) -> Result<Vec<event_entity::Model>> {
        event_entity::Entity::find()
            .filter(event_entity::Column::OwnerUserId.eq(owner_user_id))
            .filter(event_entity::Column::EpisodeId.eq(episode_id))
            .order_by_asc(event_entity::Column::Sequence)
            .all(&self.db)
            .await
            .map_err(Into::into)
    }

    /// Prune delivered publication rows and retention-expired episode history.
    /// Open episodes are eligible only when every indexed request is terminal;
    /// an associated pending outbox row always preserves the whole episode.
    pub async fn prune_before(
        &self,
        before: &str,
        dry_run: bool,
        batch_size: usize,
    ) -> Result<PruneSummary> {
        validate_timestamp(before, "trajectory prune cutoff")?;
        if batch_size == 0 || batch_size > MAX_OUTBOX_BATCH_SIZE {
            anyhow::bail!(
                "trajectory prune batch size must be between 1 and {MAX_OUTBOX_BATCH_SIZE}"
            )
        }
        let before = chrono::DateTime::parse_from_rfc3339(before)?;
        if dry_run {
            return self.prune_dry_run(before).await;
        }

        let mut summary = self.prune_delivered_outbox(before, batch_size).await?;
        let episodes = self.prune_expired_episodes(before, batch_size).await?;
        merge_prune_summary(&mut summary, episodes)?;
        Ok(summary)
    }

    async fn prune_dry_run(
        &self,
        before: chrono::DateTime<chrono::FixedOffset>,
    ) -> Result<PruneSummary> {
        let txn = self.db.begin().await?;
        let delivered = outbox_entity::Entity::find()
            .filter(outbox_entity::Column::DeliveredAt.is_not_null())
            .all(&txn)
            .await?;
        let delivered_outbox_rows = count_before(
            delivered
                .iter()
                .filter_map(|row| row.delivered_at.as_deref()),
            before,
            "stored trajectory outbox delivered_at",
        )?;
        let episodes = episode_entity::Entity::find().all(&txn).await?;
        let mut summary = PruneSummary {
            delivered_outbox_rows,
            ..PruneSummary::default()
        };
        for episode in episodes {
            if let Some((request_rows, event_rows)) =
                prunable_episode_counts(&txn, &episode, before).await?
            {
                summary.episode_rows = checked_prune_add(summary.episode_rows, 1)?;
                summary.request_rows = checked_prune_add(summary.request_rows, request_rows)?;
                summary.event_rows = checked_prune_add(summary.event_rows, event_rows)?;
            }
        }
        txn.commit().await?;
        Ok(summary)
    }

    async fn prune_delivered_outbox(
        &self,
        before: chrono::DateTime<chrono::FixedOffset>,
        batch_size: usize,
    ) -> Result<PruneSummary> {
        let mut cursor: Option<String> = None;
        let mut summary = PruneSummary::default();
        loop {
            let txn = self.db.begin().await?;
            let mut query = outbox_entity::Entity::find()
                .filter(outbox_entity::Column::DeliveredAt.is_not_null())
                .order_by_asc(outbox_entity::Column::OutboxId)
                .limit(u64::try_from(batch_size).context("prune batch size exceeds u64")?);
            if let Some(cursor) = cursor.as_deref() {
                query = query.filter(outbox_entity::Column::OutboxId.gt(cursor));
            }
            let rows = query.all(&txn).await?;
            if rows.is_empty() {
                txn.commit().await?;
                break;
            }
            for row in &rows {
                let delivered_at = row.delivered_at.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("delivered trajectory outbox row lost delivered_at")
                })?;
                if timestamp_is_before(
                    delivered_at,
                    before,
                    "stored trajectory outbox delivered_at",
                )? {
                    let deleted = outbox_entity::Entity::delete_many()
                        .filter(outbox_entity::Column::OwnerUserId.eq(&row.owner_user_id))
                        .filter(outbox_entity::Column::OutboxId.eq(&row.outbox_id))
                        .filter(outbox_entity::Column::DeliveredAt.eq(delivered_at))
                        .exec(&txn)
                        .await?;
                    summary.delivered_outbox_rows =
                        checked_prune_add(summary.delivered_outbox_rows, deleted.rows_affected)?;
                }
            }
            cursor = rows.last().map(|row| row.outbox_id.clone());
            txn.commit().await?;
        }
        Ok(summary)
    }

    async fn prune_expired_episodes(
        &self,
        before: chrono::DateTime<chrono::FixedOffset>,
        batch_size: usize,
    ) -> Result<PruneSummary> {
        let mut cursor: Option<String> = None;
        let mut summary = PruneSummary::default();
        loop {
            let mut query = episode_entity::Entity::find()
                .order_by_asc(episode_entity::Column::EpisodeId)
                .limit(u64::try_from(batch_size).context("prune batch size exceeds u64")?);
            if let Some(cursor) = cursor.as_deref() {
                query = query.filter(episode_entity::Column::EpisodeId.gt(cursor));
            }
            let rows = query.all(&self.db).await?;
            if rows.is_empty() {
                break;
            }
            for episode in &rows {
                let deleted = self.prune_expired_episode(episode, before).await?;
                merge_prune_summary(&mut summary, deleted)?;
            }
            cursor = rows.last().map(|episode| episode.episode_id.clone());
        }
        Ok(summary)
    }

    async fn prune_expired_episode(
        &self,
        candidate: &episode_entity::Model,
        before: chrono::DateTime<chrono::FixedOffset>,
    ) -> Result<PruneSummary> {
        for attempt in 0..MAX_LEDGER_MUTATION_ATTEMPTS {
            let txn = self.db.begin().await?;
            let current = episode_entity::Entity::find()
                .filter(episode_entity::Column::OwnerUserId.eq(&candidate.owner_user_id))
                .filter(episode_entity::Column::EpisodeId.eq(&candidate.episode_id))
                .one(&txn)
                .await?;
            let Some(current) = current else {
                txn.commit().await?;
                return Ok(PruneSummary::default());
            };
            let Some((request_rows, event_rows)) =
                prunable_episode_counts(&txn, &current, before).await?
            else {
                txn.commit().await?;
                return Ok(PruneSummary::default());
            };
            match delete_prunable_episode_in_tx(&txn, &current, request_rows, event_rows).await {
                Ok(summary) => {
                    txn.commit().await?;
                    return Ok(summary);
                }
                Err(error) if error.downcast_ref::<PruneRace>().is_some() => {
                    txn.rollback().await?;
                    tokio::time::sleep(contention_backoff(attempt)).await;
                }
                Err(error) if classify_database_error(&error).retryable_contention => {
                    txn.rollback().await?;
                    tokio::time::sleep(contention_backoff(attempt)).await;
                }
                Err(error) => {
                    txn.rollback().await?;
                    return Err(error);
                }
            }
        }
        anyhow::bail!(
            "trajectory episode prune contention exhausted after {MAX_LEDGER_MUTATION_ATTEMPTS} attempts"
        )
    }
}

fn audit_stable_episode(
    episode_row: episode_entity::Model,
    rows: Vec<event_entity::Model>,
) -> Result<EpisodeAudit> {
    let episode = stored_episode(episode_row.clone())?;
    let mut events = Vec::with_capacity(rows.len());
    for (index, row) in rows.into_iter().enumerate() {
        let event_id = row.event_id.clone();
        let indexed_sequence = u64::try_from(row.sequence).ok();
        let event = match stored_event(row) {
            Ok(event) => event,
            Err(_) => {
                return Ok(EpisodeAudit::Corrupt {
                    episode,
                    event_id: Some(event_id),
                    sequence: indexed_sequence,
                    reason: "stored_event_invalid".to_owned(),
                });
            }
        };
        let expected = u64::try_from(index)
            .context("trajectory event count exceeds sequence range")?
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("trajectory event sequence overflow"))?;
        if event.sequence != expected {
            return Ok(EpisodeAudit::Corrupt {
                episode,
                event_id: Some(event.event_id),
                sequence: Some(event.sequence),
                reason: "sequence_gap".to_owned(),
            });
        }
        events.push(event);
    }
    let snapshot = match reduce(&events, &std::collections::BTreeSet::new()) {
        Ok(snapshot) => snapshot,
        Err(_) => {
            let fallback = (
                events.last().map(|event| event.event_id.clone()),
                events.last().map(|event| event.sequence),
            );
            let (event_id, sequence) = match first_reducer_failure(&events) {
                Some(failure) => failure,
                None => fallback,
            };
            return Ok(EpisodeAudit::Corrupt {
                episode,
                event_id,
                sequence,
                reason: "reducer_rejected_prefix".to_owned(),
            });
        }
    };
    if validate_episode_head(&episode_row, &events).is_err() {
        return Ok(EpisodeAudit::Corrupt {
            episode,
            event_id: events.last().map(|event| event.event_id.clone()),
            sequence: events.last().map(|event| event.sequence),
            reason: "episode_head_mismatch".to_owned(),
        });
    }
    Ok(EpisodeAudit::Valid {
        episode,
        events,
        snapshot,
    })
}

#[derive(Debug, thiserror::Error)]
#[error("trajectory episode changed during prune")]
struct PruneRace;

async fn delete_prunable_episode_in_tx(
    txn: &DatabaseTransaction,
    episode: &episode_entity::Model,
    expected_request_rows: u64,
    expected_event_rows: u64,
) -> Result<PruneSummary> {
    serialize_prefix_prune(txn, &episode.owner_user_id, &episode.episode_id).await?;
    delete_prefixes_owned_only_by_episode(txn, &episode.owner_user_id, &episode.episode_id).await?;
    let deleted_requests = request_entity::Entity::delete_many()
        .filter(request_entity::Column::OwnerUserId.eq(&episode.owner_user_id))
        .filter(request_entity::Column::EpisodeId.eq(&episode.episode_id))
        .exec(txn)
        .await?;
    let deleted_events = event_entity::Entity::delete_many()
        .filter(event_entity::Column::OwnerUserId.eq(&episode.owner_user_id))
        .filter(event_entity::Column::EpisodeId.eq(&episode.episode_id))
        .exec(txn)
        .await?;
    if deleted_requests.rows_affected != expected_request_rows
        || deleted_events.rows_affected != expected_event_rows
    {
        return Err(PruneRace.into());
    }

    let mut delete_episode = episode_entity::Entity::delete_many()
        .filter(episode_entity::Column::OwnerUserId.eq(&episode.owner_user_id))
        .filter(episode_entity::Column::EpisodeId.eq(&episode.episode_id))
        .filter(episode_entity::Column::NextSequence.eq(episode.next_sequence))
        .filter(episode_entity::Column::LastCapturedAt.eq(&episode.last_captured_at));
    delete_episode = match episode.closed_at.as_deref() {
        Some(closed_at) => delete_episode.filter(episode_entity::Column::ClosedAt.eq(closed_at)),
        None => delete_episode.filter(episode_entity::Column::ClosedAt.is_null()),
    };
    let deleted_episode = delete_episode.exec(txn).await?;
    if deleted_episode.rows_affected != 1 {
        return Err(PruneRace.into());
    }
    Ok(PruneSummary {
        delivered_outbox_rows: 0,
        episode_rows: 1,
        event_rows: deleted_events.rows_affected,
        request_rows: deleted_requests.rows_affected,
    })
}

fn episode_prefix_digests(owner_user_id: &str, episode_id: &str) -> SelectStatement {
    Query::select()
        .column(request_entity::Column::FullInputDigest)
        .from(request_entity::Entity)
        .and_where(Expr::col(request_entity::Column::OwnerUserId).eq(owner_user_id.to_owned()))
        .and_where(Expr::col(request_entity::Column::EpisodeId).eq(episode_id.to_owned()))
        .to_owned()
}

async fn serialize_prefix_prune(
    txn: &DatabaseTransaction,
    owner_user_id: &str,
    episode_id: &str,
) -> Result<()> {
    prefix_entity::Entity::update_many()
        .col_expr(
            prefix_entity::Column::Ambiguous,
            Expr::col(prefix_entity::Column::Ambiguous).into(),
        )
        .filter(prefix_entity::Column::OwnerUserId.eq(owner_user_id))
        .filter(
            Expr::col(prefix_entity::Column::FullInputDigest)
                .in_subquery(episode_prefix_digests(owner_user_id, episode_id)),
        )
        .exec(txn)
        .await?;
    Ok(())
}

async fn delete_prefixes_owned_only_by_episode(
    txn: &DatabaseTransaction,
    owner_user_id: &str,
    episode_id: &str,
) -> Result<()> {
    let other_episode_request = Query::select()
        .expr(Expr::value(1))
        .from(request_entity::Entity)
        .and_where(
            Expr::col((request_entity::Entity, request_entity::Column::OwnerUserId))
                .equals((prefix_entity::Entity, prefix_entity::Column::OwnerUserId)),
        )
        .and_where(
            Expr::col((
                request_entity::Entity,
                request_entity::Column::FullInputDigest,
            ))
            .equals((
                prefix_entity::Entity,
                prefix_entity::Column::FullInputDigest,
            )),
        )
        .and_where(Expr::col(request_entity::Column::EpisodeId).ne(episode_id.to_owned()))
        .to_owned();
    prefix_entity::Entity::delete_many()
        .filter(prefix_entity::Column::OwnerUserId.eq(owner_user_id))
        .filter(
            Expr::col(prefix_entity::Column::FullInputDigest)
                .in_subquery(episode_prefix_digests(owner_user_id, episode_id)),
        )
        .filter(Cond::all().not().add(Expr::exists(other_episode_request)))
        .exec(txn)
        .await?;
    Ok(())
}

async fn prunable_episode_counts(
    txn: &DatabaseTransaction,
    episode: &episode_entity::Model,
    before: chrono::DateTime<chrono::FixedOffset>,
) -> Result<Option<(u64, u64)>> {
    let last_is_old = timestamp_is_before(
        &episode.last_captured_at,
        before,
        "stored trajectory episode last_captured_at",
    )?;
    let closed_is_old = episode
        .closed_at
        .as_deref()
        .map(|closed_at| {
            timestamp_is_before(closed_at, before, "stored trajectory episode closed_at")
        })
        .transpose()?;
    let closed_is_old = closed_is_old == Some(true);
    if !last_is_old || (episode.closed_at.is_some() && !closed_is_old) {
        return Ok(None);
    }

    let requests = request_entity::Entity::find()
        .filter(request_entity::Column::OwnerUserId.eq(&episode.owner_user_id))
        .filter(request_entity::Column::EpisodeId.eq(&episode.episode_id))
        .all(txn)
        .await?;
    if requests.is_empty() {
        return Ok(None);
    }
    if requests
        .iter()
        .any(|request| !matches!(request.status.as_str(), "settled" | "failed"))
    {
        return Ok(None);
    }
    let outbox_ids = requests
        .iter()
        .filter_map(|request| request.settlement_outbox_id.clone())
        .collect::<Vec<_>>();
    if !outbox_ids.is_empty()
        && outbox_entity::Entity::find()
            .filter(outbox_entity::Column::OwnerUserId.eq(&episode.owner_user_id))
            .filter(outbox_entity::Column::OutboxId.is_in(outbox_ids))
            .filter(outbox_entity::Column::DeliveredAt.is_null())
            .one(txn)
            .await?
            .is_some()
    {
        return Ok(None);
    }
    let request_rows = u64::try_from(requests.len()).context("too many trajectory requests")?;
    let event_rows = event_entity::Entity::find()
        .filter(event_entity::Column::OwnerUserId.eq(&episode.owner_user_id))
        .filter(event_entity::Column::EpisodeId.eq(&episode.episode_id))
        .count(txn)
        .await?;
    Ok(Some((request_rows, event_rows)))
}

fn timestamp_is_before(
    value: &str,
    before: chrono::DateTime<chrono::FixedOffset>,
    field: &str,
) -> Result<bool> {
    Ok(chrono::DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("{field} must be RFC3339"))?
        < before)
}

fn count_before<'a>(
    values: impl Iterator<Item = &'a str>,
    before: chrono::DateTime<chrono::FixedOffset>,
    field: &str,
) -> Result<u64> {
    let mut count = 0_u64;
    for value in values {
        if timestamp_is_before(value, before, field)? {
            count = checked_prune_add(count, 1)?;
        }
    }
    Ok(count)
}

fn merge_prune_summary(target: &mut PruneSummary, other: PruneSummary) -> Result<()> {
    target.delivered_outbox_rows =
        checked_prune_add(target.delivered_outbox_rows, other.delivered_outbox_rows)?;
    target.episode_rows = checked_prune_add(target.episode_rows, other.episode_rows)?;
    target.event_rows = checked_prune_add(target.event_rows, other.event_rows)?;
    target.request_rows = checked_prune_add(target.request_rows, other.request_rows)?;
    Ok(())
}

fn checked_prune_add(left: u64, right: u64) -> Result<u64> {
    left.checked_add(right)
        .ok_or_else(|| anyhow::anyhow!("trajectory prune count overflow"))
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
            let events = events_for_episode_in_tx(txn, owner_user_id, &existing.episode_id).await?;
            validate_episode_head(&existing, &events)?;
            if existing.closed_at.is_some() {
                anyhow::bail!("trajectory episode is closed and cannot accept new events")
            }
            validate_existing_episode(&existing, &input.episode, &input.event)?;
            validate_candidate_history(&events, &input.event)?;
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
            validate_new_episode_start(&input)?;
            validate_candidate_history(&[], &input.event)?;
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
    insert_request(txn, owner_user_id, request_id, input).await
}

async fn insert_request(
    txn: &DatabaseTransaction,
    owner_user_id: &str,
    request_id: &str,
    input: BeginRequest,
) -> Result<()> {
    let full_input_digest = input.full_input_digest.as_str().to_owned();
    let episode_id = input.episode.episode_id;
    request_entity::ActiveModel {
        request_id: Set(request_id.to_owned()),
        owner_user_id: Set(owner_user_id.to_owned()),
        episode_id: Set(episode_id.clone()),
        start_event_id: Set(input.event.event_id),
        settlement_event_id: Set(None),
        settlement_outbox_id: Set(None),
        full_input_digest: Set(full_input_digest.clone()),
        native_parent_id: Set(input.native_parent_id),
        protocol: Set(input.protocol),
        status: Set(request_status_name(RequestStatus::Started).into()),
    }
    .insert(txn)
    .await?;
    record_prefix_episode(txn, owner_user_id, &full_input_digest, &episode_id).await?;
    Ok(())
}

async fn record_prefix_episode(
    txn: &DatabaseTransaction,
    owner_user_id: &str,
    full_input_digest: &str,
    episode_id: &str,
) -> Result<()> {
    let row = prefix_entity::ActiveModel {
        owner_user_id: Set(owner_user_id.to_owned()),
        full_input_digest: Set(full_input_digest.to_owned()),
        episode_id: Set(episode_id.to_owned()),
        ambiguous: Set(false),
    };
    match prefix_entity::Entity::insert(row)
        .on_conflict(prefix_insert_conflict())
        .exec(txn)
        .await
    {
        Ok(_) | Err(DbErr::RecordNotInserted) => {}
        Err(error) => return Err(error.into()),
    }
    prefix_entity::Entity::update_many()
        .col_expr(prefix_entity::Column::Ambiguous, Expr::value(true))
        .filter(prefix_entity::Column::OwnerUserId.eq(owner_user_id))
        .filter(prefix_entity::Column::FullInputDigest.eq(full_input_digest))
        .filter(prefix_entity::Column::EpisodeId.ne(episode_id))
        .filter(prefix_entity::Column::Ambiguous.eq(false))
        .exec(txn)
        .await?;
    Ok(())
}

fn prefix_insert_conflict() -> OnConflict {
    OnConflict::columns([
        prefix_entity::Column::OwnerUserId,
        prefix_entity::Column::FullInputDigest,
    ])
    .do_nothing_on([
        prefix_entity::Column::OwnerUserId,
        prefix_entity::Column::FullInputDigest,
    ])
    .to_owned()
}

async fn find_prefix_episode(
    txn: &DatabaseTransaction,
    owner_user_id: &str,
    ancestor_prefix_digests: &[KeyedDigest],
) -> Result<PrefixResolution> {
    let requested_digests = ancestor_prefix_digests
        .iter()
        .map(|digest| digest.as_str().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    if requested_digests.is_empty() {
        return Ok(PrefixResolution::None);
    }
    let prefixes = load_prefix_evidence(txn, owner_user_id, requested_digests).await?;
    let prefixes = prefixes
        .into_iter()
        .map(|prefix| (prefix.full_input_digest.clone(), prefix))
        .collect::<std::collections::BTreeMap<_, _>>();

    for digest in ancestor_prefix_digests.iter().rev() {
        let Some(prefix) = prefixes.get(digest.as_str()) else {
            continue;
        };
        if prefix.ambiguous {
            return Ok(PrefixResolution::Ambiguous);
        }
        return owned_episode(txn, owner_user_id, &prefix.episode_id)
            .await
            .map(Box::new)
            .map(PrefixResolution::Unique);
    }
    Ok(PrefixResolution::None)
}

async fn load_prefix_evidence(
    txn: &DatabaseTransaction,
    owner_user_id: &str,
    requested_digests: std::collections::BTreeSet<String>,
) -> Result<Vec<prefix_entity::Model>> {
    Ok(prefix_entity::Entity::find()
        .filter(prefix_entity::Column::OwnerUserId.eq(owner_user_id))
        .filter(prefix_entity::Column::FullInputDigest.is_in(requested_digests))
        .all(txn)
        .await?)
}

async fn native_parent_request(
    txn: &DatabaseTransaction,
    owner_user_id: &str,
    native_parent_id: &str,
) -> Result<Option<request_entity::Model>> {
    request_entity::Entity::find()
        .filter(request_entity::Column::OwnerUserId.eq(owner_user_id))
        .filter(request_entity::Column::RequestId.eq(native_parent_id))
        .one(txn)
        .await
        .map_err(Into::into)
}

async fn exact_retry_result(
    txn: &DatabaseTransaction,
    owner_user_id: &str,
    existing: &request_entity::Model,
    input: &CorrelateAndBegin,
) -> Result<CorrelateAndBeginResult> {
    let episode = owned_episode(txn, owner_user_id, &existing.episode_id).await?;
    let events = events_for_episode_in_tx(txn, owner_user_id, &episode.episode_id).await?;
    validate_episode_head(&episode, &events)?;
    let start = events
        .iter()
        .find(|event| event.event_id == existing.start_event_id)
        .ok_or_else(|| anyhow::anyhow!("trajectory retry is missing its original start event"))?;
    validate_stored_native_parent(txn, owner_user_id, existing.native_parent_id.as_deref()).await?;
    let ancestor_count = start
        .evidence
        .structural
        .get("correlation.ancestor_prefix_count")
        .copied();
    let starts_with_prior_turns = start
        .evidence
        .structural
        .get("correlation.starts_with_prior_turns")
        .copied();
    let ancestor_prefixes_truncated = start
        .evidence
        .structural
        .get("correlation.ancestor_prefixes_truncated")
        .copied();
    let native_parent_present = start
        .evidence
        .structural
        .get("correlation.native_parent_present")
        .copied();
    let native_parent_digest = start
        .evidence
        .digests
        .get("correlation.native_parent")
        .map(String::as_str);
    let canonical_input_bytes = start
        .evidence
        .structural
        .get("request.canonical_input_bytes")
        .copied();
    if existing.start_event_id != input.event_id
        || existing.full_input_digest != input.full_input_digest.as_str()
        || existing.protocol != input.protocol
        || ancestor_count
            != Some(
                u64::try_from(input.ancestor_prefix_digests.len())
                    .context("too many ancestor prefix digests")?,
            )
        || starts_with_prior_turns != Some(u64::from(input.starts_with_prior_turns))
        || ancestor_prefixes_truncated != Some(u64::from(input.ancestor_prefixes_truncated))
        || native_parent_present != Some(u64::from(input.native_parent_digest.is_some()))
        || native_parent_digest != input.native_parent_digest.as_ref().map(KeyedDigest::as_str)
        || canonical_input_bytes != Some(input.canonical_input_bytes)
        || start.owner_user_id != owner_user_id
        || start.request_id.as_deref() != Some(input.request_id.as_str())
        || start.kind != TrajectoryEventKind::RequestStarted
    {
        anyhow::bail!(
            "trajectory request '{}' already exists with different content",
            existing.request_id
        )
    }
    let source = start
        .evidence
        .categorical
        .get("correlation.source")
        .ok_or_else(|| anyhow::anyhow!("trajectory retry has no correlation source"))?;
    let completeness = start
        .evidence
        .categorical
        .get("history.completeness")
        .ok_or_else(|| anyhow::anyhow!("trajectory retry has no history completeness"))?;
    let source = parse_correlation_source(source)?;
    let completeness = parse_completeness(completeness)?;
    let start_sequence = start.sequence;
    let prior_events = events
        .iter()
        .cloned()
        .take_while(|event| event.sequence < start_sequence)
        .collect::<Vec<_>>();
    let guarded_route = match &input.guarded_route {
        Some(route) => {
            reduce(&events, &route.policy.protected_tiers)?;
            let expected = build_guarded_route_batch(&prior_events, start, completeness, route)?;
            let persisted_route = events
                .iter()
                .find(|event| event.event_id == route.route_event_id)
                .ok_or_else(|| anyhow::anyhow!("trajectory retry is missing its route intent"))?;
            if persisted_route != &expected.route_event {
                anyhow::bail!(
                    "trajectory request '{}' already exists with a different route intent",
                    existing.request_id
                )
            }
            match (
                &expected.guard_event,
                events
                    .iter()
                    .find(|event| event.event_id == route.guard_event_id),
            ) {
                (Some(expected), Some(persisted)) if expected == persisted => {}
                (None, None) => {}
                _ => anyhow::bail!(
                    "trajectory request '{}' already exists with a different guard activation",
                    existing.request_id
                ),
            }
            let route_event_count = events
                .iter()
                .filter(|event| {
                    event.request_id.as_deref() == Some(existing.request_id.as_str())
                        && event.kind == TrajectoryEventKind::RouteIntentRecorded
                })
                .count();
            let guard_event_count = events
                .iter()
                .filter(|event| {
                    event.request_id.as_deref() == Some(existing.request_id.as_str())
                        && event.kind == TrajectoryEventKind::GuardActivated
                })
                .count();
            if route_event_count != 1
                || guard_event_count != usize::from(expected.guard_event.is_some())
            {
                anyhow::bail!(
                    "trajectory request '{}' has an unexpected persisted routing batch",
                    existing.request_id
                )
            }
            Some(expected.result)
        }
        None => {
            for event in events.iter().filter(|event| {
                event.request_id.as_deref() == Some(existing.request_id.as_str())
                    && event.kind == TrajectoryEventKind::RouteIntentRecorded
            }) {
                if validate_persisted_route_intent(event)?.is_some() {
                    anyhow::bail!(
                        "trajectory request '{}' already exists with guarded routing content",
                        existing.request_id
                    )
                }
            }
            None
        }
    };
    Ok(CorrelateAndBeginResult {
        episode_id: existing.episode_id.clone(),
        source,
        completeness,
        prior_events,
        guarded_route,
    })
}

async fn append_guarded_route_in_tx(
    txn: &DatabaseTransaction,
    owner_user_id: &str,
    prior_events: &[TrajectoryEvent],
    start: &TrajectoryEvent,
    completeness: HistoryCompleteness,
    input: &GuardedRouteInput,
) -> Result<GuardedRouteResult> {
    let batch = build_guarded_route_batch(prior_events, start, completeness, input)?;
    let mut candidate_history = Vec::with_capacity(
        prior_events
            .len()
            .checked_add(3)
            .ok_or_else(|| anyhow::anyhow!("guarded route history capacity overflow"))?,
    );
    candidate_history.extend_from_slice(prior_events);
    candidate_history.push(start.clone());
    candidate_history.push(batch.route_event.clone());
    if let Some(guard_event) = &batch.guard_event {
        candidate_history.push(guard_event.clone());
    }
    reduce(&candidate_history, &input.policy.protected_tiers)?;
    let episode = owned_episode(txn, owner_user_id, &start.episode_id).await?;
    validate_sequence(&episode, &batch.route_event)?;
    validate_event(&batch.route_event)?;
    append_event(txn, &batch.route_event).await?;
    update_episode_head(txn, episode, &batch.route_event, None).await?;
    if let Some(guard_event) = &batch.guard_event {
        let episode = owned_episode(txn, owner_user_id, &start.episode_id).await?;
        validate_sequence(&episode, guard_event)?;
        validate_event(guard_event)?;
        append_event(txn, guard_event).await?;
        update_episode_head(txn, episode, guard_event, None).await?;
    }
    Ok(batch.result)
}

fn build_guarded_route_batch(
    prior_events: &[TrajectoryEvent],
    start: &TrajectoryEvent,
    completeness: HistoryCompleteness,
    input: &GuardedRouteInput,
) -> Result<GuardedRouteBatch> {
    let prior_snapshot = if prior_events.is_empty() {
        None
    } else {
        Some(reduce(prior_events, &input.policy.protected_tiers)?)
    };
    let mut through_start = Vec::with_capacity(
        prior_events
            .len()
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("guard snapshot history capacity overflow"))?,
    );
    through_start.extend_from_slice(prior_events);
    through_start.push(start.clone());
    let pre_intent_snapshot = reduce(&through_start, &input.policy.protected_tiers)?;
    let mut evaluation = evaluate(
        &input.policy,
        ProgressGuardInput {
            prior_snapshot: prior_snapshot.as_ref(),
            pre_intent_snapshot: &pre_intent_snapshot,
            correlation_completeness: completeness,
            current_projection: &input.projection,
            candidate_tier: input.candidate_tier.as_deref(),
            policy_digest: &input.policy_digest,
        },
    )?;
    evaluation.intent.experiment = input.experiment.clone();
    let before_tool_floor = evaluation.intent.selected_tier.clone();
    let tool_floor_applied = input.carries_tools
        && before_tool_floor.as_ref().is_some_and(|tier| {
            !input.tool_safe_tiers.contains(tier)
                && input
                    .tool_use_tier
                    .as_deref()
                    .is_some_and(|floor| floor != tier)
        });
    let tool_floor_explanation = if tool_floor_applied {
        let floor = input
            .tool_use_tier
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("tool floor disappeared during guard evaluation"))?;
        if !input.policy.protected_tiers.contains(floor) {
            anyhow::bail!("tool safety floor must remain a protected progress-guard tier")
        }
        evaluation.intent.selected_tier = Some(floor.clone());
        "tool-carrying request requires the configured protected tool floor"
    } else if !input.carries_tools {
        "request carries no tools"
    } else if before_tool_floor
        .as_ref()
        .is_some_and(|tier| input.tool_safe_tiers.contains(tier))
    {
        "selected tier is already tool-safe"
    } else {
        "no distinct tool safety floor is configured"
    };
    evaluation.intent.clauses.push(RouteIntentClause {
        clause_id: "tool_safety.floor".to_string(),
        disposition: if tool_floor_applied {
            RouteIntentClauseDisposition::Applied
        } else {
            RouteIntentClauseDisposition::Skipped
        },
        explanation: tool_floor_explanation.to_string(),
    });

    let route_sequence = start
        .sequence
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("route intent sequence overflow"))?;
    let mut categorical = std::collections::BTreeMap::from([
        ("route.projection".to_owned(), input.projection.key()),
        (
            "route.workflow_state".to_owned(),
            input.projection.state_kind.to_string(),
        ),
        ("route.policy".to_owned(), input.policy_name.clone()),
        ("route.request_key".to_owned(), input.request_key.clone()),
        ("route.eval_schema".to_owned(), "trajectory.v1".to_owned()),
    ]);
    if let Some(baseline_tier) = &input.baseline_tier {
        categorical.insert("route.baseline_tier".to_owned(), baseline_tier.clone());
    }
    if let Some(baseline_effort) = input.baseline_effort {
        categorical.insert(
            "route.baseline_effort".to_owned(),
            baseline_effort.to_string(),
        );
    }
    if let Some(preset) = &input.preset {
        categorical.insert("route.preset".to_owned(), preset.clone());
    }
    if let Some(candidate) = &evaluation.intent.candidate_tier {
        categorical.insert("route.candidate_tier".to_owned(), candidate.clone());
    }
    if let Some(selected) = &evaluation.intent.selected_tier {
        categorical.insert("route.selected_tier".to_owned(), selected.clone());
        if let Some(effort) = input.tier_efforts.get(selected) {
            categorical.insert("route.selected_effort".to_owned(), effort.to_string());
        }
    }
    if let Some(experiment) = &evaluation.intent.experiment {
        categorical.insert(
            "route.experiment_id".to_owned(),
            experiment.experiment_id.clone(),
        );
        categorical.insert(
            "route.experiment_arm".to_owned(),
            match experiment.arm {
                crate::eval::types::ExperimentArm::Control => "control",
                crate::eval::types::ExperimentArm::Challenger => "challenger",
            }
            .to_owned(),
        );
        categorical.insert(
            "route.experiment_assignment_unit".to_owned(),
            match experiment.assignment_unit {
                crate::eval::types::ExperimentAssignmentUnit::Task => "task",
                crate::eval::types::ExperimentAssignmentUnit::Episode => "episode",
            }
            .to_owned(),
        );
    }
    for (index, clause) in evaluation.intent.clauses.iter().enumerate() {
        let prefix = format!("route.clause_{index:02}");
        categorical.insert(format!("{prefix}.id"), clause.clause_id.clone());
        categorical.insert(
            format!("{prefix}.disposition"),
            match clause.disposition {
                RouteIntentClauseDisposition::Applied => "applied",
                RouteIntentClauseDisposition::Skipped => "skipped",
            }
            .to_string(),
        );
    }
    let applied_clause_count = u64::try_from(
        evaluation
            .intent
            .clauses
            .iter()
            .filter(|clause| clause.disposition == RouteIntentClauseDisposition::Applied)
            .count(),
    )
    .context("too many applied route clauses")?;
    let mut route_event = TrajectoryEvent {
        schema_version: TRAJECTORY_SCHEMA_VERSION,
        event_id: input.route_event_id.clone(),
        owner_user_id: start.owner_user_id.clone(),
        episode_id: start.episode_id.clone(),
        request_id: start.request_id.clone(),
        sequence: route_sequence,
        kind: TrajectoryEventKind::RouteIntentRecorded,
        evidence: TrajectoryEvidence {
            structural: std::collections::BTreeMap::from([
                (
                    "route.applied_clause_count".to_owned(),
                    applied_clause_count,
                ),
                (
                    "route.selected_is_protected".to_owned(),
                    u64::from(
                        evaluation
                            .intent
                            .selected_tier
                            .as_ref()
                            .is_some_and(|tier| input.policy.protected_tiers.contains(tier)),
                    ),
                ),
            ]),
            categorical,
            digests: std::collections::BTreeMap::from([
                (
                    "route.health_snapshot".to_owned(),
                    evaluation.intent.trajectory_snapshot_digest.clone(),
                ),
                (
                    "route.policy_lock".to_owned(),
                    evaluation.intent.policy_digest.clone(),
                ),
            ]),
        },
        captured_at: start.captured_at.clone(),
        content_digest: String::new(),
    };
    if let Some(experiment) = &evaluation.intent.experiment {
        route_event.evidence.structural.insert(
            "route.experiment_challenger_propensity_ppm".to_owned(),
            u64::from(experiment.challenger_propensity_ppm),
        );
        route_event.evidence.digests.insert(
            "route.experiment_assignment_id".to_owned(),
            experiment.assignment_id_digest.clone(),
        );
    }
    route_event.content_digest = route_event.semantic_digest()?;

    let guard_event = if evaluation.activated {
        let mut event = TrajectoryEvent {
            schema_version: TRAJECTORY_SCHEMA_VERSION,
            event_id: input.guard_event_id.clone(),
            owner_user_id: start.owner_user_id.clone(),
            episode_id: start.episode_id.clone(),
            request_id: start.request_id.clone(),
            sequence: route_sequence
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("guard activation sequence overflow"))?,
            kind: TrajectoryEventKind::GuardActivated,
            evidence: TrajectoryEvidence {
                structural: std::collections::BTreeMap::from([(
                    "guard.hold_for_requests".to_owned(),
                    input.policy.hold_for_requests,
                )]),
                categorical: std::collections::BTreeMap::new(),
                digests: std::collections::BTreeMap::from([
                    (
                        "guard.health_snapshot".to_owned(),
                        evaluation.intent.trajectory_snapshot_digest.clone(),
                    ),
                    (
                        "guard.policy_lock".to_owned(),
                        evaluation.intent.policy_digest.clone(),
                    ),
                ]),
            },
            captured_at: start.captured_at.clone(),
            content_digest: String::new(),
        };
        event.content_digest = event.semantic_digest()?;
        Some(event)
    } else {
        None
    };
    Ok(GuardedRouteBatch {
        route_event,
        guard_event,
        result: GuardedRouteResult {
            snapshot: pre_intent_snapshot,
            intent: evaluation.intent,
            guard_activated: evaluation.activated,
            tool_floor_applied,
            route_sequence,
            causal_completeness: evaluation.causal_completeness,
        },
    })
}

async fn validate_stored_native_parent(
    txn: &DatabaseTransaction,
    owner_user_id: &str,
    native_parent_id: Option<&str>,
) -> Result<()> {
    let Some(native_parent_id) = native_parent_id else {
        return Ok(());
    };
    let parent = request_entity::Entity::find_by_id(native_parent_id)
        .one(txn)
        .await?
        .ok_or_else(|| anyhow::anyhow!("trajectory retry has a missing stored native parent"))?;
    if parent.owner_user_id != owner_user_id {
        anyhow::bail!("trajectory retry has a cross-owner stored native parent")
    }
    Ok(())
}

fn validate_episode_head(
    episode: &episode_entity::Model,
    events: &[TrajectoryEvent],
) -> Result<()> {
    let last = events
        .last()
        .ok_or_else(|| anyhow::anyhow!("trajectory episode head has no event history"))?;
    let expected_next = last
        .sequence
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("trajectory episode head sequence overflow"))?;
    if u64::try_from(episode.next_sequence)
        .context("trajectory episode head sequence is negative")?
        != expected_next
    {
        anyhow::bail!("trajectory episode head next_sequence disagrees with event history")
    }
    let latest_request_id = events
        .iter()
        .rev()
        .find(|event| event.kind == TrajectoryEventKind::RequestStarted)
        .and_then(|event| event.request_id.as_deref());
    if episode.latest_request_id.as_deref() != latest_request_id {
        anyhow::bail!("trajectory episode head latest_request_id disagrees with event history")
    }
    match (
        last.kind == TrajectoryEventKind::EpisodeClosed,
        &episode.closed_at,
    ) {
        (false, None) => {}
        (true, Some(closed_at)) if closed_at == &last.captured_at => {}
        _ => anyhow::bail!("trajectory episode head closed state disagrees with event history"),
    }
    Ok(())
}

async fn reserve_episode_head(
    txn: &DatabaseTransaction,
    owner_user_id: &str,
    episode_id: &str,
    expected_sequence: u64,
    request_id: &str,
    captured_at: &str,
    completeness: HistoryCompleteness,
) -> Result<bool> {
    let expected =
        i64::try_from(expected_sequence).context("trajectory sequence exceeds database range")?;
    let next = expected
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("trajectory sequence overflow"))?;
    let result = episode_entity::Entity::update_many()
        .col_expr(episode_entity::Column::NextSequence, Expr::value(next))
        .col_expr(
            episode_entity::Column::LastCapturedAt,
            Expr::value(captured_at.to_owned()),
        )
        .col_expr(
            episode_entity::Column::LatestRequestId,
            Expr::value(request_id.to_owned()),
        )
        .col_expr(
            episode_entity::Column::HistoryCompleteness,
            Expr::value(completeness_name(completeness)),
        )
        .filter(episode_entity::Column::OwnerUserId.eq(owner_user_id))
        .filter(episode_entity::Column::EpisodeId.eq(episode_id))
        .filter(episode_entity::Column::NextSequence.eq(expected))
        .filter(episode_entity::Column::ClosedAt.is_null())
        .exec(txn)
        .await?;
    Ok(result.rows_affected == 1)
}

#[derive(Default)]
struct DatabaseErrorClassification {
    retryable_contention: bool,
    unique_violation: bool,
}

fn classify_database_error(error: &anyhow::Error) -> DatabaseErrorClassification {
    let mut classification = DatabaseErrorClassification::default();
    for cause in error.chain() {
        let Some(db_error) = cause.downcast_ref::<DbErr>().and_then(sqlx_database_error) else {
            continue;
        };
        classification.unique_violation |= db_error.is_unique_violation();
        let code = db_error.code();
        let code = code.as_deref();
        let mysql_number = db_error
            .try_downcast_ref::<sea_orm::SqlxMySqlError>()
            .map(sea_orm::SqlxMySqlError::number);
        classification.retryable_contention |= retryable_database_code(code, mysql_number);
    }
    classification
}

fn sqlx_database_error(
    error: &DbErr,
) -> Option<&(dyn sea_orm::sqlx::error::DatabaseError + 'static)> {
    match error {
        DbErr::Exec(RuntimeErr::SqlxError(sea_orm::SqlxError::Database(error)))
        | DbErr::Query(RuntimeErr::SqlxError(sea_orm::SqlxError::Database(error))) => {
            Some(error.as_ref())
        }
        _ => None,
    }
}

fn retryable_database_code(code: Option<&str>, mysql_number: Option<u16>) -> bool {
    if matches!(code, Some("40001" | "40P01")) || matches!(mysql_number, Some(1205 | 1213)) {
        return true;
    }
    let Some(code) = code else {
        return false;
    };
    if matches!(code, "1205" | "1213") {
        return true;
    }
    code.len() <= 4
        && code
            .parse::<i32>()
            .is_ok_and(|sqlite_code| matches!(sqlite_code & 0xff, 5 | 6))
}

fn contention_backoff(attempt: usize) -> std::time::Duration {
    std::time::Duration::from_millis(1_u64 << attempt.min(5))
}

fn parse_correlation_source(value: &str) -> Result<CorrelationSource> {
    match value {
        "native_parent_id" => Ok(CorrelationSource::NativeParentId),
        "canonical_prefix" => Ok(CorrelationSource::CanonicalPrefix),
        "explicit_root" => Ok(CorrelationSource::ExplicitRoot),
        "unresolved" => Ok(CorrelationSource::Unresolved),
        _ => anyhow::bail!("stored trajectory event has invalid correlation source"),
    }
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
        .one(txn)
        .await?
        .is_some()
    {
        anyhow::bail!("trajectory event id is already owned by another user")
    }
    Ok(false)
}

/// Holds a new request start at the episode head when the wall clock disagrees
/// with the episode's own ordering.
///
/// A start timestamp is `Utc::now()` backdated by the pipeline's monotonic
/// elapsed time, while the preceding settlement is its start plus a monotonic
/// duration. The two readings round independently, so a request arriving within
/// a couple of milliseconds of its predecessor's settlement can carry a start
/// that precedes the event it follows — which the reducer rejects as a
/// regression. Settlement already pins itself to the last event for the same
/// reason; do the same here rather than let a sub-millisecond turnaround fail
/// the request.
fn episode_monotonic_captured_at(
    prior_events: &[TrajectoryEvent],
    captured_at: &str,
) -> Result<String> {
    let Some(head) = prior_events.last() else {
        return Ok(captured_at.to_owned());
    };
    let observed = chrono::DateTime::parse_from_rfc3339(captured_at)
        .context("parsing trajectory request start timestamp")?;
    let head_captured_at = chrono::DateTime::parse_from_rfc3339(&head.captured_at)
        .context("parsing trajectory episode head timestamp")?;
    if observed < head_captured_at {
        return Ok(head.captured_at.clone());
    }
    Ok(captured_at.to_owned())
}

fn validate_candidate_history(
    events: &[TrajectoryEvent],
    candidate: &TrajectoryEvent,
) -> Result<()> {
    let mut candidate_history = Vec::with_capacity(
        events
            .len()
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("trajectory candidate history capacity overflow"))?,
    );
    candidate_history.extend_from_slice(events);
    candidate_history.push(candidate.clone());
    reduce(&candidate_history, &std::collections::BTreeSet::new())?;
    Ok(())
}

fn mutation_error_is_retryable(error: &anyhow::Error) -> bool {
    let classification = classify_database_error(error);
    classification.retryable_contention || classification.unique_violation
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

fn validate_new_episode_start(input: &BeginRequest) -> Result<()> {
    let event_completeness = match input
        .event
        .evidence
        .categorical
        .get("history.completeness")
        .map(String::as_str)
    {
        Some(value) => parse_completeness(value)?,
        None => HistoryCompleteness::Unknown,
    };
    if event_completeness != input.episode.completeness {
        anyhow::bail!("new trajectory episode completeness disagrees with its start event")
    }

    let episode_source = parse_correlation_source(&input.episode.correlation_source)?;
    let event_source = input
        .event
        .evidence
        .categorical
        .get("correlation.source")
        .map(|source| parse_correlation_source(source))
        .transpose()?
        .unwrap_or(CorrelationSource::Unresolved);
    if event_source != episode_source {
        anyhow::bail!("new trajectory episode correlation source disagrees with its start event")
    }

    let correlation_key_id = input.episode.correlation_key_id.as_str();
    if input.full_input_digest.key_id() != correlation_key_id {
        anyhow::bail!("new trajectory episode and full-input digest use different key ids")
    }
    if let Some(native_parent_digest) = input
        .event
        .evidence
        .digests
        .get("correlation.native_parent")
    {
        let native_parent_digest = KeyedDigest::parse(native_parent_digest.clone())?;
        if native_parent_digest.key_id() != correlation_key_id {
            anyhow::bail!("new trajectory episode and native-parent evidence use different key ids")
        }
    }
    Ok(())
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

fn stored_episode(row: episode_entity::Model) -> Result<StoredEpisode> {
    Ok(StoredEpisode {
        episode_id: row.episode_id,
        owner_user_id: row.owner_user_id,
        correlation_source: row.correlation_source,
        correlation_key_id: row.correlation_key_id,
        completeness: parse_completeness(&row.history_completeness)?,
        next_sequence: u64::try_from(row.next_sequence)
            .context("stored trajectory episode next_sequence is negative")?,
        first_captured_at: row.first_captured_at,
        last_captured_at: row.last_captured_at,
        closed_at: row.closed_at,
        latest_request_id: row.latest_request_id,
    })
}

fn first_reducer_failure(events: &[TrajectoryEvent]) -> Option<(Option<String>, Option<u64>)> {
    for end in 1..=events.len() {
        match reduce_prefix(&events[..end], &std::collections::BTreeSet::new()) {
            Ok(PrefixReduction::Complete(_) | PrefixReduction::AwaitingGuardActivation) => {
                continue;
            }
            Err(_) => {}
        }
        let event = events.get(end.saturating_sub(1));
        return Some((
            event.map(|event| event.event_id.clone()),
            event.map(|event| event.sequence),
        ));
    }
    None
}

fn stored_outbox(row: outbox_entity::Model) -> Result<PendingOutbox> {
    let payload = serde_json::from_str::<super::types::OutboxPayload>(&row.payload_json)
        .context("stored trajectory outbox payload is invalid")?;
    validate_outbox_payload(&payload)?;
    if canonical_digest(&payload)? != row.payload_digest {
        anyhow::bail!("stored trajectory outbox payload digest does not match its content")
    }
    Ok(PendingOutbox {
        outbox_id: row.outbox_id,
        owner_user_id: row.owner_user_id,
        topic: row.topic,
        payload,
        payload_json: row.payload_json,
        payload_digest: row.payload_digest,
        attempts: u64::try_from(row.attempts).context("stored outbox attempts are negative")?,
        created_at: row.created_at,
    })
}

fn stored_outbox_batch_item(row: outbox_entity::Model) -> OutboxBatchItem {
    let payload = serde_json::from_str::<super::types::OutboxPayload>(&row.payload_json)
        .ok()
        .filter(|payload| validate_outbox_payload(payload).is_ok())
        .filter(|payload| {
            canonical_digest(payload).is_ok_and(|digest| digest == row.payload_digest)
        });
    OutboxBatchItem {
        outbox_id: row.outbox_id,
        owner_user_id: row.owner_user_id,
        topic: row.topic,
        payload,
    }
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
    use std::borrow::Cow;
    use std::collections::{BTreeMap, BTreeSet};
    use std::fmt;
    use std::sync::Arc;

    use sea_orm::sqlx::error::{DatabaseError, ErrorKind};
    use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, QueryTrait, RuntimeErr, Statement};

    use super::*;
    use crate::eval::types::{EvalExperimentRef, ExperimentArm, ExperimentAssignmentUnit};
    use crate::trajectory::evaluation::build_operational_evaluation;
    use crate::trajectory::guard::{IncompleteHistoryAction, ProgressGuardPolicy};
    use crate::trajectory::replay::replay_episode;
    use crate::trajectory::types::*;
    use crate::workflow_state::ir::RouteProjection;

    const POLICY_DIGEST: &str =
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[tokio::test]
    async fn guarded_root_rolls_back_every_partial_batch_failure() -> anyhow::Result<()> {
        for failure in ["route_validation", "guard_insert"] {
            let store = store().await?;
            let mut route = guarded_route_input("route-1", "guard-1");
            if failure == "route_validation" {
                route.policy_digest = "invalid".into();
            } else {
                route.guard_event_id = "start-1".into();
            }
            let result = store
                .correlate_and_begin(
                    "owner-a",
                    correlate_input("request-1", "episode-1", "start-1", route),
                )
                .await;
            assert!(result.is_err(), "failure point {failure}");
            assert_trajectory_tables_empty(&store).await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn prefix_lookup_returns_bounded_evidence_for_high_cardinality_digest()
    -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let digest = keyed_digest("key-1", "9");
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            r#"WITH digits(n) AS (VALUES (0),(1),(2),(3),(4),(5),(6),(7),(8),(9)),
               fixture_rows(value) AS (
                 SELECT a.n * 1000 + b.n * 100 + c.n * 10 + d.n
                 FROM digits a CROSS JOIN digits b CROSS JOIN digits c CROSS JOIN digits d
               )
               INSERT INTO trajectory_requests (
                 request_id, owner_user_id, episode_id, start_event_id,
                 settlement_event_id, settlement_outbox_id, full_input_digest,
                 native_parent_id, protocol, status
               )
               SELECT 'request-' || value, 'owner-a', 'episode-' || value,
                      'start-' || value, NULL, NULL, ?, NULL, 'responses', 'settled'
               FROM fixture_rows"#,
            [digest.as_str().into()],
        ))
        .await?;

        let txn = db.begin().await?;
        record_prefix_episode(&txn, "owner-a", digest.as_str(), "episode-0").await?;
        record_prefix_episode(&txn, "owner-a", digest.as_str(), "episode-1").await?;
        let rows = load_prefix_evidence(
            &txn,
            "owner-a",
            BTreeSet::from([digest.as_str().to_owned()]),
        )
        .await?;
        assert_eq!(rows.len(), 1);
        assert!(rows[0].ambiguous);
        assert!(matches!(
            find_prefix_episode(&txn, "owner-a", std::slice::from_ref(&digest)).await?,
            PrefixResolution::Ambiguous
        ));
        txn.rollback().await?;
        Ok(())
    }

    #[test]
    fn prefix_summary_upsert_and_ambiguity_update_render_for_every_database_backend() {
        for backend in [
            DatabaseBackend::Sqlite,
            DatabaseBackend::Postgres,
            DatabaseBackend::MySql,
        ] {
            let insert = prefix_entity::Entity::insert(prefix_entity::ActiveModel {
                owner_user_id: Set("owner-a".to_owned()),
                full_input_digest: Set(keyed_digest("key-1", "9").as_str().to_owned()),
                episode_id: Set("episode-a".to_owned()),
                ambiguous: Set(false),
            })
            .on_conflict(prefix_insert_conflict())
            .build(backend);
            let insert_sql = insert.sql.to_ascii_lowercase();
            match backend {
                DatabaseBackend::MySql => {
                    assert!(insert_sql.contains("on duplicate key update"));
                }
                DatabaseBackend::Postgres | DatabaseBackend::Sqlite => {
                    assert!(insert_sql.contains("on conflict"));
                    assert!(insert_sql.contains("do nothing"));
                }
            }

            let update = prefix_entity::Entity::update_many()
                .col_expr(prefix_entity::Column::Ambiguous, Expr::value(true))
                .filter(prefix_entity::Column::OwnerUserId.eq("owner-a"))
                .filter(prefix_entity::Column::FullInputDigest.eq("digest-a"))
                .filter(prefix_entity::Column::EpisodeId.ne("episode-a"))
                .filter(prefix_entity::Column::Ambiguous.eq(false))
                .build(backend);
            let update_sql = update.sql.to_ascii_lowercase();
            assert!(update_sql.starts_with("update"));
            assert!(update_sql.contains("trajectory_prefix_index"));
            assert!(update_sql.contains("ambiguous"));
            assert!(update_sql.contains("owner_user_id"));
            assert!(update_sql.contains("full_input_digest"));
            assert!(update_sql.contains("episode_id"));
        }
    }

    #[tokio::test]
    async fn guarded_exact_retry_is_one_immutable_batch() -> anyhow::Result<()> {
        let store = store().await?;
        let route = guarded_route_input("route-1", "guard-1");
        let input = correlate_input("request-1", "episode-1", "start-1", route.clone());
        let first = store.correlate_and_begin("owner-a", input).await?;
        let first_route = first
            .guarded_route
            .ok_or_else(|| anyhow::anyhow!("missing first guarded route"))?;
        assert!(first_route.guard_activated);

        let retry = store
            .correlate_and_begin(
                "owner-a",
                correlate_input("request-1", "episode-1", "start-1", route.clone()),
            )
            .await?;
        assert_eq!(retry.guarded_route, Some(first_route));
        assert_eq!(
            store
                .events_for_episode("owner-a", "episode-1")
                .await?
                .len(),
            3
        );

        let mut changed_canonical_window =
            correlate_input("request-1", "episode-1", "start-1", route.clone());
        changed_canonical_window.ancestor_prefixes_truncated = true;
        let conflict = store
            .correlate_and_begin("owner-a", changed_canonical_window)
            .await
            .err()
            .ok_or_else(|| {
                anyhow::anyhow!("exact retry accepted changed canonical-window truncation")
            })?;
        assert!(conflict.to_string().contains("different content"));

        let mut changed_policy = route.clone();
        changed_policy.policy_digest =
            "sha256:1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into();
        let conflict = store
            .correlate_and_begin(
                "owner-a",
                correlate_input("request-1", "episode-1", "start-1", changed_policy),
            )
            .await;
        assert!(conflict.is_err());
        assert_eq!(
            store
                .events_for_episode("owner-a", "episode-1")
                .await?
                .len(),
            3
        );

        let barrier = Arc::new(tokio::sync::Barrier::new(8));
        let tasks = (0..8)
            .map(|_| {
                let store = store.clone();
                let barrier = barrier.clone();
                let route = route.clone();
                tokio::spawn(async move {
                    barrier.wait().await;
                    store
                        .correlate_and_begin(
                            "owner-a",
                            correlate_input("request-1", "episode-1", "start-1", route),
                        )
                        .await
                })
            })
            .collect::<Vec<_>>();
        for task in tasks {
            task.await??;
        }
        assert_eq!(
            store
                .events_for_episode("owner-a", "episode-1")
                .await?
                .len(),
            3
        );
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_first_guarded_commit_converges_without_mixed_batches() -> anyhow::Result<()>
    {
        let (store, _directory) = file_store().await?;
        let barrier = Arc::new(tokio::sync::Barrier::new(8));
        let tasks = (0..8)
            .map(|_| {
                let store = store.clone();
                let barrier = barrier.clone();
                tokio::spawn(async move {
                    let route = guarded_route_input("route-1", "guard-1");
                    barrier.wait().await;
                    store
                        .correlate_and_begin(
                            "owner-a",
                            correlate_input("request-1", "episode-1", "start-1", route),
                        )
                        .await
                })
            })
            .collect::<Vec<_>>();
        let mut selected = None;
        for task in tasks {
            let result = task.await??;
            let route = result
                .guarded_route
                .ok_or_else(|| anyhow::anyhow!("concurrent commit lost guarded route"))?;
            if let Some(expected) = &selected {
                assert_eq!(expected, &route);
            } else {
                selected = Some(route);
            }
        }
        assert_eq!(
            store
                .events_for_episode("owner-a", "episode-1")
                .await?
                .len(),
            3
        );
        assert!(store.request("owner-a", "request-1").await?.is_some());

        let (conflict_store, _conflict_directory) = file_store().await?;
        let conflict_barrier = Arc::new(tokio::sync::Barrier::new(2));
        let tasks = [
            POLICY_DIGEST,
            "sha256:1123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ]
        .into_iter()
        .map(|digest| {
            let store = conflict_store.clone();
            let barrier = conflict_barrier.clone();
            tokio::spawn(async move {
                let mut route = guarded_route_input("route-1", "guard-1");
                route.policy_digest = digest.into();
                barrier.wait().await;
                store
                    .correlate_and_begin(
                        "owner-a",
                        correlate_input("request-1", "episode-1", "start-1", route),
                    )
                    .await
            })
        })
        .collect::<Vec<_>>();
        let mut successes = 0;
        let mut conflicts = 0;
        for task in tasks {
            match task.await? {
                Ok(_) => successes += 1,
                Err(error) => {
                    conflicts += 1;
                    assert!(error.to_string().contains("different route intent"));
                }
            }
        }
        assert_eq!((successes, conflicts), (1, 1));
        assert_eq!(
            conflict_store
                .events_for_episode("owner-a", "episode-1")
                .await?
                .len(),
            3
        );
        Ok(())
    }

    #[tokio::test]
    async fn guarded_route_persists_exact_named_policy_evaluation_identity() -> anyhow::Result<()> {
        let store = store().await?;
        let result = store
            .correlate_and_begin(
                "owner-a",
                correlate_input(
                    "request-identity",
                    "episode-identity",
                    "start-identity",
                    guarded_route_input("route-identity", "guard-identity"),
                ),
            )
            .await?;
        let route_sequence = result
            .guarded_route
            .ok_or_else(|| anyhow::anyhow!("guarded route missing"))?
            .route_sequence;
        let events = store
            .events_for_episode("owner-a", "episode-identity")
            .await?;
        let route = events
            .iter()
            .find(|event| event.sequence == route_sequence)
            .ok_or_else(|| anyhow::anyhow!("route event missing"))?;
        assert_eq!(
            route.evidence.categorical.get("route.policy"),
            Some(&"auto:cost".to_string())
        );
        assert_eq!(
            route.evidence.categorical.get("route.request_key"),
            Some(&"agent_trace/v2|edit|normal".to_string())
        );
        assert_eq!(
            route.evidence.categorical.get("route.baseline_tier"),
            Some(&"reference".to_string())
        );
        assert_eq!(
            route.evidence.categorical.get("route.preset"),
            Some(&"auto:cost".to_string())
        );
        assert_eq!(
            route.evidence.structural.get("route.selected_is_protected"),
            Some(&1)
        );
        Ok(())
    }

    #[tokio::test]
    async fn guarded_existing_episode_failure_preserves_original_head() -> anyhow::Result<()> {
        let store = store().await?;
        store
            .begin_request("owner-a", begin("episode-1", "request-0"))
            .await?;
        store
            .append_route_intent(
                "owner-a",
                route_event("episode-1", "request-0", 2, "route-0"),
            )
            .await?;
        let before = store.events_for_episode("owner-a", "episode-1").await?;

        let mut route = guarded_route_input("route-1", "guard-1");
        route.policy_digest = "invalid".into();
        let mut input = correlate_input("request-1", "unused-new-episode", "start-1", route);
        input.ancestor_prefix_digests = vec![keyed_digest("key-1", "2")];
        input.starts_with_prior_turns = true;
        assert!(store.correlate_and_begin("owner-a", input).await.is_err());

        assert_eq!(
            store.events_for_episode("owner-a", "episode-1").await?,
            before
        );
        assert!(store.request("owner-a", "request-1").await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn continuation_start_behind_the_episode_head_is_held_at_the_head() -> anyhow::Result<()>
    {
        let store = store().await?;
        seed_started_episode(
            &store,
            "owner-a",
            "episode-1",
            "request-0",
            "2026-08-01T00:00:00.010Z",
        )
        .await?;

        let route = guarded_route_input("route-1", "guard-1");
        let mut input = correlate_input("request-1", "unused-new-episode", "start-1", route);
        input.ancestor_prefix_digests = vec![keyed_digest("key-1", "2")];
        input.starts_with_prior_turns = true;
        input.captured_at = "2026-08-01T00:00:00.008Z".into();
        store.correlate_and_begin("owner-a", input).await?;

        let events = store.events_for_episode("owner-a", "episode-1").await?;
        let start = events
            .iter()
            .find(|event| event.request_id.as_deref() == Some("request-1"))
            .ok_or_else(|| anyhow::anyhow!("continuation start was not persisted"))?;
        assert_eq!(start.captured_at, "2026-08-01T00:00:00.010Z");
        assert_eq!(
            store
                .episode("owner-a", "episode-1")
                .await?
                .ok_or_else(|| anyhow::anyhow!("episode is missing"))?
                .last_captured_at,
            "2026-08-01T00:00:00.010Z"
        );
        reduce(&events, &BTreeSet::new())?;
        Ok(())
    }

    #[tokio::test]
    async fn continuation_start_ahead_of_the_episode_head_keeps_its_own_timestamp()
    -> anyhow::Result<()> {
        let store = store().await?;
        seed_started_episode(
            &store,
            "owner-a",
            "episode-1",
            "request-0",
            "2026-08-01T00:00:00.010Z",
        )
        .await?;

        let route = guarded_route_input("route-1", "guard-1");
        let mut input = correlate_input("request-1", "unused-new-episode", "start-1", route);
        input.ancestor_prefix_digests = vec![keyed_digest("key-1", "2")];
        input.starts_with_prior_turns = true;
        input.captured_at = "2026-08-01T00:00:00.025Z".into();
        store.correlate_and_begin("owner-a", input).await?;

        let events = store.events_for_episode("owner-a", "episode-1").await?;
        let start = events
            .iter()
            .find(|event| event.request_id.as_deref() == Some("request-1"))
            .ok_or_else(|| anyhow::anyhow!("continuation start was not persisted"))?;
        assert_eq!(start.captured_at, "2026-08-01T00:00:00.025Z");
        Ok(())
    }

    #[test]
    fn persisted_hold_covers_exactly_n_subsequent_requests() -> anyhow::Result<()> {
        let policy = guarded_route_input("route-1", "guard-1");
        let mut events = Vec::new();

        let first = guarded_start("request-1", 1)?;
        let first_batch =
            build_guarded_route_batch(&events, &first, HistoryCompleteness::Complete, &policy)?;
        assert!(first_batch.guard_event.is_some());
        events.push(first);
        events.push(first_batch.route_event);
        if let Some(guard) = first_batch.guard_event {
            events.push(guard);
        }
        assert_eq!(
            reduce(&events, &policy.policy.protected_tiers)?.active_hold_remaining,
            2
        );

        for (request, expected_remaining) in [("request-2", 1), ("request-3", 0)] {
            let sequence = u64::try_from(events.len())?
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("test sequence overflow"))?;
            let start = guarded_start(request, sequence)?;
            let mut input = policy.clone();
            input.route_event_id = format!("route-{request}");
            input.guard_event_id = format!("guard-{request}");
            let batch =
                build_guarded_route_batch(&events, &start, HistoryCompleteness::Complete, &input)?;
            assert!(batch.guard_event.is_none(), "active hold must not reset");
            events.push(start);
            events.push(batch.route_event);
            assert_eq!(
                reduce(&events, &policy.policy.protected_tiers)?.active_hold_remaining,
                expected_remaining
            );
        }

        let sequence = u64::try_from(events.len())?
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("test sequence overflow"))?;
        let fourth = guarded_start("request-4", sequence)?;
        let mut fourth_input = policy.clone();
        fourth_input.route_event_id = "route-request-4".into();
        fourth_input.guard_event_id = "guard-request-4".into();
        let fourth_batch = build_guarded_route_batch(
            &events,
            &fourth,
            HistoryCompleteness::Complete,
            &fourth_input,
        )?;
        assert!(fourth_batch.guard_event.is_some());
        Ok(())
    }

    #[test]
    fn tool_floor_runs_after_progress_guard_and_remains_protected() -> anyhow::Result<()> {
        let start = guarded_start("request-1", 1)?;
        let mut input = guarded_route_input("route-1", "guard-1");
        input.policy.protected_tiers.insert("tool-protected".into());
        input.carries_tools = true;
        input.tool_use_tier = Some("tool-protected".into());
        input.tool_safe_tiers = BTreeSet::from(["tool-protected".into()]);

        let batch = build_guarded_route_batch(&[], &start, HistoryCompleteness::Complete, &input)?;

        assert!(batch.result.guard_activated);
        assert!(batch.result.tool_floor_applied);
        assert_eq!(
            batch.result.intent.selected_tier.as_deref(),
            Some("tool-protected")
        );
        assert!(batch.result.intent.clauses.iter().any(|clause| {
            clause.clause_id == "progress_guard.max_episode_requests"
                && clause.disposition == RouteIntentClauseDisposition::Applied
        }));
        assert!(batch.result.intent.clauses.iter().any(|clause| {
            clause.clause_id == "tool_safety.floor"
                && clause.disposition == RouteIntentClauseDisposition::Applied
        }));
        Ok(())
    }

    #[test]
    fn guarded_route_producer_emits_complete_experiment_evidence() -> anyhow::Result<()> {
        let start = guarded_start("request-1", 1)?;
        let mut input = guarded_route_input("route-experiment", "guard-experiment");
        input.experiment = Some(EvalExperimentRef {
            experiment_id: POLICY_DIGEST.into(),
            arm: ExperimentArm::Challenger,
            assignment_unit: ExperimentAssignmentUnit::Task,
            assignment_id_digest:
                "sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".into(),
            challenger_propensity_ppm: 100_000,
        });

        let batch = build_guarded_route_batch(&[], &start, HistoryCompleteness::Complete, &input)?;
        let evidence = &batch.route_event.evidence;
        assert_eq!(
            evidence.categorical.get("route.experiment_id"),
            Some(&POLICY_DIGEST.into())
        );
        assert_eq!(
            evidence.categorical.get("route.experiment_arm"),
            Some(&"challenger".into())
        );
        assert_eq!(
            evidence.categorical.get("route.experiment_assignment_unit"),
            Some(&"task".into())
        );
        assert_eq!(
            evidence.digests.get("route.experiment_assignment_id"),
            Some(&"sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".into())
        );
        assert_eq!(
            evidence
                .structural
                .get("route.experiment_challenger_propensity_ppm"),
            Some(&100_000)
        );
        let guard = batch
            .guard_event
            .ok_or_else(|| anyhow::anyhow!("fixture must trigger the configured progress guard"))?;
        let settlement = settlement_at("episode-1", "request-1", 4, "settled-experiment");
        let envelope =
            build_operational_evaluation(&[start, batch.route_event, guard, settlement.event])?;
        let experiment = envelope.subject.decisions[0]
            .experiment
            .as_ref()
            .ok_or_else(|| {
                anyhow::anyhow!("evaluation must retain producer experiment evidence")
            })?;
        assert_eq!(experiment.experiment_id, POLICY_DIGEST);
        assert_eq!(experiment.arm, ExperimentArm::Challenger);
        assert_eq!(experiment.assignment_unit, ExperimentAssignmentUnit::Task);
        assert_eq!(
            experiment.assignment_id_digest,
            "sha256:abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        );
        assert_eq!(experiment.challenger_propensity_ppm, 100_000);
        Ok(())
    }

    #[test]
    fn replay_rejects_resigned_all_skipped_route_with_wrong_health_digest() -> anyhow::Result<()> {
        let start = guarded_start("request-1", 1)?;
        let mut input = guarded_route_input("route-1", "guard-1");
        input.policy.max_episode_requests = None;
        let mut batch =
            build_guarded_route_batch(&[], &start, HistoryCompleteness::Complete, &input)?;
        assert!(batch.guard_event.is_none());
        assert!(
            batch
                .result
                .intent
                .clauses
                .iter()
                .all(|clause| { clause.disposition == RouteIntentClauseDisposition::Skipped })
        );

        batch.route_event.evidence.digests.insert(
            "route.health_snapshot".into(),
            format!("sha256:{}", "f".repeat(64)),
        );
        batch.route_event.content_digest = batch.route_event.semantic_digest()?;

        assert!(
            reduce(&[start, batch.route_event], &input.policy.protected_tiers).is_err(),
            "a re-signed typed route must remain bound to its factual event prefix"
        );
        Ok(())
    }

    #[test]
    fn replay_rejects_resigned_trigger_and_guard_with_same_wrong_health_digest()
    -> anyhow::Result<()> {
        let start = guarded_start("request-1", 1)?;
        let input = guarded_route_input("route-1", "guard-1");
        let mut batch =
            build_guarded_route_batch(&[], &start, HistoryCompleteness::Complete, &input)?;
        let wrong_digest = format!("sha256:{}", "f".repeat(64));
        batch
            .route_event
            .evidence
            .digests
            .insert("route.health_snapshot".into(), wrong_digest.clone());
        batch.route_event.content_digest = batch.route_event.semantic_digest()?;
        let mut guard = batch
            .guard_event
            .ok_or_else(|| anyhow::anyhow!("test policy must trigger a guard"))?;
        guard
            .evidence
            .digests
            .insert("guard.health_snapshot".into(), wrong_digest);
        guard.content_digest = guard.semantic_digest()?;

        assert!(
            reduce(
                &[start, batch.route_event, guard],
                &input.policy.protected_tiers,
            )
            .is_err(),
            "matching route/guard lies must not substitute for the factual prefix digest"
        );
        Ok(())
    }

    #[tokio::test]
    async fn live_and_offline_replay_share_exact_guarded_prefix_digest() -> anyhow::Result<()> {
        let store = store().await?;
        let input = guarded_route_input("route-1", "guard-1");
        let committed = store
            .correlate_and_begin(
                "owner-a",
                correlate_input("request-1", "episode-1", "start-1", input.clone()),
            )
            .await?
            .guarded_route
            .ok_or_else(|| anyhow::anyhow!("missing guarded route result"))?;
        let events = store.events_for_episode("owner-a", "episode-1").await?;
        let factual_prefix = reduce(&events[..1], &input.policy.protected_tiers)?;
        let persisted_digest = events[1]
            .evidence
            .digests
            .get("route.health_snapshot")
            .ok_or_else(|| anyhow::anyhow!("route lost its health digest"))?;

        assert_eq!(committed.snapshot, factual_prefix);
        assert_eq!(persisted_digest, &factual_prefix.evidence_digest);
        assert_eq!(
            committed.intent.trajectory_snapshot_digest,
            factual_prefix.evidence_digest
        );
        assert_eq!(
            replay_episode(
                &store,
                "owner-a",
                "episode-1",
                &input.policy.protected_tiers
            )
            .await?,
            reduce(&events, &input.policy.protected_tiers)?
        );
        Ok(())
    }

    #[tokio::test]
    async fn new_episode_rejects_invalid_start_evidence_without_writes() -> anyhow::Result<()> {
        for (key, value) in [
            ("history.completeness", "invalid"),
            ("correlation.source", "invalid"),
        ] {
            let store = store().await?;
            let mut input = begin("episode-1", "request-1");
            input
                .event
                .evidence
                .categorical
                .insert(key.into(), value.into());
            input.event.content_digest = input.event.semantic_digest()?;

            assert!(store.begin_request("owner-a", input).await.is_err());
            assert_trajectory_tables_empty(&store).await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn new_episode_rejects_index_event_fact_mismatches_without_writes() -> anyhow::Result<()>
    {
        let mut mismatches = Vec::new();

        let mut completeness = begin("episode-1", "request-1");
        completeness.episode.completeness = HistoryCompleteness::Incomplete;
        mismatches.push(completeness);

        let mut source = begin("episode-1", "request-1");
        source.episode.correlation_source = "canonical_prefix".into();
        mismatches.push(source);

        let mut key = begin("episode-1", "request-1");
        key.full_input_digest = keyed_digest("key-2", "2");
        mismatches.push(key);

        let mut event_key = begin("episode-1", "request-1");
        event_key.event.evidence.digests.insert(
            "correlation.native_parent".into(),
            keyed_digest("key-2", "3").into(),
        );
        event_key.event.content_digest = event_key.event.semantic_digest()?;
        mismatches.push(event_key);

        for input in mismatches {
            let store = store().await?;
            assert!(store.begin_request("owner-a", input).await.is_err());
            assert_trajectory_tables_empty(&store).await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn valid_incomplete_and_unmatched_new_starts_replay() -> anyhow::Result<()> {
        let store = store().await?;
        let mut incomplete = begin("episode-incomplete", "request-incomplete");
        incomplete.episode.completeness = HistoryCompleteness::Incomplete;
        incomplete.episode.correlation_source = "unresolved".into();
        incomplete
            .event
            .evidence
            .categorical
            .insert("history.completeness".into(), "incomplete".into());
        incomplete
            .event
            .evidence
            .categorical
            .insert("correlation.source".into(), "unresolved".into());
        incomplete.event.content_digest = incomplete.event.semantic_digest()?;
        store.begin_request("owner-a", incomplete).await?;

        let mut unmatched = begin("episode-unmatched", "request-unmatched");
        unmatched.episode.completeness = HistoryCompleteness::Unknown;
        unmatched.episode.correlation_source = "unresolved".into();
        unmatched
            .event
            .evidence
            .categorical
            .remove("history.completeness");
        unmatched
            .event
            .evidence
            .categorical
            .remove("correlation.source");
        unmatched.event.content_digest = unmatched.event.semantic_digest()?;
        store.begin_request("owner-a", unmatched).await?;

        let protected_tiers = BTreeSet::new();
        let incomplete =
            replay_episode(&store, "owner-a", "episode-incomplete", &protected_tiers).await?;
        assert_eq!(
            incomplete.health.completeness,
            HistoryCompleteness::Incomplete
        );
        let unmatched =
            replay_episode(&store, "owner-a", "episode-unmatched", &protected_tiers).await?;
        assert_eq!(unmatched.health.completeness, HistoryCompleteness::Unknown);
        Ok(())
    }

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

        store
            .append_route_intent(
                "owner-a",
                route_event("episode-1", "request-1", 2, "event-route-1"),
            )
            .await?;

        let settlement = settlement("episode-1", "request-1", "event-settle-1", "outbox-1");
        store.settle_request("owner-a", settlement.clone()).await?;
        store.settle_request("owner-a", settlement).await?;
        assert_eq!(
            store
                .events_for_episode("owner-a", "episode-1")
                .await?
                .len(),
            3
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
    async fn marking_delivery_twice_preserves_the_first_success_time() -> anyhow::Result<()> {
        let store = store().await?;
        store
            .begin_request("owner-a", begin("episode-1", "request-1"))
            .await?;
        store
            .append_route_intent(
                "owner-a",
                route_event("episode-1", "request-1", 2, "event-route-1"),
            )
            .await?;
        store
            .settle_request(
                "owner-a",
                settlement("episode-1", "request-1", "event-settle-1", "outbox-1"),
            )
            .await?;

        store
            .mark_outbox_delivered("owner-a", "outbox-1", "2026-08-01T00:02:00Z")
            .await?;
        store
            .mark_outbox_delivered("owner-a", "outbox-1", "2026-08-01T00:03:00Z")
            .await?;
        let delivered_at = store
            .db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT delivered_at FROM trajectory_outbox WHERE outbox_id = 'outbox-1'"
                    .to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("delivered outbox row missing"))?
            .try_get::<String>("", "delivered_at")?;

        assert_eq!(delivered_at, "2026-08-01T00:02:00Z");
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
            .append_route_intent(
                "owner-a",
                route_event("episode-1", "request-1", 2, "event-route-1"),
            )
            .await?;
        store
            .append_route_intent(
                "owner-a",
                route_event("episode-2", "request-2", 2, "event-route-2"),
            )
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
            2
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
        first_store
            .append_route_intent(
                "owner-a",
                route_event("episode-1", "request-1", 2, "event-route-1"),
            )
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
    async fn correlation_store_rejects_native_digest_from_another_key() -> anyhow::Result<()> {
        let store = store().await?;
        let input = CorrelateAndBegin {
            request_id: "request-child".into(),
            new_episode_id: "episode-child".into(),
            event_id: "event-child".into(),
            correlation_key_id: "key-1".into(),
            native_parent_id: Some("request-parent".into()),
            native_parent_digest: Some(keyed_digest("key-2", "3")),
            full_input_digest: keyed_digest("key-1", "1"),
            ancestor_prefix_digests: Vec::new(),
            ancestor_prefixes_truncated: false,
            starts_with_prior_turns: false,
            canonical_input_bytes: 1,
            protocol: "responses".into(),
            captured_at: "2026-08-01T00:00:00Z".into(),
            guarded_route: None,
        };

        let error = store
            .correlate_and_begin("owner-a", input)
            .await
            .err()
            .ok_or_else(|| anyhow::anyhow!("native evidence accepted a different key"))?;
        assert!(error.to_string().contains("different correlation key"));
        assert!(store.request("owner-a", "request-child").await?.is_none());
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

    #[tokio::test]
    async fn conditional_head_reservation_rejects_a_stale_sequence() -> anyhow::Result<()> {
        let store = store().await?;
        store
            .begin_request("owner-a", begin("episode-1", "request-1"))
            .await?;

        let first = store.db.begin().await?;
        assert!(
            reserve_episode_head(
                &first,
                "owner-a",
                "episode-1",
                2,
                "request-2",
                "2026-08-01T00:01:00Z",
                HistoryCompleteness::Complete,
            )
            .await?
        );
        first.commit().await?;

        let stale = store.db.begin().await?;
        assert!(
            !reserve_episode_head(
                &stale,
                "owner-a",
                "episode-1",
                2,
                "request-stale",
                "2026-08-01T00:01:01Z",
                HistoryCompleteness::Complete,
            )
            .await?
        );
        stale.rollback().await?;
        Ok(())
    }

    #[tokio::test]
    async fn store_rejects_settlement_before_intent_without_persisting() -> anyhow::Result<()> {
        let store = store().await?;
        store
            .begin_request("owner-a", begin("episode-1", "request-1"))
            .await?;

        assert!(
            store
                .settle_request(
                    "owner-a",
                    settlement_at("episode-1", "request-1", 2, "event-settle-1")
                )
                .await
                .is_err()
        );
        assert_eq!(
            store
                .events_for_episode("owner-a", "episode-1")
                .await?
                .len(),
            1
        );
        assert!(store.pending_outbox("owner-a").await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn store_rejects_invalid_guard_evidence_without_persisting() -> anyhow::Result<()> {
        for (label, hold) in [
            ("missing", None),
            ("zero", Some(0)),
            ("oversized", Some(u64::from(u32::MAX) + 1)),
        ] {
            let store = store().await?;
            store
                .begin_request("owner-a", begin("episode-1", "request-1"))
                .await?;
            store
                .append_route_intent(
                    "owner-a",
                    route_event("episode-1", "request-1", 2, "event-route-1"),
                )
                .await?;

            assert!(
                store
                    .append_guard_activation(
                        "owner-a",
                        guard_event("episode-1", "request-1", 3, label, hold),
                    )
                    .await
                    .is_err(),
                "{label} guard was persisted"
            );
            assert_eq!(
                store
                    .events_for_episode("owner-a", "episode-1")
                    .await?
                    .len(),
                2,
                "{label} guard changed history"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn store_rejects_guard_and_route_phase_violations_without_persisting()
    -> anyhow::Result<()> {
        let pre_intent = store().await?;
        pre_intent
            .begin_request("owner-a", begin("episode-1", "request-1"))
            .await?;
        assert!(
            pre_intent
                .append_guard_activation(
                    "owner-a",
                    guard_event("episode-1", "request-1", 2, "pre", Some(1)),
                )
                .await
                .is_err()
        );
        assert_eq!(
            pre_intent
                .events_for_episode("owner-a", "episode-1")
                .await?
                .len(),
            1
        );

        let duplicate = store().await?;
        duplicate
            .begin_request("owner-a", begin("episode-1", "request-1"))
            .await?;
        duplicate
            .append_route_intent(
                "owner-a",
                route_event("episode-1", "request-1", 2, "event-route-1"),
            )
            .await?;
        duplicate
            .append_guard_activation(
                "owner-a",
                guard_event("episode-1", "request-1", 3, "first", Some(1)),
            )
            .await?;
        assert!(
            duplicate
                .append_guard_activation(
                    "owner-a",
                    guard_event("episode-1", "request-1", 4, "duplicate", Some(1)),
                )
                .await
                .is_err()
        );
        assert_eq!(
            duplicate
                .events_for_episode("owner-a", "episode-1")
                .await?
                .len(),
            3
        );

        let post_settlement = store().await?;
        post_settlement
            .begin_request("owner-a", begin("episode-1", "request-1"))
            .await?;
        post_settlement
            .append_route_intent(
                "owner-a",
                route_event("episode-1", "request-1", 2, "event-route-1"),
            )
            .await?;
        post_settlement
            .settle_request(
                "owner-a",
                settlement_at("episode-1", "request-1", 3, "event-settle-1"),
            )
            .await?;
        assert!(
            post_settlement
                .append_guard_activation(
                    "owner-a",
                    guard_event("episode-1", "request-1", 4, "post", Some(1)),
                )
                .await
                .is_err()
        );
        assert!(
            post_settlement
                .append_route_intent(
                    "owner-a",
                    route_event("episode-1", "request-1", 4, "event-route-post"),
                )
                .await
                .is_err()
        );
        assert_eq!(
            post_settlement
                .events_for_episode("owner-a", "episode-1")
                .await?
                .len(),
            3
        );
        Ok(())
    }

    #[tokio::test]
    async fn store_rejects_timestamp_regression_on_every_append_path() -> anyhow::Result<()> {
        let route_store = store().await?;
        route_store
            .begin_request("owner-a", begin("episode-1", "request-1"))
            .await?;
        let route = with_captured_at(
            route_event("episode-1", "request-1", 2, "event-route-1"),
            "2026-07-31T23:59:59Z",
        )?;
        assert!(
            route_store
                .append_route_intent("owner-a", route)
                .await
                .is_err()
        );
        assert_eq!(
            route_store
                .events_for_episode("owner-a", "episode-1")
                .await?
                .len(),
            1
        );

        let guard_store = store().await?;
        guard_store
            .begin_request("owner-a", begin("episode-1", "request-1"))
            .await?;
        guard_store
            .append_route_intent(
                "owner-a",
                route_event("episode-1", "request-1", 2, "event-route-1"),
            )
            .await?;
        let guard = with_captured_at(
            guard_event("episode-1", "request-1", 3, "guard", Some(1)),
            "2026-07-31T23:59:59Z",
        )?;
        assert!(
            guard_store
                .append_guard_activation("owner-a", guard)
                .await
                .is_err()
        );
        assert_eq!(
            guard_store
                .events_for_episode("owner-a", "episode-1")
                .await?
                .len(),
            2
        );

        let settle_store = store().await?;
        settle_store
            .begin_request("owner-a", begin("episode-1", "request-1"))
            .await?;
        settle_store
            .append_route_intent(
                "owner-a",
                route_event("episode-1", "request-1", 2, "event-route-1"),
            )
            .await?;
        let mut settlement = settlement_at("episode-1", "request-1", 3, "event-settle-1");
        settlement.event = with_captured_at(settlement.event, "2026-07-31T23:59:59Z")?;
        assert!(
            settle_store
                .settle_request("owner-a", settlement)
                .await
                .is_err()
        );
        assert_eq!(
            settle_store
                .events_for_episode("owner-a", "episode-1")
                .await?
                .len(),
            2
        );
        Ok(())
    }

    #[tokio::test]
    async fn route_guard_and_settlement_exact_retries_remain_idempotent() -> anyhow::Result<()> {
        let store = store().await?;
        store
            .begin_request("owner-a", begin("episode-1", "request-1"))
            .await?;
        let route = route_event("episode-1", "request-1", 2, "event-route-1");
        store.append_route_intent("owner-a", route.clone()).await?;
        store.append_route_intent("owner-a", route).await?;
        let guard = guard_event("episode-1", "request-1", 3, "guard", Some(1));
        store
            .append_guard_activation("owner-a", guard.clone())
            .await?;
        store.append_guard_activation("owner-a", guard).await?;
        let settlement = settlement_at("episode-1", "request-1", 4, "event-settle-1");
        store.settle_request("owner-a", settlement.clone()).await?;
        store.settle_request("owner-a", settlement).await?;

        assert_eq!(
            store
                .events_for_episode("owner-a", "episode-1")
                .await?
                .len(),
            4
        );
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_identical_appends_converge_at_every_request_phase() -> anyhow::Result<()> {
        let (store, _directory) = file_store().await?;
        store
            .begin_request("owner-a", begin("episode-1", "request-1"))
            .await?;
        let store = Arc::new(store);

        let barrier = Arc::new(tokio::sync::Barrier::new(17));
        let tasks = (0..16)
            .map(|_| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                tokio::spawn(async move {
                    barrier.wait().await;
                    store
                        .append_route_intent(
                            "owner-a",
                            route_event("episode-1", "request-1", 2, "event-route-1"),
                        )
                        .await
                })
            })
            .collect::<Vec<_>>();
        barrier.wait().await;
        for task in tasks {
            task.await??;
        }

        let barrier = Arc::new(tokio::sync::Barrier::new(17));
        let tasks = (0..16)
            .map(|_| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                tokio::spawn(async move {
                    barrier.wait().await;
                    store
                        .append_guard_activation(
                            "owner-a",
                            guard_event("episode-1", "request-1", 3, "1", Some(1)),
                        )
                        .await
                })
            })
            .collect::<Vec<_>>();
        barrier.wait().await;
        for task in tasks {
            task.await??;
        }

        let barrier = Arc::new(tokio::sync::Barrier::new(17));
        let tasks = (0..16)
            .map(|_| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                tokio::spawn(async move {
                    barrier.wait().await;
                    store
                        .settle_request(
                            "owner-a",
                            settlement_at("episode-1", "request-1", 4, "event-settle-1"),
                        )
                        .await
                })
            })
            .collect::<Vec<_>>();
        barrier.wait().await;
        for task in tasks {
            task.await??;
        }

        assert_eq!(
            store
                .events_for_episode("owner-a", "episode-1")
                .await?
                .len(),
            4
        );
        Ok(())
    }

    #[test]
    fn database_retry_classification_uses_typed_codes_and_constraint_kind() {
        for code in ["40001", "40P01", "1205", "1213", "5", "261", "6"] {
            let classification = classify_database_error(&mock_database_error(code, false));
            assert!(classification.retryable_contention, "code {code}");
            assert!(!classification.unique_violation, "code {code}");
        }

        let arbitrary = classify_database_error(&mock_database_error("23514", false));
        assert!(!arbitrary.retryable_contention);
        assert!(!arbitrary.unique_violation);

        let unique = classify_database_error(&mock_database_error("23505", true));
        assert!(!unique.retryable_contention);
        assert!(unique.unique_violation);

        let english_only = anyhow::Error::new(DbErr::Exec(RuntimeErr::Internal(
            "database is locked; deadlock detected".into(),
        )));
        let classification = classify_database_error(&english_only);
        assert!(!classification.retryable_contention);
        assert!(!classification.unique_violation);

        assert!(contention_backoff(0) < contention_backoff(1));
        assert!(contention_backoff(1) < contention_backoff(5));
        assert_eq!(contention_backoff(5), contention_backoff(31));
    }

    #[derive(Debug)]
    struct MockDatabaseError {
        code: &'static str,
        unique: bool,
    }

    impl fmt::Display for MockDatabaseError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("opaque database failure")
        }
    }

    impl std::error::Error for MockDatabaseError {}

    impl DatabaseError for MockDatabaseError {
        fn message(&self) -> &str {
            "opaque database failure"
        }

        fn code(&self) -> Option<Cow<'_, str>> {
            Some(Cow::Borrowed(self.code))
        }

        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }

        fn kind(&self) -> ErrorKind {
            if self.unique {
                ErrorKind::UniqueViolation
            } else {
                ErrorKind::Other
            }
        }
    }

    fn mock_database_error(code: &'static str, unique: bool) -> anyhow::Error {
        anyhow::Error::new(DbErr::Exec(RuntimeErr::SqlxError(
            sea_orm::SqlxError::Database(Box::new(MockDatabaseError { code, unique })),
        )))
    }

    async fn assert_trajectory_tables_empty(store: &TrajectoryStore) -> anyhow::Result<()> {
        for table in [
            "trajectory_episodes",
            "trajectory_events",
            "trajectory_requests",
            "trajectory_prefix_index",
            "trajectory_outbox",
        ] {
            let row = store
                .db
                .query_one(Statement::from_string(
                    DatabaseBackend::Sqlite,
                    format!("SELECT COUNT(*) AS count FROM {table}"),
                ))
                .await?
                .ok_or_else(|| anyhow::anyhow!("missing {table} count row"))?;
            assert_eq!(row.try_get::<i64>("", "count")?, 0, "table {table}");
        }
        Ok(())
    }

    async fn store() -> anyhow::Result<TrajectoryStore> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        Ok(TrajectoryStore::new(db))
    }

    #[tokio::test]
    async fn pruning_is_owner_safe_bounded_dry_run_exact_and_pending_safe() -> anyhow::Result<()> {
        let store = store().await?;
        let delivered = seed_terminal_episode(
            &store,
            "owner-a",
            "episode-a",
            "request-a",
            "2026-08-01T00:00:00Z",
            true,
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing delivered outbox fixture"))?;
        store
            .mark_outbox_delivered("owner-a", &delivered, "2026-08-01T00:05:00Z")
            .await?;
        seed_terminal_episode(
            &store,
            "owner-b",
            "episode-b",
            "request-b",
            "2026-08-01T00:00:00Z",
            false,
        )
        .await?;
        let pending = seed_terminal_episode(
            &store,
            "owner-c",
            "episode-pending",
            "request-pending",
            "2026-08-01T00:00:00Z",
            true,
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing pending outbox fixture"))?;
        seed_started_episode(
            &store,
            "owner-d",
            "episode-active",
            "request-active",
            "2026-08-01T00:00:00Z",
        )
        .await?;
        seed_started_episode(
            &store,
            "owner-f",
            "episode-closed-active",
            "request-closed-active",
            "2026-08-01T00:00:00Z",
        )
        .await?;
        store
            .db
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "UPDATE trajectory_episodes SET closed_at = '2026-08-01T00:00:00Z' \
                 WHERE owner_user_id = 'owner-f' AND episode_id = 'episode-closed-active'",
            ))
            .await?;
        seed_terminal_episode(
            &store,
            "owner-e",
            "episode-fresh",
            "request-fresh",
            "2026-08-03T00:00:00Z",
            false,
        )
        .await?;

        let dry = store.prune_before("2026-08-02T00:00:00Z", true, 1).await?;
        assert_eq!(
            dry,
            PruneSummary {
                delivered_outbox_rows: 1,
                episode_rows: 2,
                event_rows: 6,
                request_rows: 2,
            }
        );
        assert_eq!(
            store.resolve_episode_owner("episode-a").await?.as_deref(),
            Some("owner-a")
        );

        let deleted = store.prune_before("2026-08-02T00:00:00Z", false, 1).await?;
        assert_eq!(deleted, dry);
        assert!(store.resolve_episode_owner("episode-a").await?.is_none());
        assert!(store.resolve_episode_owner("episode-b").await?.is_none());
        for owner in ["owner-a", "owner-b"] {
            assert_eq!(
                prefix_entity::Entity::find()
                    .filter(prefix_entity::Column::OwnerUserId.eq(owner))
                    .count(&store.db)
                    .await?,
                0,
                "pruning the owner's last request must remove its prefix summary"
            );
        }
        assert_eq!(
            store
                .resolve_episode_owner("episode-pending")
                .await?
                .as_deref(),
            Some("owner-c")
        );
        assert_eq!(
            store
                .resolve_episode_owner("episode-active")
                .await?
                .as_deref(),
            Some("owner-d")
        );
        assert_eq!(
            store
                .resolve_episode_owner("episode-fresh")
                .await?
                .as_deref(),
            Some("owner-e")
        );
        assert_eq!(
            store
                .resolve_episode_owner("episode-closed-active")
                .await?
                .as_deref(),
            Some("owner-f")
        );
        assert_eq!(
            store
                .pending_outbox("owner-c")
                .await?
                .into_iter()
                .map(|row| row.outbox_id)
                .collect::<Vec<_>>(),
            vec![pending]
        );
        assert_eq!(
            store.prune_before("2026-08-02T00:00:00Z", false, 1).await?,
            PruneSummary::default()
        );
        Ok(())
    }

    #[tokio::test]
    async fn pruning_retains_ambiguity_until_the_last_matching_history_is_removed()
    -> anyhow::Result<()> {
        let store = store().await?;
        seed_terminal_episode(
            &store,
            "owner-a",
            "episode-old",
            "request-old",
            "2026-08-01T00:00:00Z",
            false,
        )
        .await?;
        seed_terminal_episode(
            &store,
            "owner-a",
            "episode-fresh",
            "request-fresh",
            "2026-08-03T00:00:00Z",
            false,
        )
        .await?;
        let digest = keyed_digest("key-1", "2");

        let txn = store.db.begin().await?;
        assert!(matches!(
            find_prefix_episode(&txn, "owner-a", std::slice::from_ref(&digest)).await?,
            PrefixResolution::Ambiguous
        ));
        txn.rollback().await?;

        let first = store.prune_before("2026-08-02T00:00:00Z", false, 1).await?;
        assert_eq!(first.episode_rows, 1);
        let txn = store.db.begin().await?;
        assert!(matches!(
            find_prefix_episode(&txn, "owner-a", std::slice::from_ref(&digest)).await?,
            PrefixResolution::Ambiguous
        ));
        txn.rollback().await?;

        let second = store.prune_before("2026-08-04T00:00:00Z", false, 1).await?;
        assert_eq!(second.episode_rows, 1);
        let txn = store.db.begin().await?;
        assert!(matches!(
            find_prefix_episode(&txn, "owner-a", &[digest]).await?,
            PrefixResolution::None
        ));
        txn.rollback().await?;
        Ok(())
    }

    #[tokio::test]
    async fn stale_parent_cas_rolls_back_child_deletes() -> anyhow::Result<()> {
        let store = store().await?;
        seed_terminal_episode(
            &store,
            "owner-a",
            "episode-cas",
            "request-cas",
            "2026-08-01T00:00:00Z",
            false,
        )
        .await?;
        let stale = episode_entity::Entity::find_by_id("episode-cas")
            .one(&store.db)
            .await?
            .ok_or_else(|| anyhow::anyhow!("missing CAS fixture"))?;
        store
            .db
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "UPDATE trajectory_episodes SET next_sequence = 99 \
                 WHERE owner_user_id = 'owner-a' AND episode_id = 'episode-cas'",
            ))
            .await?;

        let txn = store.db.begin().await?;
        let error = delete_prunable_episode_in_tx(&txn, &stale, 1, 3)
            .await
            .expect_err("stale parent head must fail its final CAS");
        assert!(error.to_string().contains("changed during prune"));
        txn.rollback().await?;

        assert!(store.request("owner-a", "request-cas").await?.is_some());
        assert_eq!(
            store
                .events_for_episode("owner-a", "episode-cas")
                .await?
                .len(),
            3
        );
        Ok(())
    }

    #[tokio::test]
    async fn prune_count_mismatch_rolls_back_every_child_delete() -> anyhow::Result<()> {
        let store = store().await?;
        seed_terminal_episode(
            &store,
            "owner-a",
            "episode-count",
            "request-count",
            "2026-08-01T00:00:00Z",
            false,
        )
        .await?;
        let episode = episode_entity::Entity::find_by_id("episode-count")
            .one(&store.db)
            .await?
            .ok_or_else(|| anyhow::anyhow!("missing count fixture"))?;
        let txn = store.db.begin().await?;
        let error = delete_prunable_episode_in_tx(&txn, &episode, 2, 3)
            .await
            .expect_err("pre-read/delete count mismatch must fail closed");
        assert!(error.to_string().contains("changed during prune"));
        txn.rollback().await?;

        assert!(store.request("owner-a", "request-count").await?.is_some());
        assert_eq!(
            store
                .events_for_episode("owner-a", "episode-count")
                .await?
                .len(),
            3
        );
        Ok(())
    }

    #[tokio::test]
    async fn episode_audit_is_owner_scoped_and_names_the_first_corrupt_event() -> anyhow::Result<()>
    {
        let store = store().await?;
        seed_terminal_episode(
            &store,
            "owner-a",
            "episode-audit",
            "request-audit",
            "2026-08-01T00:00:00Z",
            false,
        )
        .await?;

        let metadata = store
            .episode("owner-a", "episode-audit")
            .await?
            .ok_or_else(|| anyhow::anyhow!("missing episode metadata"))?;
        assert_eq!(metadata.correlation_source, "explicit_root");
        assert_eq!(metadata.completeness, HistoryCompleteness::Complete);
        assert!(store.episode("owner-b", "episode-audit").await?.is_none());
        match store.audit_episode("owner-a", "episode-audit").await? {
            EpisodeAudit::Valid {
                snapshot, events, ..
            } => {
                assert_eq!(snapshot.through_sequence, 3);
                assert_eq!(events.len(), 3);
            }
            EpisodeAudit::Corrupt { .. } => anyhow::bail!("valid episode audited as corrupt"),
        }

        store
            .db
            .execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                "UPDATE trajectory_events SET event_json = '{\"invalid\":true}' \
                 WHERE owner_user_id = 'owner-a' AND event_id = 'route-request-audit'",
            ))
            .await?;
        match store.audit_episode("owner-a", "episode-audit").await? {
            EpisodeAudit::Corrupt {
                event_id,
                sequence,
                reason,
                ..
            } => {
                assert_eq!(event_id.as_deref(), Some("route-request-audit"));
                assert_eq!(sequence, Some(2));
                assert_eq!(reason, "stored_event_invalid");
                assert!(!reason.contains("invalid\":true"));
            }
            EpisodeAudit::Valid { .. } => anyhow::bail!("corrupt episode audited as valid"),
        }

        seed_terminal_episode(
            &store,
            "owner-a",
            "episode-secret",
            "request-secret",
            "2026-08-01T00:00:00Z",
            false,
        )
        .await?;
        let mut start = store
            .events_for_episode("owner-a", "episode-secret")
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("missing secret fixture start"))?;
        let sentinel = "private-metadata-SUPER-SECRET-sentinel";
        start
            .evidence
            .categorical
            .insert("correlation.source".into(), sentinel.into());
        start.content_digest = start.semantic_digest()?;
        event_entity::Entity::update_many()
            .col_expr(
                event_entity::Column::EventJson,
                Expr::value(serde_json::to_string(&start)?),
            )
            .col_expr(
                event_entity::Column::ContentDigest,
                Expr::value(start.content_digest.clone()),
            )
            .filter(event_entity::Column::OwnerUserId.eq("owner-a"))
            .filter(event_entity::Column::EventId.eq(&start.event_id))
            .exec(&store.db)
            .await?;
        match store.audit_episode("owner-a", "episode-secret").await? {
            EpisodeAudit::Corrupt { reason, .. } => {
                assert_eq!(reason, "reducer_rejected_prefix");
                assert!(!reason.contains(sentinel));
            }
            EpisodeAudit::Valid { .. } => {
                anyhow::bail!("secret-corrupt episode audited as valid")
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn guarded_audit_attributes_intrinsic_route_corruption_before_guard_corruption()
    -> anyhow::Result<()> {
        async fn guarded_episode(
            store: &TrajectoryStore,
            label: &str,
        ) -> anyhow::Result<Vec<TrajectoryEvent>> {
            let result = store
                .correlate_and_begin(
                    "owner-a",
                    correlate_input(
                        &format!("request-{label}"),
                        &format!("episode-{label}"),
                        &format!("start-{label}"),
                        guarded_route_input(&format!("route-{label}"), &format!("guard-{label}")),
                    ),
                )
                .await?;
            let events = store
                .events_for_episode("owner-a", &result.episode_id)
                .await?;
            assert_eq!(events.len(), 3, "fixture must include route and guard");
            Ok(events)
        }

        async fn replace_event(
            store: &TrajectoryStore,
            event: &TrajectoryEvent,
        ) -> anyhow::Result<()> {
            event_entity::Entity::update_many()
                .col_expr(
                    event_entity::Column::EventJson,
                    Expr::value(serde_json::to_string(event)?),
                )
                .col_expr(
                    event_entity::Column::ContentDigest,
                    Expr::value(event.content_digest.clone()),
                )
                .filter(event_entity::Column::OwnerUserId.eq("owner-a"))
                .filter(event_entity::Column::EventId.eq(&event.event_id))
                .exec(&store.db)
                .await?;
            Ok(())
        }

        async fn assert_first_corrupt(
            store: &TrajectoryStore,
            episode_id: &str,
            expected_event_id: &str,
            expected_sequence: u64,
        ) -> anyhow::Result<()> {
            match store.audit_episode("owner-a", episode_id).await? {
                EpisodeAudit::Corrupt {
                    event_id,
                    sequence,
                    reason,
                    ..
                } => {
                    assert_eq!(event_id.as_deref(), Some(expected_event_id));
                    assert_eq!(sequence, Some(expected_sequence));
                    assert_eq!(reason, "reducer_rejected_prefix");
                }
                EpisodeAudit::Valid { .. } => anyhow::bail!("corrupt fixture audited as valid"),
            }
            Ok(())
        }

        let store = store().await?;

        let mut projection = guarded_episode(&store, "invalid-projection").await?;
        projection[1].evidence.categorical.insert(
            "route.projection".into(),
            "not-a-canonical-projection".into(),
        );
        projection[1].content_digest = projection[1].semantic_digest()?;
        replace_event(&store, &projection[1]).await?;
        assert_first_corrupt(
            &store,
            "episode-invalid-projection",
            "route-invalid-projection",
            2,
        )
        .await?;

        let mut health = guarded_episode(&store, "invalid-health").await?;
        health[1].evidence.digests.insert(
            "route.health_snapshot".into(),
            format!("sha256:{}", "1".repeat(64)),
        );
        health[1].content_digest = health[1].semantic_digest()?;
        replace_event(&store, &health[1]).await?;
        assert_first_corrupt(&store, "episode-invalid-health", "route-invalid-health", 2).await?;

        let mut guard = guarded_episode(&store, "invalid-guard").await?;
        guard[2]
            .evidence
            .structural
            .insert("guard.hold_for_requests".into(), 0);
        guard[2].content_digest = guard[2].semantic_digest()?;
        replace_event(&store, &guard[2]).await?;
        assert_first_corrupt(&store, "episode-invalid-guard", "guard-invalid-guard", 3).await?;
        Ok(())
    }

    #[tokio::test]
    async fn file_sqlite_audit_retries_a_concurrent_append_without_false_corruption()
    -> anyhow::Result<()> {
        let (store, _directory) = file_store().await?;
        store
            .begin_request("owner-a", begin("episode-audit-race", "request-audit-race"))
            .await?;
        let first_head_read = Arc::new(tokio::sync::Barrier::new(2));
        let writer_done = Arc::new(tokio::sync::Barrier::new(2));
        let probe = AuditReadProbe {
            first_read_barriers: Some((first_head_read.clone(), writer_done.clone())),
            force_contention: false,
        };
        let auditing_store = store.clone();
        let audit = tokio::spawn(async move {
            auditing_store
                .audit_episode_with_probe("owner-a", "episode-audit-race", &probe)
                .await
        });

        first_head_read.wait().await;
        store
            .append_route_intent(
                "owner-a",
                route_event(
                    "episode-audit-race",
                    "request-audit-race",
                    2,
                    "route-audit-race",
                ),
            )
            .await?;
        writer_done.wait().await;

        match audit.await?? {
            EpisodeAudit::Valid {
                episode,
                events,
                snapshot,
            } => {
                assert_eq!(episode.next_sequence, 3);
                assert_eq!(events.len(), 2);
                assert_eq!(snapshot.through_sequence, 2);
            }
            EpisodeAudit::Corrupt { reason, .. } => {
                anyhow::bail!("concurrent append was misreported as corrupt: {reason}")
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn audit_contention_exhaustion_is_an_error_not_a_corrupt_report() -> anyhow::Result<()> {
        let (store, _directory) = file_store().await?;
        store
            .begin_request(
                "owner-a",
                begin("episode-audit-contention", "request-audit-contention"),
            )
            .await?;
        let error = store
            .audit_episode_with_probe(
                "owner-a",
                "episode-audit-contention",
                &AuditReadProbe {
                    first_read_barriers: None,
                    force_contention: true,
                },
            )
            .await
            .expect_err("permanent audit contention must not become a corrupt report");
        assert!(error.to_string().contains("audit contention exhausted"));
        Ok(())
    }

    #[tokio::test]
    async fn transactional_settlement_builder_rebases_concurrent_episode_heads()
    -> anyhow::Result<()> {
        let store = store().await?;
        store
            .begin_request("owner-a", begin("episode-head", "request-a"))
            .await?;
        store
            .append_route_intent(
                "owner-a",
                route_event("episode-head", "request-a", 2, "route-a"),
            )
            .await?;

        let mut second = begin("episode-head", "request-b");
        second.event.sequence = 3;
        second.event.event_id = "event-start-request-b".into();
        second.event.content_digest = second.event.semantic_digest()?;
        second.full_input_digest = keyed_digest("key-1", "3");
        store.begin_request("owner-a", second).await?;
        store
            .append_route_intent(
                "owner-a",
                route_event("episode-head", "request-b", 4, "route-b"),
            )
            .await?;

        let left = store.settle_request_from_current_head(
            "owner-a",
            "request-a",
            |_request, _prefix, sequence| {
                Ok(settlement_at(
                    "episode-head",
                    "request-a",
                    sequence,
                    "settlement-a",
                ))
            },
        );
        let right = store.settle_request_from_current_head(
            "owner-a",
            "request-b",
            |_request, _prefix, sequence| {
                Ok(settlement_at(
                    "episode-head",
                    "request-b",
                    sequence,
                    "settlement-b",
                ))
            },
        );
        let (left, right) = tokio::join!(left, right);
        left?;
        right?;

        let events = store.events_for_episode("owner-a", "episode-head").await?;
        let settlement_sequences = events
            .iter()
            .filter(|event| event.kind == TrajectoryEventKind::RequestSettled)
            .map(|event| event.sequence)
            .collect::<BTreeSet<_>>();
        assert_eq!(settlement_sequences, BTreeSet::from([5, 6]));
        Ok(())
    }

    #[tokio::test]
    async fn transactional_settlement_builder_rebuilds_exact_retry_and_rejects_conflict()
    -> anyhow::Result<()> {
        let store = store().await?;
        store
            .begin_request("owner-a", begin("episode-retry", "request-retry"))
            .await?;
        store
            .append_route_intent(
                "owner-a",
                route_event("episode-retry", "request-retry", 2, "route-retry"),
            )
            .await?;
        let exact = |_request: &StoredRequest, _prefix: &[TrajectoryEvent], sequence| {
            Ok(settlement_at(
                "episode-retry",
                "request-retry",
                sequence,
                "settlement-retry",
            ))
        };

        store
            .settle_request_from_current_head("owner-a", "request-retry", exact)
            .await?;
        store
            .settle_request_from_current_head("owner-a", "request-retry", exact)
            .await?;

        let conflict = store
            .settle_request_from_current_head(
                "owner-a",
                "request-retry",
                |_request, _prefix, sequence| {
                    Ok(settlement_at(
                        "episode-retry",
                        "request-retry",
                        sequence,
                        "different-settlement",
                    ))
                },
            )
            .await;
        assert!(
            conflict
                .expect_err("conflicting settlement must fail")
                .to_string()
                .contains("different settlement")
        );
        assert_eq!(
            store
                .events_for_episode("owner-a", "episode-retry")
                .await?
                .len(),
            3
        );
        Ok(())
    }

    async fn file_store() -> anyhow::Result<(TrajectoryStore, tempfile::TempDir)> {
        let directory = tempfile::tempdir()?;
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("trajectory-store.db").display()
        );
        let db = crate::db::connect(&url).await?;
        crate::db::run_migrations(&db).await?;
        Ok((TrajectoryStore::new(db), directory))
    }

    async fn seed_started_episode(
        store: &TrajectoryStore,
        owner: &str,
        episode_id: &str,
        request_id: &str,
        captured_at: &str,
    ) -> anyhow::Result<()> {
        let mut input = begin(episode_id, request_id);
        input.event.owner_user_id = owner.to_owned();
        input.event.captured_at = captured_at.to_owned();
        input.event.content_digest = input.event.semantic_digest()?;
        store.begin_request(owner, input).await?;

        let mut route = route_event(episode_id, request_id, 2, &format!("route-{request_id}"));
        route.owner_user_id = owner.to_owned();
        route.captured_at = captured_at.to_owned();
        route.content_digest = route.semantic_digest()?;
        store.append_route_intent(owner, route).await
    }

    async fn seed_terminal_episode(
        store: &TrajectoryStore,
        owner: &str,
        episode_id: &str,
        request_id: &str,
        captured_at: &str,
        with_outbox: bool,
    ) -> anyhow::Result<Option<String>> {
        seed_started_episode(store, owner, episode_id, request_id, captured_at).await?;
        let mut settlement =
            settlement_at(episode_id, request_id, 3, &format!("settle-{request_id}"));
        settlement.event.owner_user_id = owner.to_owned();
        settlement.event.captured_at = captured_at.to_owned();
        settlement.event.content_digest = settlement.event.semantic_digest()?;
        let outbox_id = with_outbox.then(|| format!("outbox-{request_id}"));
        settlement.outbox = outbox_id.as_ref().map(|outbox_id| OutboxWrite {
            outbox_id: outbox_id.clone(),
            topic: "trajectory.settled".into(),
            payload: OutboxPayload {
                structural: BTreeMap::from([("trajectory.event_count".into(), 3)]),
                digests: BTreeMap::from([(
                    "trajectory.event".into(),
                    settlement.event.content_digest.clone(),
                )]),
                evaluation: None,
            },
            created_at: captured_at.to_owned(),
        });
        store.settle_request(owner, settlement).await?;
        Ok(outbox_id)
    }

    fn begin(episode_id: &str, request_id: &str) -> BeginRequest {
        let mut start = event(
            &format!("event-start-{request_id}"),
            episode_id,
            Some(request_id),
            1,
            TrajectoryEventKind::RequestStarted,
        );
        start.evidence.categorical = BTreeMap::from([
            ("correlation.source".into(), "explicit_root".into()),
            ("history.completeness".into(), "complete".into()),
        ]);
        start.content_digest = start.semantic_digest().unwrap_or_default();
        BeginRequest {
            episode: EpisodeStart {
                episode_id: episode_id.into(),
                correlation_digest: keyed_digest("key-1", "1"),
                correlation_key_id: "key-1".into(),
                correlation_source: "explicit_root".into(),
                completeness: HistoryCompleteness::Complete,
            },
            event: start,
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
                3,
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
                            3,
                            TrajectoryEventKind::RequestSettled,
                        )
                        .content_digest,
                    )]),
                    evaluation: None,
                },
                created_at: "2026-08-01T00:01:00Z".into(),
            }),
        }
    }

    fn settlement_at(
        episode_id: &str,
        request_id: &str,
        sequence: u64,
        event_id: &str,
    ) -> Settlement {
        Settlement {
            event: event(
                event_id,
                episode_id,
                Some(request_id),
                sequence,
                TrajectoryEventKind::RequestSettled,
            ),
            status: RequestStatus::Settled,
            outbox: None,
        }
    }

    fn route_event(
        episode_id: &str,
        request_id: &str,
        sequence: u64,
        event_id: &str,
    ) -> TrajectoryEvent {
        let mut route = event(
            event_id,
            episode_id,
            Some(request_id),
            sequence,
            TrajectoryEventKind::RouteIntentRecorded,
        );
        route.evidence.categorical = BTreeMap::from([
            (
                "route.projection".to_owned(),
                "agent_trace/v2|opening|normal".to_owned(),
            ),
            ("route.selected_tier".to_owned(), "tier-a".to_owned()),
            ("route.workflow_state".to_owned(), "opening".to_owned()),
        ]);
        route.content_digest = route.semantic_digest().unwrap_or_default();
        route
    }

    fn guard_event(
        episode_id: &str,
        request_id: &str,
        sequence: u64,
        suffix: &str,
        hold: Option<u64>,
    ) -> TrajectoryEvent {
        let mut guard = event(
            &format!("event-guard-{suffix}"),
            episode_id,
            Some(request_id),
            sequence,
            TrajectoryEventKind::GuardActivated,
        );
        guard.evidence.structural = hold
            .map(|hold| BTreeMap::from([("guard.hold_for_requests".to_owned(), hold)]))
            .unwrap_or_default();
        guard.content_digest = guard.semantic_digest().unwrap_or_default();
        guard
    }

    fn with_captured_at(
        mut event: TrajectoryEvent,
        captured_at: &str,
    ) -> anyhow::Result<TrajectoryEvent> {
        event.captured_at = captured_at.to_owned();
        event.content_digest = event.semantic_digest()?;
        Ok(event)
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

    fn guarded_route_input(route_event_id: &str, guard_event_id: &str) -> GuardedRouteInput {
        GuardedRouteInput {
            route_event_id: route_event_id.into(),
            guard_event_id: guard_event_id.into(),
            policy_name: "auto:cost".into(),
            request_key: "agent_trace/v2|edit|normal".into(),
            baseline_tier: Some("reference".into()),
            baseline_effort: None,
            tier_efforts: Default::default(),
            preset: Some("auto:cost".into()),
            projection: RouteProjection::parse_key("agent_trace/v2|edit|normal")
                .unwrap_or_else(|| unreachable!()),
            candidate_tier: Some("economy".into()),
            policy_digest: POLICY_DIGEST.into(),
            experiment: None,
            policy: ProgressGuardPolicy {
                escalation_tier: "strong".into(),
                protected_tiers: BTreeSet::from(["strong".into()]),
                max_consecutive_unprotected: None,
                max_same_projection_unprotected: None,
                max_recovery_count: None,
                max_episode_requests: Some(1),
                max_episode_elapsed_ms: None,
                max_episode_cost_micro_usd: None,
                hold_for_requests: 2,
                incomplete_history: IncompleteHistoryAction::Observe,
            },
            carries_tools: false,
            tool_use_tier: Some("strong".into()),
            tool_safe_tiers: BTreeSet::from(["strong".into()]),
        }
    }

    fn correlate_input(
        request_id: &str,
        episode_id: &str,
        event_id: &str,
        guarded_route: GuardedRouteInput,
    ) -> CorrelateAndBegin {
        CorrelateAndBegin {
            request_id: request_id.into(),
            new_episode_id: episode_id.into(),
            event_id: event_id.into(),
            correlation_key_id: "key-1".into(),
            native_parent_id: None,
            native_parent_digest: None,
            full_input_digest: keyed_digest("key-1", "7"),
            ancestor_prefix_digests: Vec::new(),
            ancestor_prefixes_truncated: false,
            starts_with_prior_turns: false,
            canonical_input_bytes: 10,
            protocol: "chat_completions".into(),
            captured_at: "2026-08-01T00:00:00Z".into(),
            guarded_route: Some(guarded_route),
        }
    }

    fn guarded_start(request_id: &str, sequence: u64) -> anyhow::Result<TrajectoryEvent> {
        let mut start = event(
            &format!("start-{request_id}"),
            "episode-1",
            Some(request_id),
            sequence,
            TrajectoryEventKind::RequestStarted,
        );
        start.evidence.categorical = BTreeMap::from([
            ("correlation.source".into(), "canonical_prefix".into()),
            ("history.completeness".into(), "complete".into()),
        ]);
        start
            .evidence
            .structural
            .insert("request.canonical_input_bytes".into(), sequence * 10);
        start.content_digest = start.semantic_digest()?;
        Ok(start)
    }

    fn keyed_digest(key_id: &str, digit: &str) -> KeyedDigest {
        KeyedDigest::parse(format!("hmac-sha256:{key_id}:{}", digit.repeat(64)))
            .unwrap_or_else(|_| unreachable!())
    }
}
