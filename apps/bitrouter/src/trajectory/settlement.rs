//! Durable trajectory settlement bridge.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use bitrouter_sdk::language_model::SettlementContext;
use chrono::{DateTime, SecondsFormat, TimeDelta};

use crate::eval::settlement::PredictionObservationSnapshot;
use crate::metering::MeteringSettlementEvent;

use super::{
    canonical::CorrelationKey,
    evaluation::{TRAJECTORY_EVAL_TOPIC, build_operational_evaluation},
    publisher::TrajectoryOutboxPublisher,
    store::TrajectoryStore,
    types::{
        OutboxPayload, OutboxWrite, RequestStatus, Settlement, TRAJECTORY_SCHEMA_VERSION,
        TrajectoryEvent, TrajectoryEventKind, TrajectoryEvidence, canonical_digest,
    },
};

#[derive(Clone)]
pub(crate) struct TrajectorySettlementRecorder {
    store: TrajectoryStore,
    publisher: TrajectoryOutboxPublisher,
    identity_key: CorrelationKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrajectorySettlementDisposition {
    Untracked,
    AwaitingAuthoritativeMetering,
    Persisted,
    AlreadyTerminal,
}

impl TrajectorySettlementRecorder {
    pub(crate) fn new(
        store: TrajectoryStore,
        publisher: TrajectoryOutboxPublisher,
        identity_key: CorrelationKey,
    ) -> Self {
        Self {
            store,
            publisher,
            identity_key,
        }
    }

    /// Reports the request's closed settlement ownership state. A tracked request
    /// without authoritative metering remains unsettled; absence is never
    /// converted into zero-valued evidence.
    pub(crate) async fn record_if_tracked(
        &self,
        context: &SettlementContext,
        prediction: Option<&PredictionObservationSnapshot>,
    ) -> Result<TrajectorySettlementDisposition> {
        let owner_user_id = context.caller.user_id();
        let request_id = self
            .identity_key
            .request_identity(owner_user_id, &context.request_id)?;
        let Some(request) = self.store.request(owner_user_id, &request_id).await? else {
            return Ok(TrajectorySettlementDisposition::Untracked);
        };
        if request.status != RequestStatus::Started {
            self.store
                .validate_reusable_terminal_settlement(owner_user_id, &request_id)
                .await?;
            self.publisher.kick();
            return Ok(TrajectorySettlementDisposition::AlreadyTerminal);
        }
        let Some(metering) = context.get_event::<MeteringSettlementEvent>().cloned() else {
            return Ok(TrajectorySettlementDisposition::AwaitingAuthoritativeMetering);
        };
        if metering.request_id != context.request_id {
            anyhow::bail!("metering settlement identity does not match trajectory request")
        }
        let owner = owner_user_id.to_owned();
        let settlement = self
            .store
            .settle_request_from_current_head(&owner, &request_id, |request, events, sequence| {
                build_settlement(&owner, request, events, sequence, &metering, prediction)
            })
            .await;
        if let Err(error) = settlement {
            if self
                .store
                .request(owner_user_id, &request_id)
                .await?
                .is_some_and(|request| request.status != RequestStatus::Started)
            {
                self.store
                    .validate_reusable_terminal_settlement(owner_user_id, &request_id)
                    .await?;
                self.publisher.kick();
                return Ok(TrajectorySettlementDisposition::AlreadyTerminal);
            } else {
                return Err(error);
            }
        }
        self.publisher.kick();
        Ok(TrajectorySettlementDisposition::Persisted)
    }
}

fn build_settlement(
    owner_user_id: &str,
    request: &super::types::StoredRequest,
    events: &[TrajectoryEvent],
    sequence: u64,
    metering: &MeteringSettlementEvent,
    prediction: Option<&PredictionObservationSnapshot>,
) -> Result<Settlement> {
    let captured_at = settlement_timestamp(request, events, metering.duration_ms)?;
    let mut structural =
        BTreeMap::from([("settlement.duration_ms".to_owned(), metering.duration_ms)]);
    for (key, value) in [
        ("settlement.prompt_tokens", metering.prompt_tokens),
        ("settlement.completion_tokens", metering.completion_tokens),
        ("settlement.reasoning_tokens", metering.reasoning_tokens),
        ("settlement.cache_read_tokens", metering.cache_read_tokens),
        ("settlement.cache_write_tokens", metering.cache_write_tokens),
        ("settlement.total_tokens", metering.total_tokens),
        ("settlement.cost_micro_usd", metering.cost_micro_usd),
    ] {
        if let Some(value) = value {
            structural.insert(key.to_owned(), value);
        }
    }
    let mut categorical = BTreeMap::from([
        (
            "settlement.usage_origin".to_owned(),
            metering.usage_origin.as_str().to_owned(),
        ),
        (
            "settlement.outcome".to_owned(),
            if metering.error_code.is_some() {
                "failed"
            } else {
                "settled"
            }
            .to_owned(),
        ),
    ]);
    if !metering.provider_id.trim().is_empty() {
        categorical.insert(
            "settlement.provider".to_owned(),
            metering.provider_id.clone(),
        );
    }
    if !metering.model_id.trim().is_empty() {
        categorical.insert("settlement.model".to_owned(), metering.model_id.clone());
    }
    if let Some(error_code) = &metering.error_code {
        categorical.insert("settlement.error_code".to_owned(), error_code.clone());
    }
    if let Some(finish_reason) = &metering.finish_reason {
        categorical.insert("settlement.finish_reason".to_owned(), finish_reason.clone());
    }
    if let Some(prediction) = prediction {
        prediction.write_namespaced(&mut structural, &mut categorical);
    }
    let event_seed = canonical_digest(&(
        owner_user_id,
        request.episode_id.as_str(),
        request.request_id.as_str(),
        sequence,
        "request-settled",
    ))?;
    let mut event = TrajectoryEvent {
        schema_version: TRAJECTORY_SCHEMA_VERSION,
        event_id: format!("trajectory-settlement:{}", digest_hex(&event_seed)?),
        owner_user_id: owner_user_id.to_owned(),
        episode_id: request.episode_id.clone(),
        request_id: Some(request.request_id.clone()),
        sequence,
        kind: TrajectoryEventKind::RequestSettled,
        evidence: TrajectoryEvidence {
            structural,
            categorical,
            digests: BTreeMap::new(),
        },
        captured_at: captured_at.clone(),
        content_digest: String::new(),
    };
    event.content_digest = event.semantic_digest()?;
    let mut candidate = events.to_vec();
    candidate.push(event.clone());
    let envelope = build_operational_evaluation(&candidate)?;
    let outbox_seed = canonical_digest(&(
        owner_user_id,
        event.event_id.as_str(),
        sequence,
        TRAJECTORY_EVAL_TOPIC,
    ))?;
    let outbox = OutboxWrite {
        outbox_id: format!("trajectory-eval:{}", digest_hex(&outbox_seed)?),
        topic: TRAJECTORY_EVAL_TOPIC.to_owned(),
        payload: OutboxPayload {
            structural: BTreeMap::from([("trajectory.through_sequence".to_owned(), sequence)]),
            digests: BTreeMap::from([(
                "trajectory.settlement_event".to_owned(),
                event.content_digest.clone(),
            )]),
            evaluation: Some(Box::new(envelope)),
        },
        created_at: captured_at,
    };
    Ok(Settlement {
        event,
        status: if metering.error_code.is_some() {
            RequestStatus::Failed
        } else {
            RequestStatus::Settled
        },
        outbox: Some(outbox),
    })
}

fn settlement_timestamp(
    request: &super::types::StoredRequest,
    events: &[TrajectoryEvent],
    duration_ms: u64,
) -> Result<String> {
    let start = events
        .iter()
        .find(|event| event.event_id == request.start_event_id)
        .ok_or_else(|| anyhow::anyhow!("trajectory request start event is missing"))?;
    let start_time = DateTime::parse_from_rfc3339(&start.captured_at)?;
    let duration = TimeDelta::milliseconds(
        i64::try_from(duration_ms).context("settlement duration exceeds timestamp range")?,
    );
    let measured = start_time
        .checked_add_signed(duration)
        .ok_or_else(|| anyhow::anyhow!("settlement timestamp exceeds supported range"))?;
    let head = events
        .last()
        .map(|event| DateTime::parse_from_rfc3339(&event.captured_at))
        .transpose()?
        .unwrap_or(start_time);
    Ok(std::cmp::max(measured, head).to_rfc3339_opts(SecondsFormat::AutoSi, true))
}

fn digest_hex(digest: &str) -> Result<&str> {
    digest
        .strip_prefix("sha256:")
        .ok_or_else(|| anyhow::anyhow!("canonical digest has unsupported encoding"))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use bitrouter_sdk::caller::CallerContext;
    use bitrouter_sdk::config::EvalConfig;
    use bitrouter_sdk::event::EventBus;
    use bitrouter_sdk::language_model::{
        ApiProtocol, Content, DataContent, GenerationParams, Message, Prompt, Role,
        SettlementContext, SettlementRecorder, UsageOrigin,
    };
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

    use super::{
        TrajectorySettlementDisposition, TrajectorySettlementRecorder,
        build_operational_evaluation, build_settlement,
    };
    use crate::eval::{
        EvalService,
        admission::SubmissionPrincipal,
        settlement::{
            EvalInvocation, EvalSettlementRecorder, PendingEvalDecision, PendingEvalDecisionStore,
        },
        store::EvalStore,
    };
    use crate::metering::MeteringSettlementEvent;
    use crate::output::reports::trajectory::{inspect_report, replay_report};
    use crate::output::{Format, Output};
    use crate::trajectory::canonical::{Canonicalizer, CorrelationKey};
    use crate::trajectory::correlation::TrajectoryRuntime;
    use crate::trajectory::guard::{IncompleteHistoryAction, ProgressGuardPolicy};
    use crate::trajectory::publisher::TrajectoryOutboxPublisher;
    use crate::trajectory::store::{CorrelateAndBegin, GuardedRouteInput, TrajectoryStore};
    use crate::trajectory::types::{KeyedDigest, RequestStatus, canonical_digest};
    use crate::workflow_state::ir::RouteProjection;
    use crate::workflow_state::response_observer::{ObservedActionClass, PredictionObservation};

    const POLICY_DIGEST: &str =
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[tokio::test]
    async fn recorder_reports_closed_dispositions_and_persists_prediction_observation()
    -> anyhow::Result<()> {
        let (db, store) = store().await?;
        let identity_key = CorrelationKey::from_bytes([41; 32])?;
        let publisher = TrajectoryOutboxPublisher::new(
            store.clone(),
            EvalStore::new(db),
            EvalConfig::default(),
            10,
        )?;
        let recorder =
            TrajectorySettlementRecorder::new(store.clone(), publisher, identity_key.clone());
        assert_eq!(
            recorder
                .record_if_tracked(&context("untracked"), None)
                .await?,
            TrajectorySettlementDisposition::Untracked
        );

        let stored_request = identity_key.request_identity("owner-a", "external-request")?;
        begin_guarded(&store, &stored_request, "episode-observed").await?;
        let mut settlement = context("external-request");
        assert_eq!(
            recorder.record_if_tracked(&settlement, None).await?,
            TrajectorySettlementDisposition::AwaitingAuthoritativeMetering
        );
        settlement.emit(metering("external-request"));
        let observation = pending_decision("external-request").observation_snapshot();
        assert_eq!(
            recorder
                .record_if_tracked(&settlement, Some(&observation))
                .await?,
            TrajectorySettlementDisposition::Persisted
        );
        assert_eq!(
            recorder
                .record_if_tracked(&settlement, Some(&observation))
                .await?,
            TrajectorySettlementDisposition::AlreadyTerminal
        );

        let events = store
            .events_for_episode("owner-a", "episode-observed")
            .await?;
        let terminal = events
            .last()
            .ok_or_else(|| anyhow::anyhow!("terminal event missing"))?;
        assert_eq!(
            terminal
                .evidence
                .categorical
                .get("routing.observed_action")
                .map(String::as_str),
            Some("mutate")
        );
        assert_eq!(
            terminal
                .evidence
                .categorical
                .get("routing.task_family_reason_codes")
                .map(String::as_str),
            Some("task_code_review")
        );
        let evaluation = build_operational_evaluation(&events)?;
        let evidence = evaluation
            .subject
            .evidence
            .iter()
            .find(|evidence| evidence.kind == "routing.prediction_observation")
            .ok_or_else(|| anyhow::anyhow!("prediction observation evidence missing"))?;
        assert_eq!(
            evidence
                .attributes
                .get("observed_action")
                .map(String::as_str),
            Some("mutate")
        );
        assert_eq!(
            evidence
                .attributes
                .get("task_family_reason_codes")
                .map(String::as_str),
            Some("task_code_review")
        );
        Ok(())
    }

    #[tokio::test]
    async fn eval_recorder_uses_exactly_one_owner_and_preserves_pending_on_storage_failure()
    -> anyhow::Result<()> {
        let (db, store) = store().await?;
        let eval_store = EvalStore::new(db.clone());
        let identity_key = CorrelationKey::from_bytes([41; 32])?;
        let stored_request = identity_key.request_identity("owner-a", "external-request")?;
        begin_guarded(&store, &stored_request, "episode-observed").await?;
        let publisher = TrajectoryOutboxPublisher::new(
            store.clone(),
            eval_store.clone(),
            EvalConfig::default(),
            10,
        )?;
        let trajectory =
            TrajectorySettlementRecorder::new(store.clone(), publisher.clone(), identity_key);
        let pending = PendingEvalDecisionStore::default();
        let invocation = EvalInvocation::new("owner-a");
        pending.insert(&invocation, pending_decision("external-request"));
        let recorder = EvalSettlementRecorder::new(
            eval_store.clone(),
            pending.clone(),
            std::sync::Arc::new(crate::metering::PricingTable::new()),
        )
        .with_trajectory(trajectory);
        let mut missing_metering = context("external-request");
        missing_metering.emit(invocation.clone());

        recorder.record(&mut missing_metering).await?;

        assert!(pending.peek(&invocation, "owner-a").is_some());
        assert!(
            eval_store
                .list_subjects_for_owner("owner-a")
                .await?
                .is_empty()
        );
        assert_eq!(
            store
                .request("owner-a", &stored_request)
                .await?
                .map(|request| request.status),
            Some(RequestStatus::Started)
        );
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "CREATE TRIGGER fail_trajectory_outbox BEFORE INSERT ON trajectory_outbox \
             BEGIN SELECT RAISE(FAIL, 'injected trajectory failure'); END"
                .to_owned(),
        ))
        .await?;
        let mut metered = context("external-request");
        metered.emit(invocation.clone());
        metered.emit(metering("external-request"));

        assert!(recorder.record(&mut metered).await.is_err());
        assert!(pending.peek(&invocation, "owner-a").is_some());
        assert_eq!(
            store
                .request("owner-a", &stored_request)
                .await?
                .map(|request| request.status),
            Some(RequestStatus::Started)
        );
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "DROP TRIGGER fail_trajectory_outbox".to_owned(),
        ))
        .await?;

        recorder.record(&mut metered).await?;
        publisher.wait_for_idle().await;

        assert!(pending.peek(&invocation, "owner-a").is_none());
        let subjects = eval_store.list_subjects_for_owner("owner-a").await?;
        assert_eq!(subjects.len(), 1);
        assert_eq!(subjects[0].scope, crate::eval::types::EvalScope::Episode);
        assert!(
            eval_store
                .subject("request:external-request")
                .await?
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn settlement_and_typed_outbox_survive_restart_and_publish() -> anyhow::Result<()> {
        let (db, store) = store().await?;
        begin_guarded(&store, "request-1", "episode-1").await?;
        let metering = metering("request-1");
        store
            .settle_request_from_current_head(
                "owner-a",
                "request-1",
                |request, events, sequence| {
                    build_settlement("owner-a", request, events, sequence, &metering, None)
                },
            )
            .await?;

        let pending = store.pending_outbox("owner-a").await?;
        assert_eq!(pending.len(), 1);
        assert!(pending[0].payload.evaluation.is_some());
        assert_eq!(
            store
                .request("owner-a", "request-1")
                .await?
                .map(|request| request.status),
            Some(RequestStatus::Settled)
        );

        let restarted_store = TrajectoryStore::new(db.clone());
        let eval_store = EvalStore::new(db);
        let publisher = TrajectoryOutboxPublisher::new(
            restarted_store.clone(),
            eval_store.clone(),
            EvalConfig::default(),
            10,
        )?;
        let summary = publisher.drain_pending().await?;
        assert_eq!(summary.delivered, 1);
        assert!(restarted_store.pending_outbox("owner-a").await?.is_empty());
        assert_eq!(
            eval_store.list_subjects_for_owner("owner-a").await?.len(),
            1
        );
        assert_eq!(
            eval_store
                .latest_admissions_for_owner("owner-a")
                .await?
                .len(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn publisher_records_the_successful_admission_time_not_outbox_creation_time()
    -> anyhow::Result<()> {
        let (db, store) = store().await?;
        begin_guarded(&store, "request-delivery-time", "episode-delivery-time").await?;
        let metering = metering("request-delivery-time");
        store
            .settle_request_from_current_head(
                "owner-a",
                "request-delivery-time",
                |request, events, sequence| {
                    build_settlement("owner-a", request, events, sequence, &metering, None)
                },
            )
            .await?;
        let created_at = store.pending_outbox("owner-a").await?[0].created_at.clone();
        let before_admission = chrono::Utc::now();
        let publisher = TrajectoryOutboxPublisher::new(
            store,
            EvalStore::new(db.clone()),
            EvalConfig::default(),
            10,
        )?;

        let summary = publisher.drain_pending().await?;
        let after_admission = chrono::Utc::now();
        let delivered_at = db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT delivered_at FROM trajectory_outbox WHERE owner_user_id = 'owner-a'"
                    .to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("delivered outbox row missing"))?
            .try_get::<String>("", "delivered_at")?;
        let delivered = chrono::DateTime::parse_from_rfc3339(&delivered_at)?;

        assert_eq!(summary.delivered, 1);
        assert_ne!(delivered_at, created_at);
        assert!(delivered >= before_admission.fixed_offset());
        assert!(delivered <= after_admission.fixed_offset());
        Ok(())
    }

    #[tokio::test]
    async fn exact_settlement_retry_is_idempotent_and_changed_authority_conflicts()
    -> anyhow::Result<()> {
        let (_db, store) = store().await?;
        begin_guarded(&store, "request-1", "episode-1").await?;
        let metering = metering("request-1");
        for _ in 0..2 {
            store
                .settle_request_from_current_head(
                    "owner-a",
                    "request-1",
                    |request, events, sequence| {
                        build_settlement("owner-a", request, events, sequence, &metering, None)
                    },
                )
                .await?;
        }
        let mut conflicting = metering;
        conflicting.provider_id = "different-provider".into();
        let error = store
            .settle_request_from_current_head(
                "owner-a",
                "request-1",
                |request, events, sequence| {
                    build_settlement("owner-a", request, events, sequence, &conflicting, None)
                },
            )
            .await
            .expect_err("changed authoritative settlement must conflict");
        assert!(error.to_string().contains("different settlement"));
        assert_eq!(
            store
                .events_for_episode("owner-a", "episode-1")
                .await?
                .len(),
            3
        );
        assert_eq!(store.pending_outbox("owner-a").await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn recorder_retry_after_restart_reuses_first_terminal_settlement() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("trajectory-retry.db").display()
        );
        let identity_key = CorrelationKey::from_bytes([41; 32])?;
        let request_id = identity_key.request_identity("owner-a", "external-request-1")?;

        let db = crate::db::connect(&database_url).await?;
        crate::db::run_migrations(&db).await?;
        let store = TrajectoryStore::new(db.clone());
        begin_guarded(&store, &request_id, "episode-1").await?;
        let publisher = TrajectoryOutboxPublisher::new(
            store.clone(),
            EvalStore::new(db.clone()),
            EvalConfig::default(),
            10,
        )?;
        let recorder = TrajectorySettlementRecorder::new(
            store.clone(),
            publisher.clone(),
            identity_key.clone(),
        );
        let mut first = context("external-request-1");
        first.emit(routing_failure_metering("external-request-1", 100));
        assert_eq!(
            recorder.record_if_tracked(&first, None).await?,
            TrajectorySettlementDisposition::Persisted
        );
        publisher.wait_for_idle().await;
        drop(recorder);
        drop(publisher);
        drop(store);
        drop(db);

        let restarted_db = crate::db::connect(&database_url).await?;
        crate::db::run_migrations(&restarted_db).await?;
        let restarted_store = TrajectoryStore::new(restarted_db.clone());
        let restarted_publisher = TrajectoryOutboxPublisher::new(
            restarted_store.clone(),
            EvalStore::new(restarted_db.clone()),
            EvalConfig::default(),
            10,
        )?;
        let restarted_recorder = TrajectorySettlementRecorder::new(
            restarted_store.clone(),
            restarted_publisher,
            identity_key,
        );
        let mut retry = context("external-request-1");
        retry.emit(routing_failure_metering("external-request-1", 175));

        assert_eq!(
            restarted_recorder.record_if_tracked(&retry, None).await?,
            TrajectorySettlementDisposition::AlreadyTerminal
        );

        let events = restarted_store
            .events_for_episode("owner-a", "episode-1")
            .await?;
        let settlements = events
            .iter()
            .filter(|event| event.kind == super::TrajectoryEventKind::RequestSettled)
            .collect::<Vec<_>>();
        assert_eq!(settlements.len(), 1);
        assert_eq!(
            settlements[0]
                .evidence
                .structural
                .get("settlement.duration_ms"),
            Some(&100)
        );
        let outbox_count: i64 = restarted_db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM trajectory_outbox",
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("trajectory outbox count row missing"))?
            .try_get("", "count")?;
        assert_eq!(outbox_count, 1);
        Ok(())
    }

    #[tokio::test]
    async fn recorder_retry_rejects_corrupt_terminal_event_index() -> anyhow::Result<()> {
        let (db, recorder, request_id, first) = terminal_recorder_fixture().await?;
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE trajectory_requests SET settlement_event_id = ? WHERE request_id = ?",
            ["missing-terminal-event".into(), request_id.into()],
        ))
        .await?;

        let error = recorder
            .record_if_tracked(&first, None)
            .await
            .expect_err("corrupt terminal event index must fail closed");
        assert!(error.to_string().contains("lost its settlement event"));
        Ok(())
    }

    #[tokio::test]
    async fn recorder_retry_rejects_corrupt_terminal_status() -> anyhow::Result<()> {
        let (db, recorder, request_id, first) = terminal_recorder_fixture().await?;
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE trajectory_requests SET status = 'settled' WHERE request_id = ?",
            [request_id.into()],
        ))
        .await?;

        let error = recorder
            .record_if_tracked(&first, None)
            .await
            .expect_err("status that disagrees with terminal evidence must fail closed");
        assert!(error.to_string().contains("inconsistent settlement event"));
        Ok(())
    }

    #[tokio::test]
    async fn recorder_retry_rejects_corrupt_terminal_outbox_index() -> anyhow::Result<()> {
        let (db, recorder, request_id, first) = terminal_recorder_fixture().await?;
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE trajectory_requests SET settlement_outbox_id = ? WHERE request_id = ?",
            ["missing-terminal-outbox".into(), request_id.into()],
        ))
        .await?;

        let error = recorder
            .record_if_tracked(&first, None)
            .await
            .expect_err("corrupt terminal outbox index must fail closed");
        assert!(error.to_string().contains("lost its outbox"));
        Ok(())
    }

    #[tokio::test]
    async fn recorder_retry_rejects_foreign_but_valid_evaluation_envelope() -> anyhow::Result<()> {
        let (db, store) = store().await?;
        let identity_key = CorrelationKey::from_bytes([41; 32])?;
        let request_id = identity_key.request_identity("owner-a", "external-request-current")?;
        begin_guarded(&store, &request_id, "episode-current").await?;
        let current_metering = routing_failure_metering("external-request-current", 100);
        store
            .settle_request_from_current_head(
                "owner-a",
                &request_id,
                |request, events, sequence| {
                    build_settlement(
                        "owner-a",
                        request,
                        events,
                        sequence,
                        &current_metering,
                        None,
                    )
                },
            )
            .await?;
        let current_outbox = store
            .pending_outbox("owner-a")
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("current terminal outbox missing"))?;

        let mut foreign_input = correlate_input("request-foreign", "episode-foreign");
        foreign_input.full_input_digest = keyed_digest('8');
        let foreign_begin = store.correlate_and_begin("owner-a", foreign_input).await?;
        assert_eq!(foreign_begin.episode_id, "episode-foreign");
        let foreign_metering = metering("request-foreign");
        store
            .settle_request_from_current_head(
                "owner-a",
                "request-foreign",
                |request, events, sequence| {
                    build_settlement(
                        "owner-a",
                        request,
                        events,
                        sequence,
                        &foreign_metering,
                        None,
                    )
                },
            )
            .await?;
        let foreign_outbox = store
            .pending_outbox("owner-a")
            .await?
            .into_iter()
            .find(|outbox| {
                outbox
                    .payload
                    .evaluation
                    .as_deref()
                    .is_some_and(|evaluation| evaluation.subject.subject_id == "episode-foreign")
            })
            .ok_or_else(|| anyhow::anyhow!("foreign terminal outbox missing"))?;
        let foreign_evaluation = foreign_outbox
            .payload
            .evaluation
            .as_deref()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("foreign terminal evaluation missing"))?;
        store
            .mark_outbox_delivered(
                "owner-a",
                &foreign_outbox.outbox_id,
                &foreign_outbox.created_at,
            )
            .await?;

        let mut corrupted_payload = current_outbox.payload;
        corrupted_payload.evaluation = Some(Box::new(foreign_evaluation));
        let payload_json = serde_json::to_string(&corrupted_payload)?;
        let payload_digest = canonical_digest(&corrupted_payload)?;
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE trajectory_outbox \
             SET payload_json = ?, payload_digest = ? \
             WHERE owner_user_id = ? AND outbox_id = ?",
            [
                payload_json.into(),
                payload_digest.into(),
                "owner-a".into(),
                current_outbox.outbox_id.into(),
            ],
        ))
        .await?;

        let eval_store = EvalStore::new(db);
        let publisher = TrajectoryOutboxPublisher::new(
            store.clone(),
            eval_store.clone(),
            EvalConfig::default(),
            10,
        )?;
        let recorder = TrajectorySettlementRecorder::new(store, publisher.clone(), identity_key);
        let mut retry = context("external-request-current");
        retry.emit(routing_failure_metering("external-request-current", 175));
        let baseline_subjects = eval_store.list_subjects_for_owner("owner-a").await?.len();
        let baseline_admissions = eval_store
            .latest_admissions_for_owner("owner-a")
            .await?
            .len();

        let result = recorder.record_if_tracked(&retry, None).await;
        publisher.wait_for_idle().await;

        assert!(
            result.is_err(),
            "terminal retry must reject another episode's valid evaluation envelope"
        );
        assert_eq!(
            eval_store.list_subjects_for_owner("owner-a").await?.len(),
            baseline_subjects,
            "rejected reuse must not publish a foreign Eval subject"
        );
        assert_eq!(
            eval_store
                .latest_admissions_for_owner("owner-a")
                .await?
                .len(),
            baseline_admissions,
            "rejected reuse must not publish a foreign Eval result"
        );
        Ok(())
    }

    #[tokio::test]
    async fn tracked_request_without_metering_proof_remains_unsettled() -> anyhow::Result<()> {
        let (db, store) = store().await?;
        let identity_key = CorrelationKey::from_bytes([41; 32])?;
        let request_id = identity_key.request_identity("owner-a", "request-1")?;
        begin_guarded(&store, &request_id, "episode-1").await?;
        let publisher = TrajectoryOutboxPublisher::new(
            store.clone(),
            EvalStore::new(db),
            EvalConfig::default(),
            10,
        )?;
        let recorder = TrajectorySettlementRecorder::new(store.clone(), publisher, identity_key);

        assert_eq!(
            recorder
                .record_if_tracked(&context("request-1"), None)
                .await?,
            TrajectorySettlementDisposition::AwaitingAuthoritativeMetering
        );
        assert_eq!(
            store
                .request("owner-a", &request_id)
                .await?
                .map(|request| request.status),
            Some(RequestStatus::Started)
        );
        assert!(store.pending_outbox("owner-a").await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn unknown_usage_and_price_remain_absent_in_settlement_and_evaluation()
    -> anyhow::Result<()> {
        let (_db, store) = store().await?;
        begin_guarded(&store, "request-1", "episode-1").await?;
        let mut unknown = metering("request-1");
        unknown.usage_origin = UsageOrigin::Unknown;
        unknown.prompt_tokens = None;
        unknown.completion_tokens = None;
        unknown.reasoning_tokens = None;
        unknown.cache_read_tokens = None;
        unknown.cache_write_tokens = None;
        unknown.total_tokens = None;
        unknown.cost_micro_usd = None;
        store
            .settle_request_from_current_head(
                "owner-a",
                "request-1",
                |request, events, sequence| {
                    build_settlement("owner-a", request, events, sequence, &unknown, None)
                },
            )
            .await?;

        let events = store.events_for_episode("owner-a", "episode-1").await?;
        let settlement = events
            .last()
            .ok_or_else(|| anyhow::anyhow!("settlement missing"))?;
        for key in [
            "settlement.prompt_tokens",
            "settlement.completion_tokens",
            "settlement.total_tokens",
            "settlement.cost_micro_usd",
        ] {
            assert!(!settlement.evidence.structural.contains_key(key));
        }
        let envelope = store.pending_outbox("owner-a").await?[0]
            .payload
            .evaluation
            .as_deref()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("typed evaluation missing"))?;
        assert!(
            !envelope
                .result
                .metrics
                .contains_key("trajectory.total_tokens")
        );
        assert!(
            !envelope
                .result
                .metrics
                .contains_key("trajectory.cost.usd_micros")
        );
        assert!(!envelope.result.metrics.contains_key("cost.usd_micros"));
        Ok(())
    }

    #[tokio::test]
    async fn routing_failure_without_target_is_terminal_and_keeps_evidence_absent()
    -> anyhow::Result<()> {
        let (_db, store) = store().await?;
        begin_guarded(&store, "request-1", "episode-1").await?;
        let mut unknown = metering("request-1");
        unknown.provider_id.clear();
        unknown.model_id.clear();
        unknown.usage_origin = UsageOrigin::Unknown;
        unknown.prompt_tokens = None;
        unknown.completion_tokens = None;
        unknown.reasoning_tokens = None;
        unknown.cache_read_tokens = None;
        unknown.cache_write_tokens = None;
        unknown.total_tokens = None;
        unknown.cost_micro_usd = None;
        unknown.error_code = Some("not_found".into());

        store
            .settle_request_from_current_head(
                "owner-a",
                "request-1",
                |request, events, sequence| {
                    build_settlement("owner-a", request, events, sequence, &unknown, None)
                },
            )
            .await?;

        let request = store
            .request("owner-a", "request-1")
            .await?
            .ok_or_else(|| anyhow::anyhow!("settled request missing"))?;
        assert_eq!(request.status, RequestStatus::Failed);
        let events = store.events_for_episode("owner-a", "episode-1").await?;
        let settlement = events
            .last()
            .ok_or_else(|| anyhow::anyhow!("settlement event missing"))?;
        assert!(
            !settlement
                .evidence
                .categorical
                .contains_key("settlement.provider")
        );
        assert!(
            !settlement
                .evidence
                .categorical
                .contains_key("settlement.model")
        );
        assert!(
            !settlement
                .evidence
                .structural
                .contains_key("settlement.total_tokens")
        );
        assert!(
            !settlement
                .evidence
                .structural
                .contains_key("settlement.cost_micro_usd")
        );
        assert_eq!(store.pending_outbox("owner-a").await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn adversarial_input_never_reaches_ledger_outbox_eval_or_operator_reports()
    -> anyhow::Result<()> {
        const SENTINELS: [&str; 8] = [
            "sk-live-API-KEY-SENTINEL",
            "Bearer AUTHORIZATION-SENTINEL",
            "private-prompt-SENTINEL",
            "private-tool-arguments-SENTINEL",
            "private-file-body-SENTINEL",
            "private-provider-metadata-SENTINEL",
            "private-native-parent-SECRET-task-label-SENTINEL",
            "private-request-header-SECRET-task-label-SENTINEL",
        ];
        let (db, store) = store().await?;
        let identity_key = CorrelationKey::from_bytes([41; 32])?;
        let runtime =
            TrajectoryRuntime::new(store.clone(), Canonicalizer::new(identity_key.clone()));
        let first_input = correlate_input("privacy-first", "unused-episode");
        let first_guarded_route = first_input
            .guarded_route
            .ok_or_else(|| anyhow::anyhow!("privacy fixture lost its guarded route"))?;
        let prompt = adversarial_prompt(&SENTINELS);
        let (first, _) = runtime
            .begin_guarded_request(
                "owner-a",
                SENTINELS[6],
                ApiProtocol::Responses,
                &prompt,
                "2026-08-01T00:00:00Z",
                first_guarded_route,
            )
            .await?;
        let mut continuation_prompt = adversarial_prompt(&SENTINELS);
        continuation_prompt.params.extra.insert(
            "previous_response_id".into(),
            serde_json::json!(SENTINELS[6]),
        );
        let second_input = correlate_input("privacy-second", "unused-episode");
        let second_guarded_route = second_input
            .guarded_route
            .ok_or_else(|| anyhow::anyhow!("privacy continuation lost its guarded route"))?;
        let (correlated, _) = runtime
            .begin_guarded_request(
                "owner-a",
                SENTINELS[7],
                ApiProtocol::Responses,
                &continuation_prompt,
                "2026-08-01T00:00:01Z",
                second_guarded_route,
            )
            .await?;
        assert_eq!(correlated.episode_id, first.episode_id);
        assert_eq!(
            correlated.evidence.native_parent_id.as_deref(),
            Some(first.request_id.as_str())
        );
        let eval_store = EvalStore::new(db.clone());
        let publisher = TrajectoryOutboxPublisher::new(
            store.clone(),
            eval_store.clone(),
            EvalConfig::default(),
            10,
        )?;
        let recorder =
            TrajectorySettlementRecorder::new(store.clone(), publisher.clone(), identity_key);
        let mut settlement_context = context(SENTINELS[7]);
        settlement_context.emit(metering(SENTINELS[7]));
        assert_eq!(
            recorder
                .record_if_tracked(&settlement_context, None)
                .await?,
            TrajectorySettlementDisposition::Persisted
        );
        publisher.wait_for_idle().await;

        let mut surfaces = Vec::new();
        surfaces.extend(
            raw_json_column(
                &db,
                "SELECT event_json FROM trajectory_events ORDER BY sequence",
                "event_json",
            )
            .await?,
        );
        surfaces.extend(
            raw_json_column(
                &db,
                "SELECT payload_json FROM trajectory_outbox ORDER BY outbox_id",
                "payload_json",
            )
            .await?,
        );
        for column in ["request_id", "native_parent_id"] {
            surfaces.extend(
                raw_json_column(
                    &db,
                    &format!(
                        "SELECT COALESCE({column}, '') AS value \
                         FROM trajectory_requests ORDER BY request_id"
                    ),
                    "value",
                )
                .await?,
            );
        }
        surfaces.extend(
            raw_json_column(
                &db,
                "SELECT COALESCE(latest_request_id, '') AS value \
                 FROM trajectory_episodes ORDER BY episode_id",
                "value",
            )
            .await?,
        );

        let inspect = inspect_report(&store, &correlated.episode_id).await?;
        let replay = replay_report(&store, &correlated.episode_id).await?;
        for report in [&inspect as &dyn crate::output::CliReport, &replay] {
            surfaces.push(String::from_utf8(
                Output::new(Format::Json).render_to_vec(report),
            )?);
            surfaces.push(String::from_utf8(
                Output::new(Format::Human).render_to_vec(report),
            )?);
        }

        assert!(publisher.drain_pending().await?.failed == 0);
        surfaces.extend(
            raw_json_column(
                &db,
                "SELECT subject_json FROM eval_subjects ORDER BY eval_id",
                "subject_json",
            )
            .await?,
        );
        surfaces.extend(
            raw_json_column(
                &db,
                "SELECT result_json FROM eval_results ORDER BY result_id",
                "result_json",
            )
            .await?,
        );

        assert!(!surfaces.is_empty());
        for surface in &surfaces {
            for sentinel in SENTINELS {
                assert!(
                    !surface.contains(sentinel),
                    "private sentinel escaped into a durable or operator surface"
                );
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn poison_outbox_does_not_block_later_item_or_shutdown_drain() -> anyhow::Result<()> {
        let (db, store) = store().await?;
        begin_guarded(&store, "request-poison", "episode-poison").await?;
        let poison_metering = metering("request-poison");
        store
            .settle_request_from_current_head(
                "owner-a",
                "request-poison",
                |request, events, sequence| {
                    let mut settlement = build_settlement(
                        "owner-a",
                        request,
                        events,
                        sequence,
                        &poison_metering,
                        None,
                    )?;
                    if let Some(outbox) = &mut settlement.outbox {
                        outbox.outbox_id = "aaa-poison".into();
                        outbox.topic = "unsupported.topic".into();
                    }
                    Ok(settlement)
                },
            )
            .await?;
        begin_guarded(&store, "request-valid", "episode-valid").await?;
        let valid_metering = metering("request-valid");
        store
            .settle_request_from_current_head(
                "owner-a",
                "request-valid",
                |request, events, sequence| {
                    let mut settlement = build_settlement(
                        "owner-a",
                        request,
                        events,
                        sequence,
                        &valid_metering,
                        None,
                    )?;
                    if let Some(outbox) = &mut settlement.outbox {
                        outbox.outbox_id = "zzz-valid".into();
                    }
                    Ok(settlement)
                },
            )
            .await?;

        let eval_store = EvalStore::new(db);
        let publisher = TrajectoryOutboxPublisher::new(
            store.clone(),
            eval_store.clone(),
            EvalConfig::default(),
            1,
        )?;
        let summary = publisher.drain_pending().await?;
        assert_eq!(summary.delivered, 1);
        assert!(summary.failed >= 1);
        let pending = store.pending_outbox("owner-a").await?;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].outbox_id, "aaa-poison");
        assert_eq!(pending[0].attempts, 1);
        assert_eq!(
            eval_store.list_subjects_for_owner("owner-a").await?.len(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn retry_after_admission_before_mark_is_idempotent() -> anyhow::Result<()> {
        let (db, store) = store().await?;
        begin_guarded(&store, "request-1", "episode-1").await?;
        let metering = metering("request-1");
        store
            .settle_request_from_current_head(
                "owner-a",
                "request-1",
                |request, events, sequence| {
                    build_settlement("owner-a", request, events, sequence, &metering, None)
                },
            )
            .await?;
        let envelope = store.pending_outbox("owner-a").await?[0]
            .payload
            .evaluation
            .as_deref()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("typed evaluation missing"))?;
        let eval_store = EvalStore::new(db.clone());
        eval_store
            .insert_subject_owned(&envelope.subject, "owner-a")
            .await?;
        EvalService::new(eval_store.clone(), EvalConfig::default())
            .submit(
                envelope.result,
                SubmissionPrincipal::BuiltinTrajectory {
                    owner_user_id: "owner-a".into(),
                },
            )
            .await?;
        assert_eq!(store.pending_outbox("owner-a").await?.len(), 1);

        let restarted = TrajectoryOutboxPublisher::new(
            TrajectoryStore::new(db),
            eval_store,
            EvalConfig::default(),
            10,
        )?;
        let summary = restarted.drain_pending().await?;
        assert_eq!(summary.delivered, 1);
        assert!(store.pending_outbox("owner-a").await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn publisher_batch_is_bounded() -> anyhow::Result<()> {
        let (db, store) = store().await?;
        assert!(
            TrajectoryOutboxPublisher::new(
                store.clone(),
                EvalStore::new(db.clone()),
                EvalConfig::default(),
                0,
            )
            .is_err()
        );
        assert!(
            TrajectoryOutboxPublisher::new(
                store,
                EvalStore::new(db),
                EvalConfig::default(),
                1_001,
            )
            .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn burst_kicks_use_one_single_flight_worker_and_converge() -> anyhow::Result<()> {
        let (db, store) = store().await?;
        for index in 0..24 {
            let request_id = format!("request-{index:02}");
            let episode_id = format!("episode-{index:02}");
            begin_guarded(&store, &request_id, &episode_id).await?;
            let metering = metering(&request_id);
            store
                .settle_request_from_current_head(
                    "owner-a",
                    &request_id,
                    |request, events, sequence| {
                        build_settlement("owner-a", request, events, sequence, &metering, None)
                    },
                )
                .await?;
        }
        let publisher = TrajectoryOutboxPublisher::new(
            store.clone(),
            EvalStore::new(db),
            EvalConfig::default(),
            3,
        )?;

        for _ in 0..1_000 {
            publisher.kick();
        }
        publisher.wait_for_idle().await;

        assert!(store.pending_outbox("owner-a").await?.is_empty());
        assert_eq!(publisher.worker_stats(), (1, 1));
        Ok(())
    }

    async fn store() -> anyhow::Result<(sea_orm::DatabaseConnection, TrajectoryStore)> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        Ok((db.clone(), TrajectoryStore::new(db)))
    }

    async fn terminal_recorder_fixture() -> anyhow::Result<(
        sea_orm::DatabaseConnection,
        TrajectorySettlementRecorder,
        String,
        SettlementContext,
    )> {
        let (db, store) = store().await?;
        let identity_key = CorrelationKey::from_bytes([41; 32])?;
        let request_id = identity_key.request_identity("owner-a", "external-request-1")?;
        begin_guarded(&store, &request_id, "episode-1").await?;
        let publisher = TrajectoryOutboxPublisher::new(
            store.clone(),
            EvalStore::new(db.clone()),
            EvalConfig::default(),
            10,
        )?;
        let recorder = TrajectorySettlementRecorder::new(store, publisher, identity_key);
        let mut first = context("external-request-1");
        first.emit(routing_failure_metering("external-request-1", 100));
        assert_eq!(
            recorder.record_if_tracked(&first, None).await?,
            TrajectorySettlementDisposition::Persisted
        );
        Ok((db, recorder, request_id, first))
    }

    async fn raw_json_column(
        db: &sea_orm::DatabaseConnection,
        query: &str,
        column: &str,
    ) -> anyhow::Result<Vec<String>> {
        db.query_all(Statement::from_string(DatabaseBackend::Sqlite, query))
            .await?
            .into_iter()
            .map(|row| row.try_get("", column).map_err(Into::into))
            .collect()
    }

    fn adversarial_prompt(sentinels: &[&str; 8]) -> Prompt {
        let mut params = GenerationParams {
            extra_protocol: Some(ApiProtocol::ChatCompletions),
            ..GenerationParams::default()
        };
        params
            .extra
            .insert("api_key".into(), serde_json::json!(sentinels[0]));
        params
            .extra
            .insert("authorization".into(), serde_json::json!(sentinels[1]));
        Prompt {
            model: "inbound-private-model".into(),
            system: Some(sentinels[2].into()),
            system_provider_metadata: BTreeMap::from([(
                "provider".into(),
                serde_json::json!({"private": sentinels[5]}),
            )]),
            messages: vec![Message {
                role: Role::User,
                content: vec![
                    Content::Text {
                        text: sentinels[2].into(),
                        provider_metadata: BTreeMap::new(),
                    },
                    Content::ToolCall {
                        id: "call-private".into(),
                        name: "private-tool".into(),
                        arguments: format!("{{\"value\":\"{}\"}}", sentinels[3]),
                        provider_executed: false,
                        dynamic: false,
                        provider_metadata: BTreeMap::from([(
                            "provider".into(),
                            serde_json::json!({"private": sentinels[5]}),
                        )]),
                    },
                    Content::File {
                        media_type: "application/octet-stream".into(),
                        data: DataContent::Base64 {
                            data: sentinels[4].into(),
                        },
                        filename: Some("private.bin".into()),
                        provider_metadata: BTreeMap::from([(
                            "provider".into(),
                            serde_json::json!({"private": sentinels[5]}),
                        )]),
                    },
                ],
            }],
            tools: Vec::new(),
            params,
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    async fn begin_guarded(
        store: &TrajectoryStore,
        request_id: &str,
        episode_id: &str,
    ) -> anyhow::Result<()> {
        let result = store
            .correlate_and_begin("owner-a", correlate_input(request_id, episode_id))
            .await?;
        assert!(result.guarded_route.is_some());
        Ok(())
    }

    fn correlate_input(request_id: &str, episode_id: &str) -> CorrelateAndBegin {
        CorrelateAndBegin {
            request_id: request_id.into(),
            new_episode_id: episode_id.into(),
            event_id: format!("start-{request_id}"),
            correlation_key_id: "key-1".into(),
            native_parent_id: None,
            native_parent_digest: None,
            full_input_digest: keyed_digest('7'),
            ancestor_prefix_digests: Vec::new(),
            ancestor_prefixes_truncated: false,
            starts_with_prior_turns: false,
            canonical_input_bytes: 10,
            protocol: "chat_completions".into(),
            captured_at: "2026-08-01T00:00:00Z".into(),
            guarded_route: Some(GuardedRouteInput {
                route_event_id: format!("route-{request_id}"),
                guard_event_id: format!("guard-{request_id}"),
                policy_name: "auto:cost".into(),
                route_projection: "agent_route/v2|code:generation|implement|normal".into(),
                request_key: "agent_trace/v2|edit|normal".into(),
                baseline_tier: Some("reference".into()),
                baseline_effort: None,
                predictive_v1_fallback_tier: Some("reference".into()),
                tier_efforts: Default::default(),
                preset: Some("auto:cost".into()),
                projection: RouteProjection::parse_key("agent_trace/v2|edit|normal")
                    .unwrap_or_else(|| unreachable!()),
                candidate_tier: Some("economy".into()),
                policy_digest: POLICY_DIGEST.into(),
                policy: ProgressGuardPolicy {
                    escalation_tier: "strong".into(),
                    protected_tiers: BTreeSet::from(["strong".into()]),
                    max_consecutive_unprotected: None,
                    max_same_projection_unprotected: None,
                    max_recovery_count: None,
                    max_episode_requests: None,
                    max_episode_elapsed_ms: None,
                    max_episode_cost_micro_usd: None,
                    hold_for_requests: 2,
                    incomplete_history: IncompleteHistoryAction::Observe,
                },
                carries_tools: false,
                tool_use_tier: Some("strong".into()),
                tool_safe_tiers: BTreeSet::from(["strong".into()]),
            }),
        }
    }

    fn keyed_digest(digit: char) -> KeyedDigest {
        KeyedDigest::parse(format!(
            "hmac-sha256:key-1:{}",
            digit.to_string().repeat(64)
        ))
        .unwrap_or_else(|_| unreachable!())
    }

    fn metering(request_id: &str) -> MeteringSettlementEvent {
        MeteringSettlementEvent {
            request_id: request_id.into(),
            provider_id: "provider".into(),
            model_id: "model".into(),
            usage_origin: UsageOrigin::ProviderReported,
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            reasoning_tokens: Some(0),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            total_tokens: Some(15),
            cost_micro_usd: Some(70),
            duration_ms: 100,
            error_code: None,
            finish_reason: Some("stop".into()),
        }
    }

    fn routing_failure_metering(request_id: &str, duration_ms: u64) -> MeteringSettlementEvent {
        MeteringSettlementEvent {
            request_id: request_id.into(),
            provider_id: String::new(),
            model_id: String::new(),
            usage_origin: UsageOrigin::Unknown,
            prompt_tokens: None,
            completion_tokens: None,
            reasoning_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            total_tokens: None,
            cost_micro_usd: None,
            duration_ms,
            error_code: Some("not_found".into()),
            finish_reason: None,
        }
    }

    fn pending_decision(request_id: &str) -> PendingEvalDecision {
        PendingEvalDecision {
            request_id: request_id.into(),
            decision_id: format!("decision-{request_id}"),
            policy: "auto:cost".into(),
            policy_digest: POLICY_DIGEST.into(),
            route_projection: "agent_trace/v2|edit|normal".into(),
            request_key: "agent_trace/v2|edit|normal".into(),
            selected_tier: "economy".into(),
            selected_effort: None,
            baseline_tier: Some("strong".into()),
            baseline_effort: None,
            predictive_v1_fallback_tier: None,
            preset: Some("auto:cost".into()),
            holdout: false,
            continuation_proposed_tier: None,
            continuation_proposed_model: None,
            continuation_proposed_effort: None,
            continuation_adjustment: None,
            predicted_role: Some("implement".into()),
            predicted_task_family: Some("code:review".into()),
            predicted_action: Some("mutate".into()),
            prediction_confidence_ppm: Some(900_000),
            task_family_confidence_ppm: Some(800_000),
            task_family_reason_codes: vec![
                "task_code_review".into(),
                "customer_secret".into(),
                "task_code_review".into(),
            ],
            predictor_contract_digest: Some(
                "sha256:7483fb5fa02c0141f568b82287234895c666fef426789e32783bdd3a00cea3ec".into(),
            ),
            prediction_confidence_kind: Some("heuristic_margin".into()),
            observation: Some(PredictionObservation::new(ObservedActionClass::Mutate)),
            observed_at: "2026-08-08T00:00:00Z".into(),
        }
    }

    fn context(request_id: &str) -> SettlementContext {
        SettlementContext {
            request_id: request_id.into(),
            caller: CallerContext::new("key-a", "owner-a"),
            target: None,
            model_id: "model".into(),
            reasoning_effort: None,
            provider_id: "provider".into(),
            account_label: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            reasoning_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            usage_origin: UsageOrigin::Unknown,
            raw_usage: None,
            web_search_count: 0,
            media_input_count: 0,
            media_output_count: 0,
            server_tool_calls: Vec::new(),
            streamed: false,
            request_duration_ms: 100,
            upstream_duration_ms: None,
            ttft_ms: None,
            generation_duration_ms: None,
            first_token_kind: None,
            finish_reason: None,
            error: None,
            events: EventBus::default(),
        }
    }
}
