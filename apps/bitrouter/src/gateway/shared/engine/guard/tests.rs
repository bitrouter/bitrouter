//! Engine guard integration against a fixture-owned PostgreSQL database.
//! These are authenticated engine calls, not stock-process acceptance tests.

#[path = "regression_tests.rs"]
mod regression_tests;

use std::{collections::BTreeMap, panic::AssertUnwindSafe, time::Duration};

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::execution::{ExecutionState, TerminalOutcome};
use bitrouter_ai::types::{ApiProtocol, GenerationParams, Message, ModelOperation, Prompt, Role};
use futures::FutureExt as _;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

use super::Input;
use crate::auth::db::{NewApiKey, insert_api_key, upsert_user};
use crate::gateway::routing::workflow::ir::RouteProjection;
use crate::gateway::shared::{
    authority::Authority,
    capture::Requirements,
    crypto::DeploymentKey,
    engine::{
        Admission, AdmissionResult, Engine, Execution, Outbox, TenantLimits, TrajectoryBinding,
    },
    schema,
    store::{Kind, Store, Stored},
    test_database::{EmptyDatabase, combine_results},
};
use crate::metering::shared::SharedMetering;
use crate::policy_lock::{PolicyDefinition, PolicyLock, semantic_digest};
use crate::trajectory::{
    canonical::Canonicalizer,
    correlation::{CorrelatedRequest, CorrelationSource},
    guard::{IncompleteHistoryAction, ProgressGuardPolicy},
    shared::SharedTrajectory,
    store::{GuardedRouteInput, GuardedRouteResult},
    types::{RequestStatus, TrajectoryEventKind},
};

mod handoff;

const AT: &str = "2026-01-01T00:00:00Z";
const LATER: &str = "2026-01-01T00:00:01Z";
const INJECTED: &str = "injected engine guard write failure";

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL with PostgreSQL CREATEDB privilege"]
async fn postgres_engine_guard_atomicity_epochs_and_catchup() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    // The database owner and registered pool clones outlive the timed campaign.
    // No detached tasks or processes are created by this fixture.
    let result = AssertUnwindSafe(tokio::time::timeout(Duration::from_secs(90), async {
        let fixture = Fixture::new(&mut database).await?;
        atomic_start(&fixture)
            .await
            .context("atomic guarded start")?;
        retained_epochs(&fixture)
            .await
            .context("retained guard epochs")?;
        missing_and_ambiguous(&fixture)
            .await
            .context("guard ancestry rejection")?;
        catchup_and_failed_delivery(&fixture)
            .await
            .context("guard catch-up and failure delivery")?;
        Ok::<(), anyhow::Error>(())
    }))
    .catch_unwind()
    .await;
    let result = match result {
        Ok(result) => result
            .context("engine guard campaign timed out")
            .and_then(|result| result),
        Err(payload) => {
            drop(payload);
            Err(anyhow::anyhow!("engine guard campaign panicked"))
        }
    };
    database.finish(result).await
}

struct Fixture {
    engine: Engine,
    observer: Store,
    authority: Authority,
    owner: String,
    principal: String,
    key_hash: String,
    requirements: Requirements,
    route: GuardedRouteInput,
    protocol: ApiProtocol,
}

impl Fixture {
    async fn new(database: &mut EmptyDatabase) -> Result<Self> {
        let db = database.connect(6, Duration::from_secs(10)).await?;
        let observer_db = database.connect(2, Duration::from_secs(10)).await?;
        let namespace = format!("guard-{}", uuid::Uuid::new_v4());
        let store = Store::new(db, &namespace, Duration::from_secs(10))?;
        let observer = Store::new(observer_db, &namespace, Duration::from_secs(10))?;
        schema::initialize_all(&store).await?;
        let authority = Authority::new(store, DeploymentKey::from_bytes(rand::random())?);
        authority.initialize().await?;
        let engine = Engine::join(authority.clone(), 180_000).await?;
        let owner = uuid::Uuid::new_v4().to_string();
        let principal = uuid::Uuid::new_v4().to_string();
        let key_hash = crate::auth::keys::hash_key(&uuid::Uuid::new_v4().to_string());
        upsert_user(authority.store().database(), &owner).await?;
        insert_api_key(
            authority.store().database(),
            &NewApiKey {
                id: principal.clone(),
                key_hash: key_hash.clone(),
                user_id: owner.clone(),
                spend_limit_micro_usd: None,
                rpm_limit: None,
                policy_id: None,
            },
        )
        .await?;
        engine
            .set_tenant(
                &owner,
                None,
                &TenantLimits {
                    concurrent_executions: 64,
                    requests_per_minute: 1000,
                    resource_pools: Default::default(),
                },
            )
            .await?;

        let config = crate::config::parse_with(
            "inherit_defaults: false\nregistry:\n  enabled: false\ntrajectory:\n  enabled: true\npresets:\n  coding:\n    model: fixture:cheap\n    policy: fixture-guard\n",
            |_| None,
        )?;
        let guard = ProgressGuardPolicy {
            escalation_tier: "strong".into(),
            protected_tiers: ["strong".into()].into(),
            max_consecutive_unprotected: None,
            max_same_projection_unprotected: None,
            max_recovery_count: None,
            max_episode_requests: Some(1),
            max_episode_elapsed_ms: None,
            max_episode_cost_micro_usd: None,
            hold_for_requests: 2,
            incomplete_history: IncompleteHistoryAction::Observe,
        };
        let definition = PolicyDefinition {
            tiers: BTreeMap::from([
                ("economy".into(), "fixture:cheap".into()),
                ("strong".into(), "fixture:strong".into()),
            ]),
            default_tier: Some("economy".into()),
            tool_use_tier: Some("strong".into()),
            tool_safe_tiers: vec!["strong".into()],
            progress_guard: Some(guard.clone()),
            ..Default::default()
        };
        let mut document = PolicyLock::default();
        document
            .policies
            .insert("fixture-guard".into(), definition.clone());
        // A second policy makes the admitted authority explicitly whole-document.
        document
            .policies
            .insert("unselected-policy".into(), definition);
        authority
            .publish(
                0,
                &crate::gateway::shared::publication::Configuration {
                    format: 1,
                    routing: serde_json::json!({
                        "trajectory": {"enabled": true},
                        "presets": {"coding": {"model": "fixture:cheap", "policy": "fixture-guard"}}
                    }),
                    policies: Vec::new(),
                    policy_lock: Some(document.clone()),
                    credentials: Vec::new(),
                    require_known_pricing: false,
                },
            )
            .await?;
        let digest = semantic_digest(&document)?;
        let requirements = Requirements::from_config(&config, Some(&document))?;
        let serialized = serde_json::to_value(&requirements)?;
        ensure!(serialized["trajectory"] == "guarded_policies");
        ensure!(serialized["policy_lock_digest"].as_str() == Some(digest.as_str()));
        let route = GuardedRouteInput {
            route_event_id: "replaced-by-engine".into(),
            guard_event_id: "replaced-by-engine".into(),
            policy_name: "fixture-guard".into(),
            route_projection: "agent_trace/v2|edit|normal".into(),
            request_key: "agent_trace/v2|edit|normal".into(),
            baseline_tier: Some("economy".into()),
            baseline_effort: None,
            tier_efforts: Default::default(),
            preset: Some("coding".into()),
            projection: RouteProjection::parse_key("agent_trace/v2|edit|normal")
                .context("fixture projection is invalid")?,
            candidate_tier: Some("economy".into()),
            policy_digest: digest,
            experiment: None,
            route_measurement: None,
            policy: guard,
            carries_tools: false,
            tool_use_tier: Some("strong".into()),
            tool_safe_tiers: ["strong".into()].into(),
        };
        Ok(Self {
            engine,
            observer,
            authority,
            owner,
            principal,
            key_hash,
            requirements,
            route,
            protocol: ApiProtocol::Responses,
        })
    }

    async fn admit(&self) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        match self
            .engine
            .admit_with_id(
                &id,
                &Admission {
                    requirements: self.requirements.clone(),
                    key_hash: &self.key_hash,
                    correlation_id: "guard-fixture-correlation",
                    idempotency_key: None,
                    fingerprint: &"d".repeat(64),
                    operation: ModelOperation::Generation,
                    revision: self.authority.head().await?.revision(),
                },
            )
            .await?
        {
            AdmissionResult::Accepted(view, authenticated) => {
                ensure!(view.execution_id == id && authenticated.api_key_id == self.principal);
                ensure!(authenticated.user_id == self.owner);
                Ok(id)
            }
            AdmissionResult::Existing(_) => {
                anyhow::bail!("fresh admission unexpectedly reused an execution")
            }
        }
    }

    fn input<'a>(&'a self, prompt: &'a Prompt, at: &'a str) -> Input<'a> {
        Input {
            owner: &self.owner,
            protocol: &self.protocol,
            prompt,
            captured_at: at,
            route: self.route.clone(),
        }
    }

    async fn route(
        &self,
        id: &str,
        prompt: &Prompt,
        at: &str,
    ) -> Result<(CorrelatedRequest, GuardedRouteResult)> {
        self.engine
            .guarded_route(&self.key_hash, id, &self.input(prompt, at))
            .await
    }

    async fn execution(&self, id: &str) -> Result<Stored<Execution>> {
        self.authority
            .store()
            .get(Kind::Execution, id)
            .await?
            .context("execution missing")
    }

    async fn binding(&self, id: &str) -> Result<TrajectoryBinding> {
        self.execution(id)
            .await?
            .value
            .trajectory
            .context("guard binding missing")
    }

    async fn snapshot(&self) -> Result<Vec<String>> {
        let tx = self.observer.begin().await?;
        let mut rows = Vec::new();
        for table in [
            "gateway_shared_records",
            "gateway_shared_trajectory_episodes",
            "gateway_shared_trajectory_requests",
            "gateway_shared_trajectory_events",
            "gateway_shared_trajectory_prefix_index",
            "gateway_shared_trajectory_outbox",
            "gateway_shared_metering_requests",
            "gateway_shared_eval_subjects",
            "gateway_shared_eval_results",
            "gateway_shared_eval_admission_events",
        ] {
            // Fixed fixture tables; every namespace is a bound value. Include
            // row revisions and metadata, not only event counts.
            // https://www.postgresql.org/docs/current/functions-aggregate.html
            let sql = format!(
                "SELECT COALESCE(jsonb_agg(to_jsonb(item) ORDER BY to_jsonb(item)::text), '[]'::jsonb)::text AS contents FROM {table} AS item WHERE namespace=$1"
            );
            let row = tx
                .query_one(Statement::from_sql_and_values(
                    DatabaseBackend::Postgres,
                    sql,
                    [self.authority.store().namespace().into()],
                ))
                .await?
                .context("snapshot missing")?;
            rows.push(row.try_get("", "contents")?);
        }
        tx.commit().await?;
        Ok(rows)
    }

    async fn fail_on(&self, kind: &'static str) -> Result<String> {
        ensure!(matches!(
            kind,
            "request_started" | "route_intent_recorded" | "guard_activated"
        ));
        let name = format!("guard_fail_{}", uuid::Uuid::new_v4().simple());
        // Generated identifiers and a fixed event-kind allowlist only. DDL is
        // confined to this test's owned physical database, never a shared schema.
        // https://www.postgresql.org/docs/current/plpgsql-trigger.html
        let sql = format!(
            "CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $body$ BEGIN IF NEW.kind='{kind}' THEN RAISE EXCEPTION '{INJECTED}'; END IF; RETURN NEW; END $body$"
        );
        self.authority
            .store()
            .database()
            .execute(Statement::from_string(DatabaseBackend::Postgres, sql))
            .await?;
        self.authority.store().database().execute(Statement::from_string(DatabaseBackend::Postgres,
            format!("CREATE TRIGGER {name} BEFORE INSERT ON gateway_shared_trajectory_events FOR EACH ROW EXECUTE FUNCTION {name}()"))).await?;
        Ok(name)
    }

    async fn remove_failure(&self, name: &str, result: Result<()>) -> Result<()> {
        let mut cleanup = Vec::new();
        for sql in [
            format!("DROP TRIGGER {name} ON gateway_shared_trajectory_events"),
            format!("DROP FUNCTION {name}()"),
        ] {
            cleanup.push(
                self.authority
                    .store()
                    .database()
                    .execute(Statement::from_string(DatabaseBackend::Postgres, sql))
                    .await
                    .map(|_| ())
                    .map_err(anyhow::Error::from),
            );
        }
        combine_results(result, cleanup)
    }
}

fn prompt(text: &str) -> Prompt {
    Prompt {
        model: "fixture:cheap".into(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![Message::text(Role::User, text)],
        tools: Vec::new(),
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    }
}

fn continued(original: &Prompt) -> Prompt {
    let mut value = original.clone();
    value
        .messages
        .push(Message::text(Role::Assistant, "fixture answer"));
    value
        .messages
        .push(Message::text(Role::User, "fixture follow-up"));
    value
}

fn rejected<T>(result: Result<T>, expected: &str) -> Result<()> {
    let error = result
        .err()
        .context("guard unexpectedly accepted invalid/faulted input")?;
    ensure!(
        error
            .chain()
            .any(|cause| cause.to_string().contains(expected)),
        "wrong guard rejection; expected {expected}, got {error:#}"
    );
    Ok(())
}

async fn atomic_start(f: &Fixture) -> Result<()> {
    for kind in [
        "request_started",
        "route_intent_recorded",
        "guard_activated",
    ] {
        let id = f.admit().await?;
        let p = prompt(&format!("atomic-{kind}"));
        let before = f.snapshot().await?;
        let trigger = f.fail_on(kind).await?;
        let failed = rejected(f.route(&id, &p, AT).await, INJECTED);
        f.remove_failure(&trigger, failed).await?;
        ensure!(
            f.snapshot().await? == before,
            "{kind} failure committed partial guard state"
        );
        ensure!(f.execution(&id).await?.value.trajectory.is_none());
        let (correlated, result) = f.route(&id, &p, AT).await?;
        ensure!(
            result.guard_activated,
            "guard-activation fault was not reachable"
        );
        let binding = f.binding(&id).await?;
        ensure!(
            binding.episode_id == correlated.episode_id
                && binding.request_id == correlated.request_id
        );
        let committed = f.snapshot().await?;
        ensure!(f.route(&id, &p, AT).await? == (correlated, result));
        ensure!(
            f.snapshot().await? == committed,
            "exact retry performed durable writes"
        );
        let mut wrong = f.input(&p, AT);
        wrong.route.policy_digest = format!("sha256:{}", "f".repeat(64));
        rejected(
            f.engine.guarded_route(&f.key_hash, &id, &wrong).await,
            "guard differs from original admitted contract",
        )?;
        ensure!(f.snapshot().await? == committed);
    }
    Ok(())
}

async fn retained_epochs(f: &Fixture) -> Result<()> {
    let original_key = f
        .authority
        .keys(&f.authority.head().await?)
        .await?
        .active_correlation()?;
    let original_epoch = original_key.key_id().to_owned();
    let original = Canonicalizer::new(original_key);
    let root = prompt("retained-epoch-root");
    let root_id = f.admit().await?;
    let first = f.route(&root_id, &root, AT).await?;
    ensure!(f.binding(&root_id).await?.correlation_epoch == original_epoch);
    let head = f.authority.head().await?;
    let rotated = f.authority.rotate_correlation(head.key_generation).await?;
    let active = f.authority.keys(&rotated).await?.active_correlation()?;
    ensure!(active.key_id() != original_epoch);
    let active = Canonicalizer::new(active);

    // Prefix correlation must search retained epochs, not just the active key.
    let child_id = f.admit().await?;
    let child = f.route(&child_id, &continued(&root), LATER).await?;
    ensure!(child.0.source == CorrelationSource::CanonicalPrefix);
    ensure!(child.0.episode_id == first.0.episode_id);
    ensure!(child.0.request_id == original.request_identity(&f.owner, &child_id)?);
    ensure!(child.0.request_id != active.request_identity(&f.owner, &child_id)?);
    ensure!(f.binding(&child_id).await?.correlation_epoch == original_epoch);

    // Native continuation uses the public gateway ID codec and the same old key.
    let native_id = f.admit().await?;
    let mut native = prompt("native-only-follow-up");
    native.params.extra.insert(
        "previous_response_id".into(),
        crate::gateway::model::protocol::responses::encode_gateway_continuation_id(&root_id)?
            .into(),
    );
    let native_result = f.route(&native_id, &native, LATER).await?;
    ensure!(native_result.0.source == CorrelationSource::NativeParentId);
    ensure!(native_result.0.episode_id == first.0.episode_id);
    ensure!(f.binding(&native_id).await?.correlation_epoch == original_epoch);
    ensure!(native_result.0.request_id == original.request_identity(&f.owner, &native_id)?);

    // A later episode head and a rotated authority must not re-run the old guard.
    let later = f.snapshot().await?;
    ensure!(f.route(&root_id, &root, AT).await? == first);
    ensure!(f.snapshot().await? == later);
    let fresh_id = f.admit().await?;
    let fresh = f
        .route(&fresh_id, &prompt("new-active-epoch-root"), AT)
        .await?;
    ensure!(fresh.0.request_id == active.request_identity(&f.owner, &fresh_id)?);
    ensure!(fresh.0.episode_id != first.0.episode_id);
    Ok(())
}

async fn missing_and_ambiguous(f: &Fixture) -> Result<()> {
    let mut missing_native = prompt("missing-native");
    missing_native
        .params
        .extra
        .insert("previous_response_id".into(), "absent-parent".into());
    for (p, expected) in [
        (
            continued(&prompt("never-started-prefix")),
            "guard ancestry is unresolved",
        ),
        (missing_native, "guard native parent is missing"),
    ] {
        let id = f.admit().await?;
        let before = f.snapshot().await?;
        rejected(f.route(&id, &p, AT).await, expected)?;
        ensure!(f.snapshot().await? == before);
        ensure!(f.execution(&id).await?.value.trajectory.is_none());
    }
    let root = prompt("ambiguous-prefix");
    let a = f.admit().await?;
    let b = f.admit().await?;
    let first = f.route(&a, &root, AT).await?;
    let second = f.route(&b, &root, AT).await?;
    ensure!(first.0.episode_id != second.0.episode_id);
    let child = f.admit().await?;
    let before = f.snapshot().await?;
    rejected(
        f.route(&child, &continued(&root), LATER).await,
        "guard prefix is ambiguous",
    )?;
    ensure!(f.snapshot().await? == before);
    Ok(())
}

async fn route_after_fence_wait(
    f: &Fixture,
    binding: &TrajectoryBinding,
    id: &str,
    p: &Prompt,
) -> Result<(CorrelatedRequest, GuardedRouteResult)> {
    let tx = f.observer.begin().await?;
    f.authority.fence(&tx).await?;
    f.engine
        .episode_fence(&tx, binding, Store::now(&tx).await?)
        .await?;
    let pid: i32 = tx
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT pg_backend_pid() AS pid".to_owned(),
        ))
        .await?
        .context("blocker PID missing")?
        .try_get("", "pid")?;
    let routing = f.route(id, p, LATER);
    tokio::pin!(routing);
    let observed = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            tokio::select! {
                _ = &mut routing => anyhow::bail!("guard completed before its episode fence was released"),
                blocked = async {
                    // Observe this owned database's exact blocker, not a sleep.
                    // https://www.postgresql.org/docs/current/functions-info.html#FUNCTIONS-INFO-SESSION
                    let row = f.observer.database().query_one(Statement::from_sql_and_values(
                        DatabaseBackend::Postgres,
                        "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid))) AS blocked",
                        [pid.into()],
                    )).await?.context("lock-wait observation missing")?;
                    let blocked = row.try_get::<bool>("", "blocked")?;
                    if !blocked { tokio::time::sleep(Duration::from_millis(5)).await; }
                    Ok::<_, anyhow::Error>(blocked)
                } => if blocked? { return Ok::<(), anyhow::Error>(()); },
            }
        }
    }).await.context("guard never waited on the episode fence").and_then(|result| result);
    // Release outside the observation timeout even if its assertion failed.
    let released = tx.rollback().await.context("release episode fixture fence");
    combine_results(observed, vec![released])?;
    routing.await
}

async fn catchup_and_failed_delivery(f: &Fixture) -> Result<()> {
    let root_prompt = prompt("failed-before-dispatch");
    let root_id = f.admit().await?;
    let root = f.route(&root_id, &root_prompt, AT).await?;
    let binding = f.binding(&root_id).await?;
    let original = f.execution(&root_id).await?;
    ensure!(original.value.view.attempts.is_empty());
    let capture =
        serde_json::to_value(original.value.projection.context("guard capture missing")?)?;
    ensure!(capture["policy"]["state"] == "pending");
    let failure = TerminalOutcome::Failed {
        code: "continuation_conflict".into(),
    };
    f.engine
        .finish_with_inputs(&root_id, &failure, None)
        .await?;
    let event_id = format!("gateway-terminal-{root_id}");
    let event: Stored<Outbox> = f
        .authority
        .store()
        .get(Kind::Outbox, &event_id)
        .await?
        .context("terminal outbox missing")?;
    ensure!(event.value.event.execution.attempts.is_empty());
    ensure!(matches!(&event.value.event.execution.state,
        ExecutionState::Terminal { result } if result.outcome == failure));
    let child_id = f.admit().await?;
    let child_prompt = continued(&root_prompt);
    let before = f.snapshot().await?;
    let trigger = f.fail_on("route_intent_recorded").await?;
    let failed = rejected(
        route_after_fence_wait(f, &binding, &child_id, &child_prompt).await,
        INJECTED,
    );
    f.remove_failure(&trigger, failed).await?;
    ensure!(
        f.snapshot().await? == before,
        "new guard failure committed catch-up, pending clear, binding, or start"
    );
    ensure!(f.execution(&child_id).await?.value.trajectory.is_none());

    let next = f.route(&child_id, &child_prompt, LATER).await?;
    ensure!(next.0.episode_id == root.0.episode_id);
    let settlement = next
        .0
        .prior_events
        .iter()
        .find(|event| {
            event.kind == TrajectoryEventKind::RequestSettled
                && event.request_id.as_deref() == Some(binding.request_id.as_str())
        })
        .context("guard did not read caught-up settlement")?;
    ensure!(settlement.sequence < next.1.route_sequence);
    ensure!(next.1.snapshot.health.settled_request_count == 1);
    let receipt: Stored<serde_json::Value> = f
        .authority
        .store()
        .get(Kind::ProjectionReceipt, &event_id)
        .await?
        .context("catch-up receipt missing")?;
    ensure!(receipt.status() == "projected");
    let tx = f.observer.begin().await?;
    f.authority.fence(&tx).await?;
    let fence = f
        .engine
        .episode_fence(&tx, &binding, Store::now(&tx).await?)
        .await?;
    ensure!(fence.value.pending.is_empty());
    tx.rollback().await?;

    // Complete the second projection phase from original Pending capture and
    // zero attempts. This is a definite failure, never a fabricated success.
    f.engine.deliver(&event.value.event).await?;
    let delivered = f.snapshot().await?;
    f.engine.deliver(&event.value.event).await?;
    ensure!(
        f.snapshot().await? == delivered,
        "delivered failure replay wrote new state"
    );
    let tx = f.observer.begin().await?;
    let record = SharedMetering::new(f.authority.store().namespace())?
        .get_in(&tx, &f.owner, &f.principal, &root_id)
        .await?
        .context("failed metering projection missing")?;
    ensure!(record.attempts.is_empty() && record.event.execution.attempts.is_empty());
    ensure!(matches!(&record.event.execution.state,
        ExecutionState::Terminal { result } if result.outcome == failure));
    let trajectory = SharedTrajectory::new(f.authority.store().namespace())?;
    let request = trajectory
        .request_in(&tx, &f.owner, &binding.request_id)
        .await?
        .context("failed request missing")?;
    ensure!(request.status == RequestStatus::Failed);
    let history = trajectory
        .history_in(&tx, &f.owner, &binding.episode_id)
        .await?;
    ensure!(
        history
            .iter()
            .filter(|event| event.kind == TrajectoryEventKind::RequestSettled
                && event.request_id.as_deref() == Some(binding.request_id.as_str()))
            .count()
            == 1
    );
    tx.commit().await?;
    let receipt: Stored<serde_json::Value> = f
        .authority
        .store()
        .get(Kind::ProjectionReceipt, &event_id)
        .await?
        .context("delivered receipt missing")?;
    ensure!(receipt.status() == "delivered");
    Ok(())
}
