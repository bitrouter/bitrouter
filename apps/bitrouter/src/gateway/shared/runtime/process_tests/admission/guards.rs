//! Stock-process candidate for the counter/hold/epoch portion of P1-A06.
//!
//! All mutations use official bootstrap, inference HTTP, or authenticated
//! OperatorClient. Store access below only reads authority and immutable facts.
//! A separate priced campaign checks cost thresholds. P1 gates remain pending.

#[path = "guards/cost.rs"]
mod cost;

use std::collections::{BTreeMap, BTreeSet};

use bitrouter_ai::eval::{AdmissionStatus, EvalVerdict};

use super::*;
use crate::gateway::shared::{engine::TrajectoryBinding, store::Kind};
use crate::policy_lock::{PolicyDefinition, PolicyLock};
use crate::storage::evaluation::shared::SharedEvaluation;
use crate::trajectory::{
    canonical::Canonicalizer,
    evaluation::decode_eval_decision,
    guard::{
        IncompleteHistoryAction, ProgressGuardPolicy, RouteIntentClauseDisposition,
        validate_persisted_route_intent,
    },
    health::reduce,
    shared::SharedTrajectory,
    types::{StoredEpisode, TrajectoryEvent, TrajectoryEventKind},
};

const POLICY: &str = "fixture-process-guard";
const PRESET: &str = "guarded";
const ROOT: &str = "Describe the fixture response.";
const OWNER: &str = "limited";

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_guard_history_holds_and_correlation_epochs_cross_replicas() -> Result<()> {
    run(Campaign::Guards).await
}

pub(super) fn seed_plan(base: &str, keys: &[crate::auth::keys::GeneratedKey; 4]) -> Result<Value> {
    let mut plan = super::seed_plan(base, keys);
    let routing = &mut plan["configuration"]["routing"];
    routing["trajectory"] = json!({"enabled": true});
    routing["presets"] = json!({PRESET: {"model": "tenant:cheap", "policy": POLICY}});
    routing["providers"]["tenant"]["models"] = json!([{"id": "cheap"}]);
    routing["providers"]["account-a"]["models"] = json!([{"id": "strong"}]);
    let guard = ProgressGuardPolicy {
        escalation_tier: "strong".into(),
        protected_tiers: BTreeSet::from(["strong".into()]),
        max_consecutive_unprotected: Some(2),
        max_same_projection_unprotected: None,
        max_recovery_count: None,
        max_episode_requests: None,
        max_episode_elapsed_ms: None,
        max_episode_cost_micro_usd: None,
        hold_for_requests: 2,
        incomplete_history: IncompleteHistoryAction::Observe,
    };
    let mut lock = PolicyLock::default();
    lock.policies.insert(
        POLICY.into(),
        PolicyDefinition {
            tiers: BTreeMap::from([
                ("economy".into(), "tenant:cheap".into()),
                ("strong".into(), "account-a:strong".into()),
            ]),
            default_tier: Some("economy".into()),
            tool_use_tier: Some("strong".into()),
            tool_safe_tiers: vec!["strong".into()],
            progress_guard: Some(guard),
            ..Default::default()
        },
    );
    plan["configuration"]["policy_lock"] = serde_json::to_value(lock)?;
    // Keep the real seed's tenant/account concurrency at eight. The A01 helper
    // deliberately reduces them to one and must not run for this campaign.
    Ok(plan)
}

pub(super) fn cost_seed_plan(
    base: &str,
    keys: &[crate::auth::keys::GeneratedKey; 4],
) -> Result<Value> {
    cost::seed_plan(base, keys)
}

pub(super) async fn cost_campaign(
    fixture: &Fixture,
    world: &World,
    mock: &mut Mock,
    jobs: &mut Requests,
) -> Result<()> {
    cost::campaign(fixture, world, mock, jobs).await
}

pub(super) async fn campaign(
    fixture: &Fixture,
    world: &World,
    mock: &mut Mock,
    jobs: &mut Requests,
) -> Result<()> {
    let head = fixture.authority.head().await?;
    let key = fixture.authority.keys(&head).await?.active_correlation()?;
    let old_epoch = key.key_id().to_owned();
    let old = Canonicalizer::new(key);
    let root_messages = messages(ROOT, None);
    let root = one(
        fixture,
        world,
        mock,
        jobs,
        &RequestCase {
            replica: 0,
            principal: 0,
            label: "guard-root",
            messages: root_messages.clone(),
            tier: "economy",
            clause: None,
        },
    )
    .await?;
    binding_key(&root, OWNER, &old_epoch, &old)?;
    let initial = episode(fixture, &root.binding).await?;
    counts(&initial, 1, 1, 0, 0, 1)?;
    replay(fixture, world, mock, &root, &root_messages, "guard-root").await?;

    // Start both requests before awaiting either provider arrival. Different
    // principals share the tenant; separate replicas must serialize one history.
    for (replica, principal, label, follow_up) in [
        (0, 0, "guard-overlap-a", "Describe branch A."),
        (1, 1, "guard-overlap-b", "Describe branch B."),
    ] {
        start(
            jobs,
            label,
            post(
                world,
                replica,
                principal,
                label,
                &messages(ROOT, Some(follow_up)),
            ),
        );
    }
    let a = mock.arrival(ROOT, "account", SECRETS[1]).await?;
    let b = mock.arrival(ROOT, "account", SECRETS[1]).await?;
    ensure!(a.model == "strong" && b.model == "strong");
    ensure!(a.execution != b.execution);
    let pending_a = execution(fixture, &a.execution).await?;
    let pending_b = execution(fixture, &b.execution).await?;
    ensure!(
        pending_a["owner"] != pending_b["owner"],
        "requests did not use two incarnations"
    );
    for (arrival, stored) in [(&a, &pending_a), (&b, &pending_b)] {
        let bound = binding(stored)?;
        ensure!(bound.episode_id == root.binding.episode_id);
        ensure!(bound.owner_user_id == OWNER && bound.correlation_epoch == old_epoch);
        ensure!(bound.request_id == old.request_identity(OWNER, &arrival.execution)?);
        let public = world.status(1, 0, &arrival.execution).await?;
        public.expect_status(StatusCode::OK)?;
        ensure!(matches!(public.execution()?.state, ExecutionState::Running));
    }
    let overlapping = episode(fixture, &root.binding).await?;
    ensure!(overlapping.events.starts_with(&initial.events));
    counts(&overlapping, 3, 1, 2, 1, 0)?;
    ensure!(activations(&overlapping) == 1);
    let routes = overlapping
        .events
        .iter()
        .filter(|event| event.kind == TrajectoryEventKind::RouteIntentRecorded)
        .collect::<Vec<_>>();
    ensure!(routes.len() == 3);
    route(
        routes.get(1).context("missing threshold route")?,
        "strong",
        Some("progress_guard.max_consecutive_unprotected"),
    )?;
    route(
        routes.get(2).context("missing held route")?,
        "strong",
        Some("progress_guard.active_hold"),
    )?;
    occupancy(fixture, OWNER, Kind::Tenant, 2).await?;
    occupancy(fixture, ACCOUNT_POOL, Kind::Resource, 2).await?;

    // Rotate through the authenticated control listener while both real provider
    // responses remain blocked. No key/episode state is written by this test.
    let revision = world.operators[0].revision().await?;
    let mutation = Mutation {
        operation_id: "guard-rotate-epoch".into(),
        command: MutationCommand::RotateCorrelation {
            expected_generation: revision.key_generation,
        },
    };
    let receipt = world.operators[0].apply(&mutation).await?;
    ensure!(
        matches!(world.operators[1].operation(&mutation.operation_id).await?,
        OperationLookup::Committed { receipt: other } if *other == receipt)
    );
    let advanced = world.operators[1].revision().await?;
    ensure!(advanced.key_generation == revision.key_generation + 1);
    ensure!(advanced.config_generation == revision.config_generation);
    let next_head = fixture.authority.head().await?;
    let next_key = fixture
        .authority
        .keys(&next_head)
        .await?
        .active_correlation()?;
    let new_epoch = next_key.key_id().to_owned();
    ensure!(new_epoch != old_epoch);
    let new = Canonicalizer::new(next_key);
    ensure!(
        episode(fixture, &root.binding).await? == overlapping,
        "rotation mutated the old episode before either provider finished"
    );
    ensure!(execution(fixture, &a.execution).await? == pending_a);
    ensure!(execution(fixture, &b.execution).await? == pending_b);

    // Release in reverse arrival order, then join both clients before polling
    // projection. No worker wait prolongs a held inference beyond its deadline.
    let second_id = b.execution.clone();
    let first_id = a.execution.clone();
    b.release(Reply::Success)?;
    a.release(Reply::Success)?;
    let mut remaining = BTreeSet::from([first_id.as_str(), second_id.as_str()]);
    for _ in 0..2 {
        let (_, wire) = completed_any(jobs).await?;
        let id = wire.header("x-bitrouter-execution-id")?;
        ensure!(
            remaining.remove(id),
            "unexpected or duplicate overlap response"
        );
        successful_wire(&wire, id)?;
    }
    ensure!(remaining.is_empty());
    let first = delivered(fixture, world, &first_id, 0).await?;
    let second = delivered(fixture, world, &second_id, 0).await?;
    for done in [&first, &second] {
        binding_key(done, OWNER, &old_epoch, &old)?;
        ensure!(done.binding.episode_id == root.binding.episode_id);
    }
    counts(&episode(fixture, &root.binding).await?, 3, 3, 0, 1, 0)?;

    // Prefix correlation must retain the original epoch after rotation. The
    // next request consumes the final hold; the following request is cheap.
    for (replica, label, tier, clause, count, unprotected) in [
        (
            1,
            "guard-last-hold",
            "strong",
            Some("progress_guard.active_hold"),
            4,
            0,
        ),
        (0, "guard-expired-hold", "economy", None, 5, 1),
    ] {
        let done = one_after_rotation(
            fixture,
            world,
            mock,
            jobs,
            &RequestCase {
                replica,
                principal: 0,
                label,
                messages: messages(ROOT, Some(label)),
                tier,
                clause,
            },
        )
        .await?;
        binding_key(&done, OWNER, &old_epoch, &old)?;
        ensure!(done.binding.episode_id == root.binding.episode_id);
        ensure!(done.binding.request_id != new.request_identity(OWNER, &done.id)?);
        let current = episode(fixture, &root.binding).await?;
        counts(&current, count, count, 0, 0, unprotected)?;
        ensure!(
            activations(&current) == 1,
            "hold incorrectly reset or reactivated"
        );
    }
    let old_final = episode(fixture, &root.binding).await?;
    let fresh = one(
        fixture,
        world,
        mock,
        jobs,
        &RequestCase {
            replica: 1,
            principal: 1,
            label: "guard-new-epoch-root",
            messages: messages("Describe a fresh independent topic.", None),
            tier: "economy",
            clause: None,
        },
    )
    .await?;
    binding_key(&fresh, OWNER, &new_epoch, &new)?;
    ensure!(fresh.binding.episode_id != root.binding.episode_id);
    counts(&episode(fixture, &fresh.binding).await?, 1, 1, 0, 0, 1)?;

    // A different tenant cannot inherit the existing prefix or read its status.
    let before = mock.calls();
    let denied = Wire::read(
        post(
            world,
            1,
            2,
            "guard-foreign-continuation",
            &messages(ROOT, Some("Describe the foreign continuation.")),
        )
        .send()
        .await?,
    )
    .await?;
    denied.expect_status(StatusCode::SERVICE_UNAVAILABLE)?;
    ensure!(
        serde_json::from_slice::<Value>(&denied.body)?["error"]["code"] == "upstream_unavailable"
    );
    let denied_id = denied.header("x-bitrouter-execution-id")?;
    let denied_view = world
        .wait_state(1, 2, denied_id, |state| {
            matches!(state, ExecutionState::Terminal { .. })
        })
        .await?;
    ensure!(
        matches!(&denied_view.state, ExecutionState::Terminal { result }
        if matches!(&result.outcome, bitrouter_ai::gateway::execution::TerminalOutcome::Failed { code }
            if code == "pipeline_failed"))
    );
    ensure!(denied_view.attempts.is_empty());
    ensure!(execution(fixture, denied_id).await?["trajectory"].is_null());
    // The public envelope intentionally hides the internal ancestry diagnostic.
    // This proves admitted failure without binding/dispatch; the engine tests
    // separately identify missing ancestry as the rejection cause.
    ensure!(
        mock.calls() == before,
        "foreign continuation dispatched a provider"
    );
    world
        .status(1, 2, &root.id)
        .await?
        .expect_status(StatusCode::NOT_FOUND)?;
    let isolated = one(
        fixture,
        world,
        mock,
        jobs,
        &RequestCase {
            replica: 0,
            principal: 2,
            label: "guard-foreign-root",
            messages: root_messages,
            tier: "economy",
            clause: None,
        },
    )
    .await?;
    binding_key(&isolated, "tenant-x", &new_epoch, &new)?;
    ensure!(isolated.binding.episode_id != root.binding.episode_id);
    ensure!(isolated.binding.request_id != new.request_identity(OWNER, &isolated.id)?);
    counts(&episode(fixture, &isolated.binding).await?, 1, 1, 0, 0, 1)?;
    ensure!(episode(fixture, &root.binding).await? == old_final);
    // Exact public replay after rotation must not recompute the original route,
    // response observation, projection receipt, or episode event sequence.
    replay(
        fixture,
        world,
        mock,
        &root,
        &messages(ROOT, None),
        "guard-root",
    )
    .await?;
    ensure!(episode(fixture, &root.binding).await? == old_final);
    ensure!(
        mock.calls() == 7,
        "unexpected duplicate or denied provider request"
    );
    for (kind, id) in [
        (Kind::Tenant, OWNER),
        (Kind::Tenant, "tenant-x"),
        (Kind::Resource, TENANT_POOL),
        (Kind::Resource, ACCOUNT_POOL),
    ] {
        occupancy(fixture, id, kind, 0).await?;
    }
    Ok(())
}

struct RequestCase<'a> {
    replica: usize,
    principal: usize,
    label: &'a str,
    messages: Value,
    tier: &'a str,
    clause: Option<&'a str>,
}

fn messages(root: &str, follow_up: Option<&str>) -> Value {
    let mut result = vec![json!({"role": "user", "content": root})];
    if let Some(follow_up) = follow_up {
        result.extend([
            json!({"role": "assistant", "content": "ok"}),
            json!({"role": "user", "content": follow_up}),
        ]);
    }
    Value::Array(result)
}

fn post(
    world: &World,
    replica: usize,
    principal: usize,
    label: &str,
    messages: &Value,
) -> reqwest::RequestBuilder {
    // OpenAI Chat Completions request shape:
    // https://platform.openai.com/docs/api-reference/chat/create
    world
        .http
        .post(format!(
            "http://{}/v1/chat/completions",
            world.replicas[replica]
        ))
        .bearer_auth(&world.keys[principal].secret)
        .header("idempotency-key", label)
        .header("x-bitrouter-request-id", label)
        .json(&json!({"model": format!("@{PRESET}"), "messages": messages}))
}

async fn one(
    fixture: &Fixture,
    world: &World,
    mock: &mut Mock,
    jobs: &mut Requests,
    case: &RequestCase<'_>,
) -> Result<Delivered> {
    one_request(fixture, world, mock, jobs, case, false).await
}

async fn one_after_rotation(
    fixture: &Fixture,
    world: &World,
    mock: &mut Mock,
    jobs: &mut Requests,
    case: &RequestCase<'_>,
) -> Result<Delivered> {
    one_request(fixture, world, mock, jobs, case, true).await
}

async fn one_request(
    fixture: &Fixture,
    world: &World,
    mock: &mut Mock,
    jobs: &mut Requests,
    case: &RequestCase<'_>,
    converging: bool,
) -> Result<Delivered> {
    let tag = case.messages[0]["content"]
        .as_str()
        .context("missing root message")?;
    let (account, secret, model) = if case.tier == "strong" {
        ("account", SECRETS[1], "strong")
    } else {
        ("tenant", SECRETS[0], "cheap")
    };
    let arrival = tokio::time::timeout(Duration::from_secs(10), async {
        for attempt in 1..=64 {
            let calls = mock.calls();
            start(jobs, case.label, post(world, case.replica, case.principal, case.label, &case.messages));
            if !converging {
                return mock.arrival(tag, account, secret).await;
            }
            // Only a definite pre-admission rejection may be resubmitted.
            // Preserve the exact request and idempotency key across publication.
            tokio::select! {
                arrival = mock.arrival(tag, account, secret) => return arrival,
                wire = completed(jobs, case.label) => {
                    let wire = wire?;
                    wire.expect_status(StatusCode::SERVICE_UNAVAILABLE)?;
                    for header in ["x-bitrouter-execution-id", "x-bitrouter-request-id", "location"] {
                        ensure!(!wire.headers.contains_key(header), "publication rejection has an admitted/uncertain identity");
                    }
                    ensure!(mock.calls() == calls, "publication rejection dispatched upstream");
                    eprintln!("{}: definite pre-admission publication rejection {attempt}", case.label);
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        anyhow::bail!("rotation publication did not converge");
    }).await.context("rotation publication deadline exceeded")??;
    ensure!(
        arrival.model == model,
        "HTTP provider model differs from selected treatment"
    );
    let id = arrival.execution.clone();
    let stored = execution(fixture, &id).await?;
    let bound = binding(&stored)?;
    let history = episode(fixture, &bound).await?;
    route(
        request_event(&history, &bound, TrajectoryEventKind::RouteIntentRecorded)?,
        case.tier,
        case.clause,
    )?;
    arrival.release(Reply::Success)?;
    let wire = completed(jobs, case.label).await?;
    successful_wire(&wire, &id)?;
    delivered(fixture, world, &id, case.principal).await
}

fn successful_wire(wire: &Wire, id: &str) -> Result<()> {
    wire.expect_status(StatusCode::OK)?;
    ensure!(wire.header("x-bitrouter-execution-id")? == id);
    let body: Value = serde_json::from_slice(&wire.body)?;
    ensure!(body["choices"][0]["message"]["content"] == "ok");
    ensure!(body["choices"][0]["finish_reason"] == "stop");
    Ok(())
}

async fn execution(fixture: &Fixture, id: &str) -> Result<Value> {
    Ok(fixture
        .authority
        .store()
        .get::<Value>(Kind::Execution, id)
        .await?
        .context("missing execution authority row")?
        .value)
}

fn binding(execution: &Value) -> Result<TrajectoryBinding> {
    serde_json::from_value(execution["trajectory"].clone()).context("missing execution binding")
}

#[derive(PartialEq, Eq)]
struct Episode {
    head: StoredEpisode,
    events: Vec<TrajectoryEvent>,
}

async fn episode(fixture: &Fixture, bound: &TrajectoryBinding) -> Result<Episode> {
    let store = fixture.authority.store();
    store
        .retry(|| async {
            let tx = store.begin().await?;
            let trajectory = SharedTrajectory::new(store.namespace())?;
            let head = trajectory
                .episode_in(&tx, &bound.owner_user_id, &bound.episode_id)
                .await?
                .context("missing bound episode")?;
            let events = trajectory
                .history_in(&tx, &bound.owner_user_id, &bound.episode_id)
                .await?;
            ensure!(head.owner_user_id == bound.owner_user_id);
            ensure!(head.correlation_key_id == bound.correlation_epoch);
            // history_in validates canonical ordering, IDs, digests and the head.
            tx.commit().await?;
            Ok(Episode { head, events })
        })
        .await
}

fn request_event<'a>(
    history: &'a Episode,
    bound: &TrajectoryBinding,
    kind: TrajectoryEventKind,
) -> Result<&'a TrajectoryEvent> {
    let mut events = history.events.iter().filter(|event| {
        event.kind == kind && event.request_id.as_deref() == Some(bound.request_id.as_str())
    });
    let event = events.next().context("missing request event")?;
    ensure!(events.next().is_none(), "duplicate request event");
    Ok(event)
}

fn counts(
    history: &Episode,
    requests: u64,
    settled: u64,
    pending: u64,
    hold: u64,
    unprotected: u64,
) -> Result<()> {
    let snapshot = reduce(&history.events, &BTreeSet::from(["strong".into()]))?;
    ensure!(snapshot.health.request_count == requests);
    ensure!(snapshot.health.settled_request_count == settled);
    ensure!(snapshot.health.unsettled_request_count == pending);
    ensure!(snapshot.active_hold_remaining == hold);
    ensure!(snapshot.health.consecutive_unprotected_requests == unprotected);
    // These unpriced mocks provide tokens, not cost or quality evidence.
    ensure!(snapshot.health.settled_cost_micro_usd.is_none());
    ensure!(snapshot.health.total_tokens == Some(settled * 10));
    Ok(())
}

fn activations(history: &Episode) -> usize {
    history
        .events
        .iter()
        .filter(|event| event.kind == TrajectoryEventKind::GuardActivated)
        .count()
}

fn route(event: &TrajectoryEvent, tier: &str, clause: Option<&str>) -> Result<()> {
    ensure!(
        event
            .evidence
            .categorical
            .get("route.selected_tier")
            .map(String::as_str)
            == Some(tier)
    );
    ensure!(
        event
            .evidence
            .categorical
            .get("route.policy")
            .map(String::as_str)
            == Some(POLICY)
    );
    let clauses = validate_persisted_route_intent(event)?.context("route is not guarded")?;
    let applied = clauses
        .iter()
        .filter(|(_, disposition)| *disposition == RouteIntentClauseDisposition::Applied)
        .map(|(id, _)| id.as_str())
        .collect::<Vec<_>>();
    match clause {
        Some(clause) => ensure!(
            applied.as_slice() == [clause],
            "unexpected applied clauses: {applied:?}"
        ),
        None => ensure!(
            applied.is_empty(),
            "unexpected guard activation: {applied:?}"
        ),
    }
    Ok(())
}

async fn occupancy(fixture: &Fixture, id: &str, kind: Kind, expected: u64) -> Result<()> {
    let row = fixture
        .authority
        .store()
        .get::<Value>(kind, id)
        .await?
        .context("missing occupancy row")?;
    ensure!(
        row.value["active"].as_u64() == Some(expected),
        "unexpected active capacity for {id}"
    );
    Ok(())
}

struct Delivered {
    id: String,
    binding: TrajectoryBinding,
    inputs: Value,
    receipt: Value,
    receipt_revision: u64,
}

fn binding_key(done: &Delivered, owner: &str, epoch: &str, key: &Canonicalizer) -> Result<()> {
    ensure!(done.binding.owner_user_id == owner);
    ensure!(done.binding.correlation_epoch == epoch);
    ensure!(done.binding.request_id == key.request_identity(owner, &done.id)?);
    Ok(())
}

async fn delivered(
    fixture: &Fixture,
    world: &World,
    id: &str,
    principal: usize,
) -> Result<Delivered> {
    let public = world.wait_state(1, principal, id, succeeded).await?;
    let peer = world.status(0, principal, id).await?;
    peer.expect_status(StatusCode::OK)?;
    same_facts(&peer.execution()?, &public)?;
    let store = fixture.authority.store();
    let event_id = format!("gateway-terminal-{id}");
    // Only the official serving processes may project or acknowledge this event.
    let receipt = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(receipt) = store
                .get::<Value>(Kind::ProjectionReceipt, &event_id)
                .await?
                && receipt.status() == "delivered"
            {
                return Ok::<_, anyhow::Error>(receipt);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("stock worker did not deliver guard projection")??;
    let stored = execution(fixture, id).await?;
    let bound = binding(&stored)?;
    let inputs = stored["projection"].clone();
    ensure!(inputs["phase"]["state"] == "terminal" && inputs["phase"]["route_ready"] == true);
    ensure!(
        inputs["response_observation"]
            == json!({
                "observed_action": "answer_or_summarize", "confidence_ppm": 850000,
                "reason_code": "canonical_response_signal"
            }),
        "real canonical mock content lost its bounded response observation"
    );
    ensure!(
        serde_json::to_vec(&json!({"response_observation": inputs["response_observation"]}))?.len()
            <= 256
    );
    let history = episode(fixture, &bound).await?;
    let route = request_event(&history, &bound, TrajectoryEventKind::RouteIntentRecorded)?;
    let decision = decode_eval_decision(route)?.context("missing persisted treatment")?;
    ensure!(
        inputs["policy"]["treatment"]["decision"] == serde_json::to_value(&decision)?,
        "captured selected treatment differs from the committed guarded route"
    );
    let settled = request_event(&history, &bound, TrajectoryEventKind::RequestSettled)?;
    ensure!(
        settled
            .evidence
            .categorical
            .get("settlement.outcome")
            .map(String::as_str)
            == Some("settled")
    );
    ensure!(
        settled
            .evidence
            .categorical
            .get("routing.observed_action")
            .map(String::as_str)
            == Some("answer_or_summarize")
    );
    ensure!(
        settled
            .evidence
            .categorical
            .get("routing.observation_reason_code")
            .map(String::as_str)
            == Some("canonical_response_signal")
    );
    ensure!(
        settled
            .evidence
            .structural
            .get("routing.observation_confidence_ppm")
            == Some(&850000)
    );
    ensure!(receipt.value["projection"]["binding"] == serde_json::to_value(&bound)?);
    ensure!(receipt.value["projection"]["evaluation"]["sink"] == "trajectory");
    ensure!(receipt.value["completion"]["sink"] == "trajectory");
    let reference = &receipt.value["projection"]["evaluation"]["reference"];
    store
        .retry(|| async {
            let tx = store.begin().await?;
            let evaluation = SharedEvaluation::new(store.namespace())?;
            let subject = evaluation
                .subject_in(
                    &tx,
                    &bound.owner_user_id,
                    reference["subject_id"]
                        .as_str()
                        .context("missing evaluation subject id")?,
                )
                .await?
                .context("worker did not deliver evaluation subject")?;
            let result_id = reference["result_digest"]
                .as_str()
                .context("missing evaluation result id")?;
            let result = evaluation
                .result_in(&tx, &bound.owner_user_id, result_id)
                .await?
                .context("worker did not deliver evaluation result")?;
            let admission = evaluation
                .latest_admission_in(&tx, &bound.owner_user_id, result_id)
                .await?
                .context("worker did not deliver evaluation admission")?;
            ensure!(admission.status == AdmissionStatus::Admitted);
            ensure!(serde_json::to_value(admission)? == receipt.value["completion"]["admission"]);
            ensure!(subject.subject_id == bound.episode_id);
            ensure!(
                subject
                    .decisions
                    .iter()
                    .filter(|item| **item == decision)
                    .count()
                    == 1
            );
            let observed = subject
                .evidence
                .iter()
                .find(|item| item.evidence_id == "routing-prediction-observation")
                .context("delivered subject lost response observation")?;
            for (name, expected) in [
                ("observed_action", "answer_or_summarize"),
                ("observation_confidence_ppm", "850000"),
                ("observation_reason_code", "canonical_response_signal"),
            ] {
                ensure!(observed.attributes.get(name).map(String::as_str) == Some(expected));
            }
            // The observation augments the action evidence without replacing any
            // selected-treatment field or inventing a quality/latency measurement.
            let attributes = inputs["policy"]["treatment"]["attributes"]
                .as_object()
                .context("missing captured treatment attributes")?;
            if let Some(expected) = attributes.get("predicted_action").and_then(Value::as_str) {
                ensure!(
                    observed
                        .attributes
                        .get("predicted_action")
                        .map(String::as_str)
                        == Some(expected)
                );
            }
            ensure!(
                observed.attributes.get("action_match")
                    == settled.evidence.categorical.get("routing.action_match")
            );
            if let Some(predicted) = attributes.get("predicted_action").and_then(Value::as_str)
                && predicted != "unknown"
            {
                ensure!(
                    observed.attributes.get("action_match").map(String::as_str)
                        == Some(if predicted == "answer_or_summarize" {
                            "true"
                        } else {
                            "false"
                        })
                );
            }
            for name in [
                "success",
                "quality.pass",
                "latency.ms",
                "request_duration_ms",
                "cost_micro_usd",
            ] {
                ensure!(!observed.attributes.contains_key(name));
            }
            ensure!(result.verdict == EvalVerdict::Inconclusive && result.confidence_ppm.is_none());
            ensure!(
                !result.metrics.contains_key("quality.pass")
                    && !result.metrics.contains_key("latency.ms")
            );
            let outbox = SharedTrajectory::new(store.namespace())?
                .outbox_in(
                    &tx,
                    &bound.owner_user_id,
                    reference["outbox_id"]
                        .as_str()
                        .context("missing outbox id")?,
                )
                .await?
                .context("missing immutable trajectory outbox")?;
            let envelope = outbox
                .payload
                .evaluation
                .as_ref()
                .context("missing evaluation envelope")?;
            ensure!(reference["outbox_digest"] == outbox.payload_digest);
            ensure!(
                envelope.subject == subject && envelope.result == result,
                "delivered evidence differs from original projection payload"
            );
            tx.commit().await?;
            Ok(())
        })
        .await?;
    Ok(Delivered {
        id: id.into(),
        binding: bound,
        inputs,
        receipt_revision: receipt.revision()?,
        receipt: receipt.value,
    })
}

async fn replay(
    fixture: &Fixture,
    world: &World,
    mock: &Mock,
    original: &Delivered,
    messages: &Value,
    label: &str,
) -> Result<()> {
    let before = mock.calls();
    let history = episode(fixture, &original.binding).await?;
    let wire = Wire::read(post(world, 1, 0, label, messages).send().await?).await?;
    wire.expect_status(StatusCode::ACCEPTED)?;
    wire.identity(&original.id, label)?;
    ensure!(succeeded(&wire.execution()?.state));
    ensure!(mock.calls() == before, "terminal replay dispatched again");
    let again = delivered(fixture, world, &original.id, 0).await?;
    ensure!(again.binding == original.binding && again.inputs == original.inputs);
    ensure!(
        again.receipt == original.receipt && again.receipt_revision == original.receipt_revision
    );
    ensure!(
        episode(fixture, &original.binding).await? == history,
        "terminal replay appended or rewrote guard history"
    );
    Ok(())
}
