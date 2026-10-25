use super::super::tests::{correlate_input, guarded_route_input};
use super::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement, TransactionTrait, sea_query::Expr};

fn fixture(
    db: &DatabaseConnection,
    installation: &str,
    tenant: &str,
    principal: &str,
) -> Result<OwnedStore> {
    let (scope, provenance) = Scope::for_test(
        installation.try_into()?,
        tenant.try_into()?,
        principal.try_into()?,
    );
    Ok(OwnedStore {
        db: db.clone(),
        scope,
        provenance,
    })
}
fn input() -> super::super::CorrelateAndBegin {
    correlate_input(
        "same-request",
        "same-episode",
        "same-start",
        guarded_route_input("same-route", "same-guard"),
    )
}
async fn start(store: &OwnedStore) -> Result<()> {
    let tx = store.database().begin().await?;
    store
        .correlate_and_begin_in(&tx, &input(), &mut io::Budget::new())
        .await?;
    tx.commit().await?;
    Ok(())
}
async fn settle(store: &OwnedStore, deliver: bool) -> Result<Settlement> {
    let tx = store.database().begin().await?;
    let owner = store.owner()?;
    let mut budget = io::Budget::new();
    let value = store
        .settle_current_in(
            &tx,
            "same-request",
            |request, events, sequence| {
                crate::trajectory::settlement::build_from_evidence(
                    &owner,
                    request,
                    events,
                    sequence,
                    TrajectoryEvidence {
                        structural: std::collections::BTreeMap::from([(
                            "settlement.duration_ms".into(),
                            10,
                        )]),
                        categorical: std::collections::BTreeMap::from([(
                            "settlement.outcome".into(),
                            "succeeded".into(),
                        )]),
                        digests: Default::default(),
                        routing_evidence: Vec::new(),
                        route_measurement: None,
                    },
                    "2026-08-01T00:00:10Z".into(),
                    RequestStatus::Settled,
                )
            },
            &mut budget,
        )
        .await?;
    if deliver {
        store
            .mark_delivered_in(
                &tx,
                value.outbox.as_ref().context("missing fixture outbox")?,
                &mut budget,
            )
            .await?;
    }
    tx.commit().await?;
    Ok(value)
}

#[tokio::test]
async fn namespaces_creators_replay_and_prune_are_isolated() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    exercise(&db, &uuid::Uuid::new_v4().to_string()).await
}

#[tokio::test]
async fn guard_route_batch_rejects_partial_input_before_writes_and_replays_exactly() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let store = fixture(&db, "batch-installation", "batch-tenant", "creator")?;
    let tx = db.begin().await?;
    let mut input = input();
    let guard = input.guarded_route.take().context("guarded fixture")?;
    let started = store
        .correlate_and_begin_in(&tx, &input, &mut io::Budget::new())
        .await?;
    let ((before, _), events) = store
        .history(&tx, &started.episode_id, &mut io::Budget::new())
        .await?;
    ensure!(events.len() == 1);
    let batch =
        super::super::build_guarded_route_batch(&[], &events[0], started.completeness, &guard)?;
    let activation = batch
        .guard_event
        .context("fixture must activate the guard")?;
    ensure!(
        store
            .append_route_in(
                &tx,
                std::slice::from_ref(&batch.route_event),
                &mut io::Budget::new()
            )
            .await
            .is_err()
    );
    let ((after_rejection, _), remaining) = store
        .history(&tx, &started.episode_id, &mut io::Budget::new())
        .await?;
    ensure!(after_rejection == before && remaining == events);
    let complete = vec![batch.route_event, activation];
    store
        .append_route_in(&tx, &complete, &mut io::Budget::new())
        .await?;
    let ((after, _), durable) = store
        .history(&tx, &started.episode_id, &mut io::Budget::new())
        .await?;
    ensure!(durable.len() == 3 && after.next_sequence == 4);
    super::super::reduce(&durable, &guard.policy.protected_tiers)?;
    store
        .append_route_in(&tx, &complete, &mut io::Budget::new())
        .await?;
    ensure!(
        store
            .append_route_in(&tx, &complete[..1], &mut io::Budget::new())
            .await
            .is_err()
    );
    let ((replayed, _), replayed_events) = store
        .history(&tx, &started.episode_id, &mut io::Budget::new())
        .await?;
    ensure!(replayed == after && replayed_events == durable);
    tx.commit().await?;
    let read = db.begin().await?;
    let ((committed, _), committed_events) = store
        .history(&read, &started.episode_id, &mut io::Budget::new())
        .await?;
    ensure!(committed == after && committed_events == durable);
    read.rollback().await?;
    db.close().await?;
    Ok(())
}

async fn exercise(db: &DatabaseConnection, suffix: &str) -> Result<()> {
    let installation = format!("i-{suffix}");
    let tenant = format!("A-{suffix}");
    let a = fixture(db, &installation, &tenant, "principal-a")?;
    let replay = fixture(db, &installation, &tenant, "principal-b")?;
    let b = fixture(db, &installation, &format!("a-{suffix}"), "principal-a")?;
    let other = fixture(db, &format!("j-{suffix}"), &tenant, "principal-a")?;
    for store in [&a, &b, &other] {
        start(store).await?;
    }
    start(&replay).await?;
    let tx = db.begin().await?;
    let (_, creator) = replay
        .request_row(&tx, "same-request", &mut io::Budget::new())
        .await?
        .context("missing request")?;
    ensure!(creator.creator().map(|p| p.as_str()) == Some("principal-a"));
    let mut changed = input();
    changed.canonical_input_bytes += 1;
    ensure!(
        replay
            .correlate_and_begin_in(&tx, &changed, &mut io::Budget::new())
            .await
            .is_err()
    );
    // An expected immutable conflict must not poison PostgreSQL's transaction.
    tx.query_one(Statement::from_string(
        db.get_database_backend(),
        "SELECT 1 AS alive".to_owned(),
    ))
    .await?
    .context("transaction unusable after replay conflict")?;
    tx.rollback().await?;
    let first = settle(&a, true).await?;
    let duplicate = settle(&replay, true).await?;
    ensure!(first.event == duplicate.event);
    let tx = db.begin().await?;
    let mut budget = io::Budget::new();
    let pending = a.pending_in(&tx, &mut budget).await?;
    ensure!(pending.is_empty());
    let summary = a
        .prune_in(&tx, "2027-01-01T00:00:00Z", false, &mut budget)
        .await?;
    ensure!(
        summary.episode_rows == 1
            && summary.request_rows == 1
            && summary.delivered_outbox_rows == 1
    );
    ensure!(
        b.request_in(&tx, "same-request", &mut io::Budget::new())
            .await?
            .is_some()
    );
    ensure!(
        other
            .request_in(&tx, "same-request", &mut io::Budget::new())
            .await?
            .is_some()
    );
    tx.commit().await?;
    // A Legacy owner equal to the internal compatibility string is still Legacy.
    let legacy = super::super::TrajectoryStore::new(db.clone());
    ensure!(legacy.request(&b.owner()?, "same-request").await?.is_none());
    ensure!(
        legacy
            .resolve_episode_owner(&b.key(KeyDomain::TrajectoryEpisode, "same-episode")?)
            .await?
            .is_none()
    );
    if db.get_database_backend() == DatabaseBackend::Sqlite {
        let untouched = legacy
            .prune_before("2027-01-01T00:00:00Z", false, 10)
            .await?;
        ensure!(untouched.episode_rows == 0);
    }
    let tx = db.begin().await?;
    ensure!(
        b.request_in(&tx, "same-request", &mut io::Budget::new())
            .await?
            .is_some()
    );
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn foreign_physical_collision_fails_without_poisoning_transaction() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    collision(&db, &uuid::Uuid::new_v4().to_string()).await
}

#[tokio::test]
async fn legacy_prefix_collision_cannot_adopt_owned_summary() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let owned = fixture(&db, "installation", "tenant", "creator")?;
    start(&owned).await?;
    let legacy = super::super::TrajectoryStore::new(db.clone());
    let legacy_input = correlate_input(
        "legacy-request",
        "legacy-episode",
        "legacy-start",
        guarded_route_input("legacy-route", "legacy-guard"),
    );
    ensure!(
        legacy
            .correlate_and_begin(&owned.owner()?, legacy_input)
            .await
            .is_err()
    );
    ensure!(
        legacy
            .request(&owned.owner()?, "legacy-request")
            .await?
            .is_none()
    );
    let tx = db.begin().await?;
    ensure!(
        owned
            .request_in(&tx, "same-request", &mut io::Budget::new())
            .await?
            .is_some()
    );
    tx.rollback().await?;
    Ok(())
}
async fn collision(db: &DatabaseConnection, suffix: &str) -> Result<()> {
    let owned = fixture(db, &format!("i-{suffix}"), "tenant", "creator")?;
    let legacy = super::super::TrajectoryStore::new(db.clone());
    let mut old = super::super::tests::begin(
        &owned.key(KeyDomain::TrajectoryEpisode, "same-episode")?,
        &format!("legacy-{suffix}"),
    );
    old.event.owner_user_id = "legacy-owner".into();
    old.event.content_digest = old.event.semantic_digest()?;
    legacy.begin_request("legacy-owner", old).await?;
    let tx = db.begin().await?;
    ensure!(
        owned
            .correlate_and_begin_in(&tx, &input(), &mut io::Budget::new())
            .await
            .is_err()
    );
    tx.query_one(Statement::from_string(
        db.get_database_backend(),
        "SELECT 1 AS alive".to_owned(),
    ))
    .await?
    .context("transaction unusable after collision")?;
    tx.rollback().await?;
    let tx = db.begin().await?;
    ensure!(
        owned
            .request_in(&tx, "same-request", &mut io::Budget::new())
            .await?
            .is_none()
    );
    tx.rollback().await?;
    ensure!(
        legacy
            .request("legacy-owner", &format!("legacy-{suffix}"))
            .await?
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn malformed_metadata_projection_and_oversized_payload_fail_closed() -> Result<()> {
    for mutation in ["metadata", "projection", "payload"] {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = fixture(&db, "installation", "tenant", "creator")?;
        start(&store).await?;
        let mut update = event_entity::Entity::update_many().filter(
            event_entity::Column::EventId.eq(store.key(KeyDomain::TrajectoryEvent, "same-start")?),
        );
        update = match mutation {
            "metadata" => update.col_expr(
                event_entity::Column::CreatorPrincipalId,
                Expr::value(Option::<String>::None),
            ),
            "projection" => {
                update.col_expr(event_entity::Column::RequestId, Expr::value("foreign"))
            }
            _ => update.col_expr(
                event_entity::Column::EventJson,
                Expr::value("x".repeat(60 * 1024 + 1)),
            ),
        };
        update.exec(&db).await?;
        let tx = db.begin().await?;
        ensure!(
            store
                .request_in(&tx, "same-request", &mut io::Budget::new())
                .await
                .is_err()
        );
        tx.rollback().await?;
    }
    Ok(())
}

#[tokio::test]
async fn borrowed_terminal_rollback_removes_event_outbox_and_status_together() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let store = fixture(&db, "installation", "tenant", "creator")?;
    start(&store).await?;
    let tx = db.begin().await?;
    let owner = store.owner()?;
    store
        .settle_current_in(
            &tx,
            "same-request",
            |request, events, sequence| {
                crate::trajectory::settlement::build_from_evidence(
                    &owner,
                    request,
                    events,
                    sequence,
                    TrajectoryEvidence {
                        structural: Default::default(),
                        categorical: Default::default(),
                        digests: Default::default(),
                        routing_evidence: Vec::new(),
                        route_measurement: None,
                    },
                    "2026-08-01T00:00:10Z".into(),
                    RequestStatus::Settled,
                )
            },
            &mut io::Budget::new(),
        )
        .await?;
    tx.rollback().await?;
    let tx = db.begin().await?;
    let request = store
        .request_in(&tx, "same-request", &mut io::Budget::new())
        .await?
        .context("missing request")?;
    ensure!(request.status == RequestStatus::Started && request.settlement_event_id.is_none());
    ensure!(
        store
            .pending_in(&tx, &mut io::Budget::new())
            .await?
            .is_empty()
    );
    tx.rollback().await?;
    Ok(())
}

#[test]
fn budget_is_one_operation_and_checks_overflow() -> Result<()> {
    let mut budget = io::Budget::new();
    budget.charge(4096, io::MAX_BYTES)?;
    ensure!(budget.charge(0, 1).is_err());
    let mut budget = io::Budget::new();
    budget.charge(4096, 0)?;
    ensure!(budget.charge(1, 0).is_err());
    let mut budget = io::Budget::new();
    ensure!(budget.charge(0, usize::MAX).is_err());
    Ok(())
}

#[tokio::test]
async fn logical_case_space_and_unicode_are_distinct_without_changing_bytes() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let store = fixture(&db, "installation", "tenant", "creator")?;
    for (index, name) in ["same", "Same", "same ", "é", "e\u{301}"]
        .iter()
        .enumerate()
    {
        let mut input = correlate_input(
            name,
            &format!("episode-{index}"),
            &format!("start-{index}"),
            guarded_route_input(&format!("route-{index}"), &format!("guard-{index}")),
        );
        input.full_input_digest = super::super::tests::keyed_digest("key-1", &index.to_string());
        let tx = db.begin().await?;
        store
            .correlate_and_begin_in(&tx, &input, &mut io::Budget::new())
            .await?;
        tx.commit().await?;
    }
    let tx = db.begin().await?;
    for name in ["same", "Same", "same ", "é", "e\u{301}"] {
        let row = store
            .request_in(&tx, name, &mut io::Budget::new())
            .await?
            .context("lost logical request")?;
        ensure!(row.request_id == name);
    }
    tx.rollback().await?;
    Ok(())
}

#[tokio::test]
async fn aggregate_sql_preflight_rejects_before_decoding_oversized_collection() -> Result<()> {
    for (count, size, expected) in [(600, 60 * 1024, "byte bound"), (4097, 1, "row bound")] {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = fixture(&db, "installation", "tenant", "creator")?;
        // Intentionally malformed JSON: the aggregate preflight must reject it
        // before any attempt to decode a payload or logical identity.
        db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x < ?) \
             INSERT INTO trajectory_outbox(outbox_id,owner_user_id,topic,payload_json,payload_digest,attempts,created_at,installation_id,tenant_id,creator_principal_id,logical_identity_json) \
             SELECT CAST(x AS TEXT),?,'topic',?,'digest',0,'time','installation','tenant','creator','{}' FROM n",
            vec![sea_orm::Value::from(count),store.owner()?.into(),"x".repeat(size).into()])).await?;
        let tx = db.begin().await?;
        let error = store
            .pending_in(&tx, &mut io::Budget::new())
            .await
            .err()
            .context("unbounded SQL collection was accepted")?;
        ensure!(
            error.to_string().contains(expected),
            "wrong rejection: {error}"
        );
        tx.rollback().await?;
    }
    Ok(())
}

#[cfg(any(feature = "db-postgres", feature = "db-mysql"))]
#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL for a dedicated PostgreSQL/MySQL database"]
async fn server_owned_trajectory_store_preserves_transaction_and_scope() -> Result<()> {
    let url = std::env::var("BITROUTER_TEST_DATABASE_URL")?;
    let parsed = url::Url::parse(&url)?;
    ensure!(
        !parsed.path().trim_matches('/').is_empty(),
        "dedicated database name required"
    );
    match parsed.scheme() {
        "postgres" | "postgresql" => ensure!(cfg!(feature = "db-postgres")),
        "mysql" => ensure!(cfg!(feature = "db-mysql")),
        _ => anyhow::bail!("server URL required"),
    }
    let db = crate::db::connect(&url).await?;
    crate::db::run_migrations(&db).await?;
    let suffix = uuid::Uuid::new_v4().to_string();
    exercise(&db, &suffix).await?;
    collision(&db, &suffix).await?;
    db.close().await?;
    Ok(())
}
