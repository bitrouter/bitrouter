//! Real database regressions shared by SQLite, PostgreSQL, and MySQL.
//!
//! SQLite always runs against a temporary file. The ignored server test requires
//! an explicit BITROUTER_TEST_DATABASE_URL pointing to a DEDICATED TEST DATABASE.
//! It applies all pending migrations and writes fixtures: never use a production
//! database. Existing migrated test databases and the known partially applied
//! migration-12 state are supported; other interrupted migrations are not covered.
//! No database is dropped, no table is truncated, and existing fixtures are not
//! deleted. Each invocation uses UUID-scoped identities and leaves server fixtures
//! intact, so repeated runs do not depend on an empty database.
//!
//! Run this target with --no-default-features, adding db-postgres or db-mysql for
//! the matching server URL. Select server_database_behaviors with --ignored to
//! opt in. Each backend exercise has a two-minute deadline and a fixed fixture
//! count; no core, harness, TUI, external process, or network model is required.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitrouter::auth::db::{self as auth_db, NewApiKey};
use bitrouter::auth::entities::users;
use bitrouter::auth::keys::hash_key;
use bitrouter::db::migration::m20240101_000012_create_trajectory_ledger;
use bitrouter::db::{self, migration::Migrator};
use bitrouter::metering::db::{MeteringSessionIdentity, ReconciliationStatus, RequestMetric};
use bitrouter::metering::entities::requests;
use bitrouter::metering::pricing::{
    ChargeStatus, ModelPricing, PricingSource, calculate_charge_evidence,
};
use bitrouter::metering::store::{MeteringStore, TimeWindow, TokenUsage};
use bitrouter::storage::evolution::{EvolutionStore, records};
use bitrouter::trajectory::store::TrajectoryStore;
use bitrouter::trajectory::types::{
    BeginRequest, EpisodeStart, HistoryCompleteness, KeyedDigest, OutboxPayload, OutboxWrite,
    PendingOutbox, RequestStatus, Settlement, StoredEpisode, StoredRequest,
    TRAJECTORY_SCHEMA_VERSION, TrajectoryEvent, TrajectoryEventKind, TrajectoryEvidence,
    canonical_digest,
};
use bitrouter_ai::types::{Usage, UsageOrigin};
use chrono::Utc;
use sea_orm::{DatabaseConnection, EntityTrait};
use sea_orm_migration::{MigrationTrait, MigratorTrait, SchemaManager};
use uuid::Uuid;

#[tokio::test]
async fn sqlite_database_behaviors() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("database-backends.sqlite");
    exercise_bounded(&format!("sqlite://{}", path.display())).await?;
    ensure!(path.is_file(), "SQLite regression must use a real file");
    Ok(())
}

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL for a dedicated PostgreSQL or MySQL test database"]
async fn server_database_behaviors() -> Result<()> {
    let database_url = std::env::var("BITROUTER_TEST_DATABASE_URL").context(
        "set BITROUTER_TEST_DATABASE_URL to a dedicated test database; no fallback is used",
    )?;
    let parsed = url::Url::parse(&database_url)
        .context("BITROUTER_TEST_DATABASE_URL must be a valid server database URL")?;
    match parsed.scheme() {
        "postgres" | "postgresql" => {
            ensure!(
                cfg!(feature = "db-postgres"),
                "enable the db-postgres feature"
            );
        }
        "mysql" => ensure!(cfg!(feature = "db-mysql"), "enable the db-mysql feature"),
        _ => anyhow::bail!("BITROUTER_TEST_DATABASE_URL must select PostgreSQL or MySQL"),
    }
    ensure!(
        !parsed.path().trim_matches('/').is_empty(),
        "BITROUTER_TEST_DATABASE_URL must explicitly name a dedicated test database"
    );
    exercise_bounded(&database_url).await
}

async fn exercise_bounded(database_url: &str) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(120), exercise(database_url))
        .await
        .context("database backend regression exceeded its two-minute deadline")?
}

async fn exercise(database_url: &str) -> Result<()> {
    let connection = db::connect(database_url).await?;
    let fixture = Fixture::new();
    let result = async {
        let previous_ledger = Migrator::get_migration_models(&connection).await?;
        db::run_migrations(&connection).await?;
        let ledger = Migrator::get_migration_models(&connection).await?;
        let expected: Vec<String> = Migrator::migrations()
            .iter()
            .map(|migration| migration.name().to_owned())
            .collect();
        let actual: Vec<String> = ledger.iter().map(|row| row.version.clone()).collect();
        ensure!(!expected.is_empty(), "the migrator must contain migrations");
        ensure!(
            actual == expected,
            "migration ledger does not match the complete migrator order"
        );
        ensure!(
            previous_ledger.iter().all(|row| ledger.contains(row)),
            "migration changed a previously applied ledger entry"
        );
        ensure!(
            Migrator::get_pending_migrations(&connection)
                .await?
                .is_empty()
        );

        write_auth(&connection, &fixture).await?;
        write_evolution(&connection, &fixture).await?;
        #[cfg(feature = "core")]
        exercise_reliability(&connection, &fixture).await?;
        write_metering(&connection, &fixture).await?;
        write_trajectory(&connection, &fixture).await?;
        check_auth(&connection, &fixture).await?;
        check_evolution(&connection, &fixture).await?;
        check_metering(&connection, &fixture).await?;
        let trajectory = read_trajectory(&connection, &fixture).await?;
        check_trajectory(&trajectory, &fixture)?;

        // Invoke the migration itself: the ledger would otherwise skip its index
        // existence checks. In particular, MySQL cannot rely on CREATE INDEX
        // IF NOT EXISTS. This must preserve already populated trajectory tables.
        m20240101_000012_create_trajectory_ledger::Migration
            .up(&SchemaManager::new(&connection))
            .await?;
        ensure!(
            read_trajectory(&connection, &fixture).await? == trajectory,
            "direct migration 12 rerun changed trajectory records"
        );

        db::run_migrations(&connection).await?;
        ensure!(
            Migrator::get_migration_models(&connection).await? == ledger,
            "second migrator run changed ledger versions or applied timestamps"
        );
        ensure!(
            read_trajectory(&connection, &fixture).await? == trajectory,
            "second migrator run changed trajectory records"
        );
        check_auth(&connection, &fixture).await?;
        check_metering(&connection, &fixture).await?;
        Ok::<_, anyhow::Error>(trajectory)
    }
    .await;
    let closed = connection.close().await;
    let trajectory = result?;
    closed?;

    // A new pool proves the fixtures survived connection teardown, including
    // SQLite's file-backed persistence and the settlement's transactional outbox.
    let reopened = db::connect(database_url).await?;
    let result = async {
        check_auth(&reopened, &fixture).await?;
        check_evolution(&reopened, &fixture).await?;
        check_metering(&reopened, &fixture).await?;
        ensure!(
            read_trajectory(&reopened, &fixture).await? == trajectory,
            "trajectory records did not survive reconnecting"
        );
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let closed = reopened.close().await;
    result?;
    closed?;
    Ok(())
}

struct Fixture {
    scope: String,
    captured_at: String,
    window: TimeWindow,
}

impl Fixture {
    fn new() -> Self {
        let now = Utc::now();
        Self {
            scope: Uuid::new_v4().to_string(),
            captured_at: now.to_rfc3339(),
            window: TimeWindow::Custom {
                start: now - chrono::Duration::minutes(1),
                end: now + chrono::Duration::minutes(5),
            },
        }
    }

    fn id(&self, suffix: &str) -> String {
        format!("db-{}-{suffix}", self.scope)
    }

    fn key(&self, owner: &str) -> NewApiKey {
        NewApiKey {
            id: self.id(&format!("key-{owner}")),
            key_hash: hash_key(&self.id(&format!("secret-{owner}"))),
            user_id: self.id(&format!("owner-{owner}")),
            spend_limit_micro_usd: Some(1_000_000),
            rpm_limit: Some(60),
            policy_id: Some(self.id(&format!("policy-{owner}"))),
        }
    }

    fn metric(
        &self,
        owner: &str,
        request: &str,
        prompt: u64,
        completion: u64,
    ) -> Result<RequestMetric> {
        let key = self.key(owner);
        let usage = Usage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            origin: UsageOrigin::ProviderReported,
            ..Default::default()
        };
        let evidence = calculate_charge_evidence(
            &usage,
            &ModelPricing::new(2.0, 10.0),
            PricingSource::Configured,
        );
        Ok(RequestMetric {
            request_id: self.id(request),
            user_id: key.user_id,
            api_key_id: key.id.clone(),
            launch_id: Some(self.id("launch")),
            session_identity: Some(MeteringSessionIdentity {
                route_scope_id: key.id,
                agent_harness: None,
                controller_instance_id: Some(self.id("controller")),
                acp_session_id: Some(self.id("session")),
                native_root_session_id: None,
                native_agent_thread_id: None,
                native_parent_agent_thread_id: None,
                native_turn_id: None,
                route_lease_id: None,
                serialized: serde_json::json!({
                    "controller_instance_id": self.id("controller"),
                    "acp_session_id": self.id("session"),
                })
                .to_string(),
            }),
            router_id: Some(self.id("router")),
            binding_digest: Some(canonical_digest(&self.id("binding"))?),
            original_selector: Some(self.id("selector")),
            model_id: self.id("model"),
            provider_id: self.id("provider"),
            prompt_tokens: prompt,
            completion_tokens: completion,
            reasoning_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            uncached_input_tokens: prompt,
            output_tokens: completion,
            usage_origin: usage.origin,
            raw_usage: Some(
                serde_json::json!({"input_tokens": prompt, "output_tokens": completion}),
            ),
            charge_status: evidence.status,
            estimated_charge_micro_usd: evidence
                .charge_micro_usd
                .context("fixture charge missing")?,
            charge_evidence: evidence,
            reconciliation_status: ReconciliationStatus::NotApplicable,
            latency_ms: 12,
            generation_time_ms: 8,
            streamed: false,
            error: None,
        })
    }

    fn event(&self, sequence: u64, kind: TrajectoryEventKind) -> Result<TrajectoryEvent> {
        let categorical = match kind {
            TrajectoryEventKind::RequestStarted => BTreeMap::from([
                ("correlation.source".into(), "explicit_root".into()),
                ("history.completeness".into(), "complete".into()),
            ]),
            TrajectoryEventKind::RouteIntentRecorded => BTreeMap::from([
                (
                    "route.projection".into(),
                    "agent_trace/v2|opening|normal".into(),
                ),
                ("route.selected_tier".into(), "economy".into()),
                ("route.workflow_state".into(), "opening".into()),
            ]),
            _ => BTreeMap::new(),
        };
        let mut event = TrajectoryEvent {
            schema_version: TRAJECTORY_SCHEMA_VERSION,
            event_id: self.id(&format!("event-{sequence}")),
            owner_user_id: self.key("a").user_id,
            episode_id: self.id("episode"),
            request_id: Some(self.id("request-a1")),
            sequence,
            kind,
            evidence: TrajectoryEvidence {
                structural: BTreeMap::new(),
                categorical,
                digests: BTreeMap::new(),
                route_measurement: None,
                routing_evidence: Vec::new(),
            },
            captured_at: self.captured_at.clone(),
            content_digest: String::new(),
        };
        event.content_digest = event.semantic_digest()?;
        Ok(event)
    }
}

async fn write_auth(connection: &DatabaseConnection, fixture: &Fixture) -> Result<()> {
    for owner in ["a", "b"] {
        let key = fixture.key(owner);
        auth_db::upsert_user(connection, &key.user_id).await?;
        let before = users::Entity::find_by_id(&key.user_id)
            .one(connection)
            .await?
            .context("upsert did not persist a user")?;
        auth_db::upsert_user(connection, &key.user_id).await?;
        ensure!(
            users::Entity::find_by_id(&key.user_id)
                .one(connection)
                .await?
                == Some(before),
            "user upsert changed the existing row"
        );
        auth_db::insert_api_key(connection, &key).await?;
    }
    Ok(())
}

async fn check_auth(connection: &DatabaseConnection, fixture: &Fixture) -> Result<()> {
    for owner in ["a", "b"] {
        let key = fixture.key(owner);
        let stored = auth_db::find_key_by_hash(connection, &key.key_hash)
            .await?
            .context("inserted API key missing")?;
        ensure!(
            stored.id == key.id && stored.user_id == key.user_id,
            "API key lookup returned another key or owner"
        );
        ensure!(stored.spend_limit_micro_usd == key.spend_limit_micro_usd);
        ensure!(stored.rpm_limit == key.rpm_limit && stored.policy_id == key.policy_id);
        ensure!(stored.active && stored.expires_at.is_none());
    }
    ensure!(
        auth_db::find_key_by_hash(connection, &hash_key(&fixture.id("missing-secret")))
            .await?
            .is_none(),
        "unknown key hash must not resolve"
    );
    Ok(())
}

async fn write_evolution(connection: &DatabaseConnection, fixture: &Fixture) -> Result<()> {
    let kind = "database-backend-fixture";
    let key = fixture.id("assignment");
    let initial = serde_json::json!({"route": "economy", "epoch": 1});
    let store = EvolutionStore::new(connection.clone(), &fixture.key("a").user_id)?;
    ensure!(
        store
            .initialize(kind, &key, Some(fixture.id("session")), &initial)
            .await?
            == (0, initial.clone())
    );
    let replacement = serde_json::json!({"route": "uncommitted", "epoch": 9});
    ensure!(
        store.initialize(kind, &key, None, &replacement).await? == (0, initial),
        "initialization must retain the first writer's body and revision"
    );
    let (revision, ()) = store
        .update(kind, &key, 0, |body: &mut serde_json::Value| {
            body["route"] = "balanced".into();
            Ok(())
        })
        .await?;
    ensure!(revision == 1);
    ensure!(
        store
            .update(kind, &key, 0, |body: &mut serde_json::Value| {
                body["route"] = "stale".into();
                Ok(())
            })
            .await
            .is_err(),
        "stale revision must not overwrite committed state"
    );
    let expected = serde_json::json!({"route": "balanced", "epoch": 1});
    ensure!(
        store.initialize(kind, &key, None, &replacement).await? == (1, expected),
        "reinitialization must not reset an updated record"
    );
    let other = EvolutionStore::new(connection.clone(), &fixture.key("b").user_id)?;
    ensure!(other.get::<serde_json::Value>(kind, &key).await?.is_none());
    ensure!(
        other.initialize(kind, &key, None, &replacement).await? == (0, replacement),
        "the same logical key must remain owner scoped"
    );
    Ok(())
}

async fn check_evolution(connection: &DatabaseConnection, fixture: &Fixture) -> Result<()> {
    let kind = "database-backend-fixture";
    let key = fixture.id("assignment");
    for (owner, revision, body, session_key) in [
        (
            "a",
            1,
            serde_json::json!({"route": "balanced", "epoch": 1}),
            Some(fixture.id("session")),
        ),
        (
            "b",
            0,
            serde_json::json!({"route": "uncommitted", "epoch": 9}),
            None,
        ),
    ] {
        let store = EvolutionStore::new(connection.clone(), &fixture.key(owner).user_id)?;
        ensure!(
            store.get::<serde_json::Value>(kind, &key).await? == Some((revision, body.clone()))
        );
        let row = records::Entity::find_by_id(store.id(kind, &key)?)
            .one(connection)
            .await?
            .context("scoped evolution record missing")?;
        ensure!(row.scope_id == store.scope_id && row.kind == kind);
        ensure!(row.session_key == session_key && row.revision == revision);
        ensure!(serde_json::from_str::<serde_json::Value>(&row.body)? == body);
    }
    Ok(())
}

#[cfg(feature = "core")]
async fn exercise_reliability(connection: &DatabaseConnection, fixture: &Fixture) -> Result<()> {
    use bitrouter::core::adequacy::reliability::{
        ReliabilityEvent, ReliabilityKey, ReliabilityObservation,
    };
    use bitrouter::core::adequacy::store::{AdequacyStore, ReliabilityAppendOutcome};

    let store = AdequacyStore::new(connection.clone());
    let event = ReliabilityEvent {
        request_id: fixture.id("reliability-request"),
        route_key: fixture.id("route"),
        endpoint_key: ReliabilityKey {
            provider: fixture.id("provider"),
            model: fixture.id("model"),
            credential_class: "api-key".into(),
            endpoint_scope: "fixture.invalid:443".into(),
            protocol: "responses".into(),
        },
        observation: ReliabilityObservation::TransientFailure,
        half_open_probe: false,
        observed_at_unix: 100,
    };
    let (first, concurrent) = tokio::join!(
        store.append_reliability_event(&event),
        store.append_reliability_event(&event),
    );
    let outcomes = [first?, concurrent?];
    ensure!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == ReliabilityAppendOutcome::Inserted)
            .count()
            == 1,
        "exactly one writer must report a new reliability event"
    );
    ensure!(outcomes.contains(&ReliabilityAppendOutcome::Duplicate));
    let original: Vec<_> = store
        .load_reliability_events()
        .await?
        .into_iter()
        .filter(|row| row.event.request_id == event.request_id)
        .collect();
    ensure!(original.len() == 1 && original[0].event == event);
    let mut retry = event.clone();
    retry.observed_at_unix = 101;
    ensure!(
        store.append_reliability_event(&retry).await? == ReliabilityAppendOutcome::Duplicate,
        "a retry observation timestamp does not create another event"
    );
    let mut conflict = event.clone();
    conflict.observation = ReliabilityObservation::Success;
    ensure!(store.append_reliability_event(&conflict).await.is_err());
    let retained: Vec<_> = store
        .load_reliability_events()
        .await?
        .into_iter()
        .filter(|row| row.event.request_id == event.request_id)
        .collect();
    ensure!(retained == original, "retries changed reliability evidence");
    Ok(())
}

async fn write_metering(connection: &DatabaseConnection, fixture: &Fixture) -> Result<()> {
    let store = MeteringStore::new(connection.clone());
    store
        .record_request(fixture.metric("a", "request-a1", 10, 5)?)
        .await?;
    let initial = requests::Entity::find_by_id(fixture.id("request-a1"))
        .one(connection)
        .await?
        .context("metering insert missing")?;
    store
        .record_request(fixture.metric("a", "request-a1", 15, 5)?)
        .await?;
    let updated = requests::Entity::find_by_id(fixture.id("request-a1"))
        .one(connection)
        .await?
        .context("metering upsert missing")?;
    ensure!(
        updated.created_at == initial.created_at,
        "metering upsert reset created_at"
    );
    store
        .record_request(fixture.metric("a", "request-a2", 20, 0)?)
        .await?;
    store
        .record_request(fixture.metric("b", "request-b1", 50, 5)?)
        .await?;
    Ok(())
}

async fn check_metering(connection: &DatabaseConnection, fixture: &Fixture) -> Result<()> {
    let store = MeteringStore::new(connection.clone());
    for (owner, count, spend, prompt, completion) in [("a", 2, 120, 35, 5), ("b", 1, 150, 50, 5)] {
        let key = fixture.key(owner);
        ensure!(store.get_request_count(&key.id, fixture.window).await? == count);
        ensure!(store.get_spend(&key.id, fixture.window).await? == spend);
        ensure!(store.get_enforceable_spend(&key.id, fixture.window).await? == Some(spend));
        ensure!(
            store
                .get_token_usage(&key.id, &fixture.id("model"), fixture.window)
                .await?
                == TokenUsage {
                    prompt_tokens: prompt,
                    completion_tokens: completion,
                    total_tokens: prompt + completion
                }
        );
        // The two owners deliberately declare the same controller and session.
        let summary = store
            .spend_summary_for_acp_session(
                &key.id,
                &fixture.id("controller"),
                &fixture.id("session"),
                fixture.window,
            )
            .await?;
        ensure!(
            summary.requests == count && summary.spend_micro_usd == spend && summary.unpriced == 0,
            "principal-scoped spend included another owner's requests"
        );
    }
    let unknown = store
        .spend_summary_for_acp_session(
            &fixture.id("missing-principal"),
            &fixture.id("controller"),
            &fixture.id("session"),
            fixture.window,
        )
        .await?;
    ensure!(unknown.requests == 0 && unknown.spend_micro_usd == 0);
    let recent = store
        .recent_requests(fixture.window, 4, Some(&fixture.id("launch")))
        .await?;
    ensure!(
        recent.len() == 3,
        "metering upsert inserted a duplicate request"
    );
    for (owner, request, prompt, completion) in [
        ("a", "request-a1", 15, 5),
        ("a", "request-a2", 20, 0),
        ("b", "request-b1", 50, 5),
    ] {
        let metric = fixture.metric(owner, request, prompt, completion)?;
        let row = requests::Entity::find_by_id(&metric.request_id)
            .one(connection)
            .await?
            .context("persisted metering request missing")?;
        ensure!(row.user_id == metric.user_id && row.api_key_id == metric.api_key_id);
        ensure!(
            row.prompt_tokens == i64::try_from(prompt)?
                && row.completion_tokens == i64::try_from(completion)?
        );
        ensure!(row.estimated_charge_micro_usd == metric.estimated_charge_micro_usd);
        ensure!(row.charge_status == ChargeStatus::Computed.as_str());
        ensure!(
            row.router_id == metric.router_id
                && row.binding_digest == metric.binding_digest
                && row.original_selector == metric.original_selector
        );
        let evidence: bitrouter::metering::pricing::ChargeEvidence = serde_json::from_str(
            row.charge_evidence_json
                .as_deref()
                .context("charge evidence missing")?,
        )?;
        ensure!(
            evidence == metric.charge_evidence,
            "charge evidence did not round-trip"
        );
        let raw: serde_json::Value =
            serde_json::from_str(row.raw_usage_json.as_deref().context("raw usage missing")?)?;
        ensure!(
            Some(raw) == metric.raw_usage,
            "raw usage did not round-trip"
        );
    }
    Ok(())
}

async fn write_trajectory(connection: &DatabaseConnection, fixture: &Fixture) -> Result<()> {
    let store = TrajectoryStore::new(connection.clone());
    let owner = fixture.key("a").user_id;
    let other_owner = fixture.key("b").user_id;
    let digest = KeyedDigest::parse(format!(
        "hmac-sha256:test-key:{}",
        hash_key(&fixture.id("input"))
    ))?;
    let begin = BeginRequest {
        episode: EpisodeStart {
            episode_id: fixture.id("episode"),
            correlation_digest: digest.clone(),
            correlation_key_id: "test-key".into(),
            correlation_source: "explicit_root".into(),
            completeness: HistoryCompleteness::Complete,
        },
        event: fixture.event(1, TrajectoryEventKind::RequestStarted)?,
        full_input_digest: digest,
        native_parent_id: None,
        protocol: "responses".into(),
    };
    store.begin_request(&owner, begin.clone()).await?;
    store.begin_request(&owner, begin).await?;
    let started = store
        .request(&owner, &fixture.id("request-a1"))
        .await?
        .context("trajectory start missing")?;
    ensure!(started.status == RequestStatus::Started && started.settlement_event_id.is_none());
    ensure!(
        store
            .events_for_episode(&owner, &fixture.id("episode"))
            .await?
            .len()
            == 1
    );
    ensure!(store.pending_outbox(&owner).await?.is_empty());
    let route = fixture.event(2, TrajectoryEventKind::RouteIntentRecorded)?;
    store.append_route_intent(&owner, route.clone()).await?;
    store.append_route_intent(&owner, route).await?;
    ensure!(
        store
            .events_for_episode(&owner, &fixture.id("episode"))
            .await?
            .len()
            == 2
    );
    let event = fixture.event(3, TrajectoryEventKind::RequestSettled)?;
    let settlement = Settlement {
        outbox: Some(OutboxWrite {
            outbox_id: fixture.id("outbox"),
            topic: "trajectory.settled".into(),
            payload: OutboxPayload {
                structural: BTreeMap::from([("trajectory.event_count".into(), 3)]),
                digests: BTreeMap::from([(
                    "trajectory.event".into(),
                    event.content_digest.clone(),
                )]),
                evaluation: None,
            },
            created_at: fixture.captured_at.clone(),
        }),
        event,
        status: RequestStatus::Settled,
    };
    store.settle_request(&owner, settlement.clone()).await?;
    store.settle_request(&owner, settlement.clone()).await?;
    let before_conflict = read_trajectory(connection, fixture).await?;
    let mut conflict = settlement;
    conflict.status = RequestStatus::Failed;
    ensure!(
        store.settle_request(&owner, conflict).await.is_err(),
        "different settlement must not overwrite a settled request"
    );
    ensure!(
        store
            .request(&other_owner, &fixture.id("request-a1"))
            .await?
            .is_none()
    );
    ensure!(
        store
            .episode(&other_owner, &fixture.id("episode"))
            .await?
            .is_none()
    );
    ensure!(
        store
            .events_for_episode(&other_owner, &fixture.id("episode"))
            .await?
            .is_empty()
    );
    ensure!(store.pending_outbox(&other_owner).await?.is_empty());
    ensure!(
        store
            .mark_outbox_delivered(&other_owner, &fixture.id("outbox"), &fixture.captured_at)
            .await
            .is_err(),
        "another owner must not acknowledge the outbox"
    );
    ensure!(
        read_trajectory(connection, fixture).await? == before_conflict,
        "rejected settlement or cross-owner acknowledgement changed persisted records"
    );
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct TrajectoryRows {
    request: StoredRequest,
    episode: StoredEpisode,
    events: Vec<TrajectoryEvent>,
    outbox: Vec<PendingOutbox>,
}

async fn read_trajectory(
    connection: &DatabaseConnection,
    fixture: &Fixture,
) -> Result<TrajectoryRows> {
    let store = TrajectoryStore::new(connection.clone());
    let owner = fixture.key("a").user_id;
    Ok(TrajectoryRows {
        request: store
            .request(&owner, &fixture.id("request-a1"))
            .await?
            .context("trajectory request missing")?,
        episode: store
            .episode(&owner, &fixture.id("episode"))
            .await?
            .context("trajectory episode missing")?,
        events: store
            .events_for_episode(&owner, &fixture.id("episode"))
            .await?,
        outbox: store.pending_outbox(&owner).await?,
    })
}

fn check_trajectory(rows: &TrajectoryRows, fixture: &Fixture) -> Result<()> {
    ensure!(rows.request.status == RequestStatus::Settled);
    ensure!(rows.request.start_event_id == fixture.id("event-1"));
    ensure!(rows.request.settlement_event_id == Some(fixture.id("event-3")));
    ensure!(
        rows.episode.next_sequence == 4
            && rows.episode.completeness == HistoryCompleteness::Complete
    );
    ensure!(rows.episode.latest_request_id == Some(fixture.id("request-a1")));
    let expected = vec![
        fixture.event(1, TrajectoryEventKind::RequestStarted)?,
        fixture.event(2, TrajectoryEventKind::RouteIntentRecorded)?,
        fixture.event(3, TrajectoryEventKind::RequestSettled)?,
    ];
    ensure!(
        rows.events == expected,
        "trajectory event order or content changed"
    );
    ensure!(
        rows.outbox.len() == 1,
        "settlement must persist exactly one pending outbox"
    );
    let pending = rows.outbox.first().context("pending outbox missing")?;
    ensure!(
        pending.outbox_id == fixture.id("outbox")
            && pending.owner_user_id == fixture.key("a").user_id
    );
    ensure!(pending.topic == "trajectory.settled" && pending.attempts == 0);
    ensure!(pending.created_at == fixture.captured_at);
    ensure!(pending.payload.structural.get("trajectory.event_count") == Some(&3));
    ensure!(
        pending.payload.digests.get("trajectory.event")
            == Some(
                &fixture
                    .event(3, TrajectoryEventKind::RequestSettled)?
                    .content_digest
            )
    );
    ensure!(serde_json::from_str::<OutboxPayload>(&pending.payload_json)? == pending.payload);
    ensure!(canonical_digest(&pending.payload)? == pending.payload_digest);
    Ok(())
}
