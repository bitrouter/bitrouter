//! Namespace, rollback, exact retry and bounded history using real PostgreSQL.

use std::{collections::BTreeMap, time::Duration};

use anyhow::{Context, Result, ensure};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, IsolationLevel, Statement};

use super::super::{
    schema,
    store::{self, Kind, NewRecord, Store},
};
use crate::trajectory::{
    shared::{MAX_HISTORY_BYTES, MAX_HISTORY_EVENTS, PrefixMatch, SharedTrajectory},
    store::TrajectoryStore,
    types::{
        BeginRequest, EpisodeStart, HistoryCompleteness, KeyedDigest, OutboxPayload, OutboxWrite,
        RequestStatus, Settlement, StoredRequest, TRAJECTORY_SCHEMA_VERSION, TrajectoryEvent,
        TrajectoryEventKind, TrajectoryEvidence,
    },
};

const OWNER: &str = "trajectory-owner";

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL for a dedicated PostgreSQL test database"]
async fn postgres_shared_trajectory() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), exercise())
        .await
        .context("shared trajectory regression deadline exceeded")?
}

async fn exercise() -> Result<()> {
    let url = std::env::var("BITROUTER_TEST_DATABASE_URL")?;
    let db = store::connect(&url, 3, Duration::from_secs(3)).await?;
    let other_db = store::connect(&url, 3, Duration::from_secs(3)).await?;
    let namespace = format!("trajectory-{}", uuid::Uuid::new_v4());
    let first = Store::new(db.clone(), &namespace, Duration::from_secs(3))?;
    let second = Store::new(other_db.clone(), &namespace, Duration::from_secs(3))?;
    let result = run(&first, &second, &db).await;
    let closed = db.close().await;
    let other_closed = other_db.close().await;
    result?;
    closed?;
    other_closed?;
    Ok(())
}

async fn run(first: &Store, second: &Store, db: &DatabaseConnection) -> Result<()> {
    schema::initialize_all(first).await?;
    let tx = first.begin().await?;
    first
        .create(
            &tx,
            NewRecord {
                kind: Kind::EpisodeFence,
                key: "fixture-fence",
                status: "ready",
                due_at_ms: None,
                now_ms: 0,
                value: &0u64,
            },
        )
        .await?;
    tx.commit().await?;
    let shared = SharedTrajectory::new(first.namespace())?;
    let other = SharedTrajectory::new(&format!("trajectory-{}", uuid::Uuid::new_v4()))?;
    let request_id = format!("request-{}", uuid::Uuid::new_v4());
    let input = start(&format!("episode-{}", uuid::Uuid::new_v4()), &request_id)?;
    let id = &input.episode.episode_id;
    let local = TrajectoryStore::new(db.clone());
    // Identical natural IDs in legacy tables and two deployment namespaces.
    local.begin_request(OWNER, input.clone()).await?;
    let tx = first.begin().await?;
    shared.begin_in(&tx, OWNER, &input).await?;
    tx.rollback().await?;
    let tx = second.begin().await?;
    ensure!(shared.request_in(&tx, OWNER, &request_id).await?.is_none());
    ensure!(shared.episode_in(&tx, OWNER, id).await?.is_none());
    ensure!(
        shared
            .prefix_in(&tx, OWNER, &input.full_input_digest)
            .await?
            == PrefixMatch::Missing
    );
    tx.commit().await?;
    let tx = first
        .begin_with_isolation(IsolationLevel::ReadCommitted)
        .await?;
    ensure!(shared.begin_in(&tx, OWNER, &input).await.is_err());
    tx.rollback().await?;
    // Concurrent callers retry the whole transaction, never a partial mutation.
    let (a, b) = tokio::join!(
        begin_retry(first, &shared, &input),
        begin_retry(second, &shared, &input)
    );
    a?;
    b?;
    let tx = second.begin().await?;
    shared.begin_in(&tx, OWNER, &input).await?;
    other.begin_in(&tx, OWNER, &input).await?;
    ensure!(shared.history_in(&tx, OWNER, id).await? == vec![input.event.clone()]);
    ensure!(
        shared
            .prefix_in(&tx, OWNER, &input.full_input_digest)
            .await?
            == PrefixMatch::Unique(id.clone())
    );
    ensure!(
        shared
            .request_in(&tx, "foreign", &request_id)
            .await?
            .is_none()
    );
    ensure!(shared.episode_in(&tx, "foreign", id).await?.is_none());
    let route = route_event(id, &request_id, 2)?;
    shared
        .append_route_in(&tx, OWNER, std::slice::from_ref(&route))
        .await?;
    tx.commit().await?;
    local.append_route_intent(OWNER, route).await?;

    let tx = first.begin().await?;
    let rolled_back = shared
        .settle_in(&tx, OWNER, &request_id, settlement)
        .await?;
    let outbox_id = &rolled_back
        .outbox
        .as_ref()
        .context("missing fixture outbox")?
        .outbox_id;
    ensure!(shared.outbox_in(&tx, OWNER, outbox_id).await?.is_some());
    tx.rollback().await?;
    let tx = second.begin().await?;
    ensure!(
        shared
            .request_in(&tx, OWNER, &request_id)
            .await?
            .context("missing request")?
            .status
            == RequestStatus::Started
    );
    ensure!(shared.outbox_in(&tx, OWNER, outbox_id).await?.is_none());
    tx.commit().await?;
    let (a, b) = tokio::join!(
        settle_retry(first, &shared, &request_id),
        settle_retry(second, &shared, &request_id)
    );
    ensure!(a? == rolled_back && b? == rolled_back);
    local.settle_request(OWNER, rolled_back.clone()).await?;

    let tx = second.begin().await?;
    let history = shared.history_in(&tx, OWNER, id).await?;
    ensure!(history == local.events_for_episode(OWNER, id).await?);
    ensure!(
        shared.request_in(&tx, OWNER, &request_id).await?
            == local.request(OWNER, &request_id).await?
    );
    ensure!(shared.episode_in(&tx, OWNER, id).await? == local.episode(OWNER, id).await?);
    ensure!(other.history_in(&tx, OWNER, id).await? == vec![input.event.clone()]);
    ensure!(other.outbox_in(&tx, OWNER, outbox_id).await?.is_none());
    ensure!(shared.outbox_in(&tx, "foreign", outbox_id).await?.is_none());
    tx.commit().await?;

    // Append a later request; exact settlement replay must use the old prefix.
    let mut later = start(id, "later")?;
    later.event.sequence = 4;
    later.event.content_digest = later.event.semantic_digest()?;
    let tx = first.begin().await?;
    shared.begin_in(&tx, OWNER, &later).await?;
    ensure!(
        shared
            .settle_in(&tx, OWNER, &request_id, settlement)
            .await?
            == rolled_back
    );
    tx.commit().await?;
    let tx = first.begin().await?;
    ensure!(
        shared
            .settle_in(&tx, OWNER, &request_id, |r, h, s| {
                let mut changed = settlement(r, h, s)?;
                changed.status = RequestStatus::Failed;
                Ok(changed)
            })
            .await
            .is_err()
    );
    tx.rollback().await?;
    let tx = first.begin().await?;
    ensure!(
        shared
            .settle_in(&tx, OWNER, &request_id, |r, h, s| {
                let mut changed = settlement(r, h, s)?;
                changed.outbox.as_mut().context("missing outbox")?.topic = "different".into();
                Ok(changed)
            })
            .await
            .is_err()
    );
    tx.rollback().await?;
    // Raw start replay after settlement is deliberately not guarded retry.
    let tx = first.begin().await?;
    ensure!(shared.begin_in(&tx, OWNER, &input).await.is_err());
    tx.rollback().await?;
    let tx = second.begin().await?;
    let before = shared.history_in(&tx, OWNER, id).await?;
    let route = route_event(id, &request_id, 2)?;
    shared
        .append_route_in(&tx, OWNER, std::slice::from_ref(&route))
        .await?;
    ensure!(shared.history_in(&tx, OWNER, id).await? == before);
    tx.commit().await?;
    let tx = first.begin().await?;
    let mut changed = route;
    changed
        .evidence
        .categorical
        .insert("route.selected_tier".into(), "changed".into());
    changed.content_digest = changed.semantic_digest()?;
    ensure!(
        shared
            .append_route_in(&tx, OWNER, &[changed])
            .await
            .is_err()
    );
    tx.rollback().await?;
    // Delivery bookkeeping is mutable; exact terminal replay must preserve it.
    let tx = first.begin().await?;
    tx.execute(Statement::from_sql_and_values(DatabaseBackend::Postgres,
        "UPDATE gateway_shared_trajectory_outbox SET attempts=3,delivered_at=$3 WHERE namespace=$1 AND outbox_id=$2",
        [first.namespace().into(), outbox_id.clone().into(), "2026-10-21T00:01:00Z".into()])).await?;
    ensure!(
        shared
            .settle_in(&tx, OWNER, &request_id, settlement)
            .await?
            == rolled_back
    );
    ensure!(
        shared
            .outbox_in(&tx, OWNER, outbox_id)
            .await?
            .context("missing delivered outbox")?
            .attempts
            == 3
    );
    let row = tx.query_one(Statement::from_sql_and_values(DatabaseBackend::Postgres,
        "SELECT delivered_at FROM gateway_shared_trajectory_outbox WHERE namespace=$1 AND outbox_id=$2",
        [first.namespace().into(), outbox_id.clone().into()])).await?.context("missing outbox")?;
    ensure!(
        row.try_get::<Option<String>>("", "delivered_at")?
            .as_deref()
            == Some("2026-10-21T00:01:00Z")
    );
    // The other namespace can own the identical outbox identity independently.
    other
        .append_route_in(&tx, OWNER, &[route_event(id, &request_id, 2)?])
        .await?;
    other
        .settle_in(&tx, OWNER, &request_id, |r, h, s| {
            let mut result = settlement(r, h, s)?;
            result.status = RequestStatus::Failed;
            result.outbox.as_mut().context("missing outbox")?.topic = "other.namespace".into();
            Ok(result)
        })
        .await?;
    ensure!(
        shared
            .outbox_in(&tx, OWNER, outbox_id)
            .await?
            .context("missing outbox")?
            .topic
            == "trajectory.settled"
    );
    ensure!(
        other
            .outbox_in(&tx, OWNER, outbox_id)
            .await?
            .context("missing other outbox")?
            .topic
            == "other.namespace"
    );
    tx.commit().await?;
    late_outbox_failure(first, &shared, outbox_id).await?;
    conflicts(first, &shared, &input).await?;
    limits(first, &shared, id, outbox_id).await?;
    guard_batches(first, second, &shared).await?;
    count_boundary(first, &shared).await?;
    Ok(())
}

async fn begin_retry(store: &Store, shared: &SharedTrajectory, input: &BeginRequest) -> Result<()> {
    store
        .retry(|| async {
            let tx = store.begin().await?;
            advance_fixture_fence(store, &tx).await?;
            shared.begin_in(&tx, OWNER, input).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
}

async fn settle_retry(store: &Store, shared: &SharedTrajectory, id: &str) -> Result<Settlement> {
    store
        .retry(|| async {
            let tx = store.begin().await?;
            advance_fixture_fence(store, &tx).await?;
            let settlement = shared.settle_in(&tx, OWNER, id, settlement).await?;
            tx.commit().await?;
            Ok(settlement)
        })
        .await
}

// The domain layer requires a caller-held episode fence. A versioned fixture
// row models that outer authority boundary and forces stale snapshots to retry.
async fn advance_fixture_fence(store: &Store, tx: &sea_orm::DatabaseTransaction) -> Result<()> {
    let mut fence = store
        .lock::<u64>(tx, Kind::EpisodeFence, "fixture-fence")
        .await?;
    fence.value = fence
        .value
        .checked_add(1)
        .context("fixture fence overflow")?;
    store.save(tx, &mut fence, "ready", None, 0).await
}

async fn conflicts(store: &Store, shared: &SharedTrajectory, input: &BeginRequest) -> Result<()> {
    let mut foreign = start("foreign-episode", "foreign-request")?;
    foreign.native_parent_id = input.event.request_id.clone();
    foreign.event.owner_user_id = "foreign".into();
    foreign.event.content_digest = foreign.event.semantic_digest()?;
    let tx = store.begin().await?;
    ensure!(shared.begin_in(&tx, "foreign", &foreign).await.is_err());
    tx.rollback().await?;
    foreign.native_parent_id = None;
    foreign.event.request_id = input.event.request_id.clone();
    foreign.event.content_digest = foreign.event.semantic_digest()?;
    let tx = store.begin().await?;
    ensure!(shared.begin_in(&tx, "foreign", &foreign).await.is_err());
    tx.rollback().await?;
    let tx = store.begin().await?;
    ensure!(
        shared
            .episode_in(&tx, "foreign", "foreign-episode")
            .await?
            .is_none()
    );
    tx.commit().await?;
    // Same owner and digest in another episode makes the prefix permanently ambiguous.
    let another = start("second-episode", "second-request")?;
    let tx = store.begin().await?;
    shared.begin_in(&tx, OWNER, &another).await?;
    ensure!(
        shared
            .prefix_in(&tx, OWNER, &input.full_input_digest)
            .await?
            == PrefixMatch::Ambiguous
    );
    shared.begin_in(&tx, OWNER, &another).await?;
    ensure!(
        shared
            .prefix_in(&tx, OWNER, &input.full_input_digest)
            .await?
            == PrefixMatch::Ambiguous
    );
    tx.commit().await?;
    Ok(())
}

async fn limits(
    store: &Store,
    shared: &SharedTrajectory,
    episode: &str,
    outbox: &str,
) -> Result<()> {
    let tx = store.begin().await?;
    tx.execute(Statement::from_sql_and_values(DatabaseBackend::Postgres,
        "UPDATE gateway_shared_trajectory_events SET event_json=' ' || event_json WHERE namespace=$1 AND episode_id=$2 AND sequence=1",
        [store.namespace().into(), episode.into()])).await?;
    let error = shared
        .history_in(&tx, OWNER, episode)
        .await
        .err()
        .context("padded history accepted")?;
    ensure!(
        error
            .to_string()
            .contains("noncanonical shared trajectory event")
    );
    tx.rollback().await?;
    // Deliberately corrupt only this namespace inside rolled-back transactions.
    // Size/count rejection happens before transferring or decoding event JSON.
    let tx = store.begin().await?;
    tx.execute(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "UPDATE gateway_shared_trajectory_events SET event_json=repeat('x',$4)
         WHERE namespace=$1 AND owner_user_id=$2 AND episode_id=$3 AND sequence=1",
        vec![
            store.namespace().into(),
            OWNER.into(),
            episode.into(),
            i32::try_from(MAX_HISTORY_BYTES + 1)?.into(),
        ],
    ))
    .await?;
    let error = shared
        .history_in(&tx, OWNER, episode)
        .await
        .err()
        .context("oversize history accepted")?;
    ensure!(error.to_string().contains("history byte limit"));
    tx.rollback().await?;
    let tx = store.begin().await?;
    tx.execute(Statement::from_sql_and_values(DatabaseBackend::Postgres,
        "INSERT INTO gateway_shared_trajectory_events (namespace,event_id,owner_user_id,episode_id,sequence,kind,event_json,content_digest,captured_at)
         SELECT $1, 'excess-' || n::text, $2, $3, n, 'request_started', '{}', '', '' FROM generate_series(5,$4) AS n",
        vec![store.namespace().into(), OWNER.into(), episode.into(), i64::try_from(MAX_HISTORY_EVENTS + 1)?.into()])).await?;
    let error = shared
        .history_in(&tx, OWNER, episode)
        .await
        .err()
        .context("excess events accepted")?;
    ensure!(error.to_string().contains("history event limit"));
    tx.rollback().await?;
    let tx = store.begin().await?;
    tx.execute(Statement::from_sql_and_values(DatabaseBackend::Postgres,
        "UPDATE gateway_shared_trajectory_events SET kind='request_settled' WHERE namespace=$1 AND episode_id=$2 AND sequence=1",
        [store.namespace().into(), episode.into()])).await?;
    ensure!(shared.history_in(&tx, OWNER, episode).await.is_err());
    tx.rollback().await?;
    let tx = store.begin().await?;
    tx.execute(Statement::from_sql_and_values(DatabaseBackend::Postgres,
        "UPDATE gateway_shared_trajectory_outbox SET payload_json=repeat('x',1048577) WHERE namespace=$1 AND outbox_id=$2",
        [store.namespace().into(), outbox.into()])).await?;
    let error = shared
        .outbox_in(&tx, OWNER, outbox)
        .await
        .err()
        .context("oversize outbox accepted")?;
    ensure!(error.to_string().contains("outbox byte limit"));
    tx.rollback().await?;
    Ok(())
}

fn start(episode: &str, request: &str) -> Result<BeginRequest> {
    let mut event = event(
        episode,
        request,
        "start",
        1,
        TrajectoryEventKind::RequestStarted,
    )?;
    event.evidence.categorical = BTreeMap::from([
        ("correlation.source".into(), "explicit_root".into()),
        ("history.completeness".into(), "complete".into()),
    ]);
    event.content_digest = event.semantic_digest()?;
    Ok(BeginRequest {
        episode: EpisodeStart {
            episode_id: episode.into(),
            correlation_digest: digest('1')?,
            correlation_key_id: "key-1".into(),
            correlation_source: "explicit_root".into(),
            completeness: HistoryCompleteness::Complete,
        },
        event,
        full_input_digest: digest('2')?,
        native_parent_id: None,
        protocol: "responses".into(),
    })
}

fn digest(hex: char) -> Result<KeyedDigest> {
    KeyedDigest::parse(format!("hmac-sha256:key-1:{}", hex.to_string().repeat(64)))
}

fn event(
    episode: &str,
    request: &str,
    suffix: &str,
    sequence: u64,
    kind: TrajectoryEventKind,
) -> Result<TrajectoryEvent> {
    let mut event = TrajectoryEvent {
        schema_version: TRAJECTORY_SCHEMA_VERSION,
        event_id: format!("{episode}-{request}-{suffix}"),
        owner_user_id: OWNER.into(),
        episode_id: episode.into(),
        request_id: Some(request.into()),
        sequence,
        kind,
        evidence: TrajectoryEvidence {
            structural: BTreeMap::new(),
            categorical: BTreeMap::new(),
            digests: BTreeMap::new(),
            route_measurement: None,
            routing_evidence: vec![],
        },
        captured_at: "2026-10-21T00:00:00Z".into(),
        content_digest: String::new(),
    };
    event.content_digest = event.semantic_digest()?;
    Ok(event)
}

fn route_event(episode: &str, request: &str, sequence: u64) -> Result<TrajectoryEvent> {
    let mut event = event(
        episode,
        request,
        "route",
        sequence,
        TrajectoryEventKind::RouteIntentRecorded,
    )?;
    event.evidence.categorical = BTreeMap::from([
        (
            "route.projection".into(),
            "agent_trace/v2|opening|normal".into(),
        ),
        ("route.selected_tier".into(), "tier-a".into()),
        ("route.workflow_state".into(), "opening".into()),
    ]);
    event.content_digest = event.semantic_digest()?;
    Ok(event)
}

fn settlement(
    request: &StoredRequest,
    history: &[TrajectoryEvent],
    sequence: u64,
) -> Result<Settlement> {
    ensure!(
        history.len() == 2 && sequence == 3,
        "settlement did not receive the original prefix"
    );
    let event = event(
        &request.episode_id,
        &request.request_id,
        "settled",
        sequence,
        TrajectoryEventKind::RequestSettled,
    )?;
    Ok(Settlement {
        outbox: Some(OutboxWrite {
            outbox_id: format!("{}-outbox", request.episode_id),
            topic: "trajectory.settled".into(),
            payload: OutboxPayload {
                structural: BTreeMap::from([("trajectory.event_count".into(), sequence)]),
                digests: BTreeMap::from([(
                    "trajectory.event".into(),
                    event.content_digest.clone(),
                )]),
                evaluation: None,
            },
            created_at: event.captured_at.clone(),
        }),
        event,
        status: RequestStatus::Settled,
    })
}

async fn guard_batches(first: &Store, second: &Store, shared: &SharedTrajectory) -> Result<()> {
    use crate::trajectory::{health::reduce, shared::tests::guarded_batch};
    let input = start("guarded-episode", "guarded-first")?;
    let batch = guarded_batch(&[], &input.event)?;
    ensure!(batch.len() == 2);
    let tx = first.begin().await?;
    shared.begin_in(&tx, OWNER, &input).await?;
    tx.commit().await?;
    let tx = first.begin().await?;
    ensure!(
        shared
            .append_route_in(&tx, OWNER, &batch[..1])
            .await
            .is_err()
    );
    tx.rollback().await?;
    let tx = first.begin().await?;
    let mut changed = batch.clone();
    changed[1].evidence.digests.insert(
        "guard.health_snapshot".into(),
        format!("sha256:{}", "b".repeat(64)),
    );
    changed[1].content_digest = changed[1].semantic_digest()?;
    ensure!(shared.append_route_in(&tx, OWNER, &changed).await.is_err());
    tx.rollback().await?;
    let tx = first.begin().await?;
    shared.append_route_in(&tx, OWNER, &batch).await?;
    tx.commit().await?;
    // Discarded application result, followed by exact replay from another pool.
    // This is not a transport-level lost-COMMIT-acknowledgement injection.
    let tx = second.begin().await?;
    let before = shared.history_in(&tx, OWNER, "guarded-episode").await?;
    shared.append_route_in(&tx, OWNER, &batch).await?;
    ensure!(shared.history_in(&tx, OWNER, "guarded-episode").await? == before);
    ensure!(reduce(&before, &Default::default())?.active_hold_remaining == 2);
    tx.commit().await?;
    let tx = second.begin().await?;
    ensure!(
        shared
            .append_route_in(&tx, OWNER, &batch[..1])
            .await
            .is_err()
    );
    tx.rollback().await?;
    for (request, remaining) in [("held-second", 1), ("held-third", 0)] {
        let tx = second.begin().await?;
        let prior = shared.history_in(&tx, OWNER, "guarded-episode").await?;
        let mut next = start("guarded-episode", request)?;
        next.event.sequence = u64::try_from(prior.len())? + 1;
        next.event.content_digest = next.event.semantic_digest()?;
        let held = guarded_batch(&prior, &next.event)?;
        ensure!(held.len() == 1);
        shared.begin_in(&tx, OWNER, &next).await?;
        shared.append_route_in(&tx, OWNER, &held).await?;
        let history = shared.history_in(&tx, OWNER, "guarded-episode").await?;
        ensure!(reduce(&history, &Default::default())?.active_hold_remaining == remaining);
        shared.append_route_in(&tx, OWNER, &batch).await?;
        ensure!(shared.history_in(&tx, OWNER, "guarded-episode").await? == history);
        tx.commit().await?;
    }
    Ok(())
}

async fn late_outbox_failure(
    store: &Store,
    shared: &SharedTrajectory,
    existing_outbox: &str,
) -> Result<()> {
    let input = start("late-conflict", "late-request")?;
    let tx = store.begin().await?;
    shared.begin_in(&tx, OWNER, &input).await?;
    shared
        .append_route_in(
            &tx,
            OWNER,
            &[route_event("late-conflict", "late-request", 2)?],
        )
        .await?;
    tx.commit().await?;
    let tx = store.begin().await?;
    ensure!(
        shared
            .settle_in(&tx, OWNER, "late-request", |r, h, s| {
                let mut result = settlement(r, h, s)?;
                result
                    .outbox
                    .as_mut()
                    .context("missing fixture outbox")?
                    .outbox_id = existing_outbox.into();
                Ok(result)
            })
            .await
            .is_err()
    );
    tx.rollback().await?;
    let tx = store.begin().await?;
    ensure!(shared.history_in(&tx, OWNER, "late-conflict").await?.len() == 2);
    ensure!(
        shared
            .request_in(&tx, OWNER, "late-request")
            .await?
            .context("missing request")?
            .settlement_event_id
            .is_none()
    );
    let without_outbox = |r: &StoredRequest, h: &[TrajectoryEvent], s: u64| {
        let mut result = settlement(r, h, s)?;
        result.outbox = None;
        Ok(result)
    };
    let first = shared
        .settle_in(&tx, OWNER, "late-request", without_outbox)
        .await?;
    tx.commit().await?;
    let tx = store.begin().await?;
    ensure!(
        shared
            .settle_in(&tx, OWNER, "late-request", without_outbox)
            .await?
            == first
    );
    ensure!(
        shared
            .settle_in(&tx, OWNER, "late-request", settlement)
            .await
            .is_err()
    );
    tx.rollback().await?;
    Ok(())
}

async fn count_boundary(store: &Store, shared: &SharedTrajectory) -> Result<()> {
    let episode = "count-boundary";
    let input = start(episode, "count-first")?;
    let route = route_event(episode, "count-first", 2)?;
    let tx = store.begin().await?;
    shared.begin_in(&tx, OWNER, &input).await?;
    shared
        .append_route_in(&tx, OWNER, std::slice::from_ref(&route))
        .await?;
    // Bulk fixture insertion avoids thousands of unrelated round trips. Every
    // inserted event has a valid signed, canonical shape and contiguous sequence.
    // https://www.postgresql.org/docs/current/functions-json.html
    let mut rows = Vec::with_capacity(MAX_HISTORY_EVENTS - 2);
    for sequence in 3..=MAX_HISTORY_EVENTS {
        let event = event(
            episode,
            &format!("count-{sequence}"),
            "start",
            u64::try_from(sequence)?,
            TrajectoryEventKind::RequestStarted,
        )?;
        rows.push(serde_json::json!({
            "event_id": event.event_id, "request_id": event.request_id, "sequence": event.sequence,
            "kind": "request_started", "event_json": serde_json::to_string(&event)?,
            "content_digest": event.content_digest, "captured_at": event.captured_at,
        }));
    }
    tx.execute(Statement::from_sql_and_values(DatabaseBackend::Postgres,
        "INSERT INTO gateway_shared_trajectory_events
         (namespace,owner_user_id,episode_id,event_id,request_id,sequence,kind,event_json,content_digest,captured_at)
         SELECT $1,$2,$3,e.event_id,e.request_id,e.sequence,e.kind,e.event_json,e.content_digest,e.captured_at
         FROM jsonb_to_recordset($4::text::jsonb) AS e(event_id text,request_id text,sequence bigint,kind text,event_json text,content_digest text,captured_at text)",
        vec![store.namespace().into(), OWNER.into(), episode.into(), serde_json::to_string(&rows)?.into()])).await?;
    tx.execute(Statement::from_sql_and_values(DatabaseBackend::Postgres,
        "UPDATE gateway_shared_trajectory_episodes SET next_sequence=$3,latest_request_id=$4 WHERE namespace=$1 AND episode_id=$2",
        vec![store.namespace().into(), episode.into(), i64::try_from(MAX_HISTORY_EVENTS + 1)?.into(), format!("count-{MAX_HISTORY_EVENTS}").into()])).await?;
    let history = shared.history_in(&tx, OWNER, episode).await?;
    ensure!(history.len() == MAX_HISTORY_EVENTS);
    shared.append_route_in(&tx, OWNER, &[route]).await?;
    ensure!(shared.history_in(&tx, OWNER, episode).await? == history);
    let mut excess = start(episode, "excess")?;
    excess.event.sequence = u64::try_from(MAX_HISTORY_EVENTS + 1)?;
    excess.event.content_digest = excess.event.semantic_digest()?;
    let error = shared
        .begin_in(&tx, OWNER, &excess)
        .await
        .err()
        .context("over-limit append accepted")?;
    ensure!(error.to_string().contains("history event limit"));
    tx.rollback().await?;
    let tx = store.begin().await?;
    ensure!(shared.episode_in(&tx, OWNER, episode).await?.is_none());
    tx.commit().await?;
    Ok(())
}
