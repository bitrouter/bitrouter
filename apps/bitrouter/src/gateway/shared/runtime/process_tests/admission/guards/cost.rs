//! Cost-only policy in a separate stock-process fixture, using published tariffs.

use bitrouter_ai::eval::MetricUnit;

use super::*;

const COST_ROOT: &str = "Describe the priced fixture response.";

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_guard_cost_threshold_uses_published_tariffs_cross_replicas() -> Result<()> {
    run(Campaign::GuardCost).await
}

pub(super) fn seed_plan(base: &str, keys: &[crate::auth::keys::GeneratedKey; 4]) -> Result<Value> {
    // A fresh database/publication isolates this policy from the counter/epoch
    // campaign. Reuse its preset name and helpers, but enable only the cost guard.
    let mut plan = super::seed_plan(base, keys)?;
    let guard = &mut plan["configuration"]["policy_lock"]["policies"][POLICY]["progress_guard"];
    guard["max_consecutive_unprotected"] = Value::Null;
    guard["max_episode_cost_micro_usd"] = json!(26);
    let routing = &mut plan["configuration"]["routing"];
    for (provider, input, output) in [("tenant", 1.0, 2.0), ("account-a", 2.0, 4.0)] {
        // PricingConfig rates are micro-USD per token. Supply every bucket so
        // the complete-known-pricing gate is exercised, even with zero cache use.
        routing["providers"][provider]["models"][0]["pricing"] = json!({
            "input_micro_usd_per_token": input,
            "cache_read_micro_usd_per_token": input,
            "cache_write_micro_usd_per_token": input,
            "output_micro_usd_per_token": output
        });
    }
    plan["configuration"]["require_known_pricing"] = json!(true);
    Ok(plan)
}

pub(super) async fn campaign(
    fixture: &Fixture,
    world: &World,
    mock: &mut Mock,
    jobs: &mut Requests,
) -> Result<()> {
    let key = fixture
        .authority
        .keys(&fixture.authority.head().await?)
        .await?
        .active_correlation()?;
    let epoch = key.key_id().to_owned();
    let canonical = Canonicalizer::new(key);
    let root_messages = messages(COST_ROOT, None);
    let root = one(
        fixture,
        world,
        mock,
        jobs,
        &RequestCase {
            replica: 0,
            principal: 0,
            label: "cost-root",
            messages: root_messages.clone(),
            tier: "economy",
            clause: None,
        },
    )
    .await?;
    binding_key(&root, OWNER, &epoch, &canonical)?;
    charge(fixture, &root, "cheap", 13).await?;
    cost_counts(&episode(fixture, &root.binding).await?, 1, 1, 0, 0, 1, 13)?;

    // Below the threshold, the peer still chooses cheap. Actual provider usage
    // (7 prompt + 3 completion) brings the accumulated cost to exactly 26.
    let below = one(
        fixture,
        world,
        mock,
        jobs,
        &RequestCase {
            replica: 1,
            principal: 1,
            label: "cost-below-threshold",
            messages: messages(COST_ROOT, Some("Describe the next priced step.")),
            tier: "economy",
            clause: None,
        },
    )
    .await?;
    binding_key(&below, OWNER, &epoch, &canonical)?;
    ensure!(below.binding.episode_id == root.binding.episode_id);
    charge(fixture, &below, "cheap", 13).await?;
    let at_threshold = episode(fixture, &root.binding).await?;
    cost_counts(&at_threshold, 2, 2, 0, 0, 2, 26)?;
    ensure!(
        activations(&at_threshold) == 0,
        "cost guard activated before reaching its threshold"
    );

    // Both submissions precede either provider arrival. Their serialized guard
    // decisions must see the committed cost, never a fresh per-process total.
    for (replica, principal, label) in [(0, 0, "cost-overlap-a"), (1, 1, "cost-overlap-b")] {
        start(
            jobs,
            label,
            post(
                world,
                replica,
                principal,
                label,
                &messages(COST_ROOT, Some(label)),
            ),
        );
    }
    let a = mock.arrival(COST_ROOT, "account", SECRETS[1]).await?;
    let b = mock.arrival(COST_ROOT, "account", SECRETS[1]).await?;
    ensure!(a.model == "strong" && b.model == "strong" && a.execution != b.execution);
    let stored_a = execution(fixture, &a.execution).await?;
    let stored_b = execution(fixture, &b.execution).await?;
    ensure!(stored_a["owner"] != stored_b["owner"]);
    for (id, stored) in [(&a.execution, &stored_a), (&b.execution, &stored_b)] {
        let bound = binding(stored)?;
        ensure!(bound.episode_id == root.binding.episode_id && bound.owner_user_id == OWNER);
        ensure!(bound.correlation_epoch == epoch);
        ensure!(bound.request_id == canonical.request_identity(OWNER, id)?);
        let public = world.status(1, 0, id).await?;
        public.expect_status(StatusCode::OK)?;
        ensure!(matches!(public.execution()?.state, ExecutionState::Running));
    }
    let overlapping = episode(fixture, &root.binding).await?;
    ensure!(overlapping.events.starts_with(&at_threshold.events));
    cost_counts(&overlapping, 4, 2, 2, 1, 0, 26)?;
    ensure!(activations(&overlapping) == 1);
    let routes = overlapping
        .events
        .iter()
        .filter(|event| event.kind == TrajectoryEventKind::RouteIntentRecorded)
        .collect::<Vec<_>>();
    ensure!(routes.len() == 4);
    route(
        routes.get(2).context("missing cost escalation")?,
        "strong",
        Some("progress_guard.max_episode_cost_micro_usd"),
    )?;
    route(
        routes.get(3).context("missing cost hold")?,
        "strong",
        Some("progress_guard.active_hold"),
    )?;
    occupancy(fixture, OWNER, Kind::Tenant, 2).await?;
    occupancy(fixture, ACCOUNT_POOL, Kind::Resource, 2).await?;

    let a_id = a.execution.clone();
    let b_id = b.execution.clone();
    b.release(Reply::Success)?;
    a.release(Reply::Success)?;
    let mut remaining = BTreeSet::from([a_id.as_str(), b_id.as_str()]);
    for _ in 0..2 {
        let (_, wire) = completed_any(jobs).await?;
        let id = wire.header("x-bitrouter-execution-id")?;
        ensure!(remaining.remove(id));
        successful_wire(&wire, id)?;
    }
    ensure!(remaining.is_empty());
    for id in [&a_id, &b_id] {
        let done = delivered(fixture, world, id, 0).await?;
        binding_key(&done, OWNER, &epoch, &canonical)?;
        ensure!(done.binding.episode_id == root.binding.episode_id);
        charge(fixture, &done, "strong", 26).await?;
    }
    let final_history = episode(fixture, &root.binding).await?;
    cost_counts(&final_history, 4, 4, 0, 1, 0, 78)?;
    ensure!(activations(&final_history) == 1);
    replay(fixture, world, mock, &root, &root_messages, "cost-root").await?;
    charge(fixture, &root, "cheap", 13).await?;
    ensure!(episode(fixture, &root.binding).await? == final_history);
    ensure!(
        mock.calls() == 4,
        "priced replay or guard dispatched extra inference"
    );
    for (kind, id) in [
        (Kind::Tenant, OWNER),
        (Kind::Resource, TENANT_POOL),
        (Kind::Resource, ACCOUNT_POOL),
    ] {
        occupancy(fixture, id, kind, 0).await?;
    }
    Ok(())
}

fn cost_counts(
    history: &Episode,
    requests: u64,
    settled: u64,
    pending: u64,
    hold: u64,
    unprotected: u64,
    cost: u64,
) -> Result<()> {
    let snapshot = reduce(&history.events, &BTreeSet::from(["strong".into()]))?;
    ensure!(snapshot.health.request_count == requests);
    ensure!(snapshot.health.settled_request_count == settled);
    ensure!(snapshot.health.unsettled_request_count == pending);
    ensure!(snapshot.active_hold_remaining == hold);
    ensure!(snapshot.health.consecutive_unprotected_requests == unprotected);
    ensure!(snapshot.health.total_tokens == Some(settled * 10));
    ensure!(snapshot.health.settled_cost_micro_usd == Some(cost));
    Ok(())
}

async fn charge(fixture: &Fixture, done: &Delivered, model: &str, expected: u64) -> Result<()> {
    let stored = execution(fixture, &done.id).await?;
    let view: GatewayExecution = serde_json::from_value(stored["view"].clone())?;
    let ExecutionState::Terminal { result } = &view.state else {
        anyhow::bail!("priced execution is not terminal");
    };
    ensure!(
        result.outcome == TerminalOutcome::Succeeded
            && result.usage.cost_micro_usd == Some(expected)
    );
    ensure!(view.attempts.len() == 1);
    let attempt_view = view.attempts.first().context("priced attempt missing")?;
    let AttemptState::Terminal { result } = &attempt_view.state else {
        anyhow::bail!("priced attempt is not terminal");
    };
    ensure!(result.usage.cost_micro_usd == Some(expected));
    let store = fixture.authority.store();
    let attempt = store
        .get::<Value>(Kind::Attempt, &attempt_view.attempt_id)
        .await?
        .context("priced attempt authority missing")?;
    ensure!(attempt.value["attribution"]["model_id"] == model);
    ensure!(
        attempt.value["terminal_facts"]["charge"]["charge_micro_usd"].as_u64() == Some(expected)
    );
    let rate = if model == "cheap" { 1.0 } else { 2.0 };
    let tariff = &attempt.value["attribution"]["tariff"];
    ensure!(tariff["pricing"]["input_micro_usd_per_token"].as_f64() == Some(rate));
    ensure!(tariff["pricing"]["output_micro_usd_per_token"].as_f64() == Some(rate * 2.0));
    let history = episode(fixture, &done.binding).await?;
    let settlement = request_event(&history, &done.binding, TrajectoryEventKind::RequestSettled)?;
    ensure!(
        settlement
            .evidence
            .structural
            .get("settlement.cost_micro_usd")
            == Some(&expected)
    );
    // Evaluation metrics reflect the exact historical prefix at that settlement,
    // even if the other overlapping request finished first or has since settled.
    let prefix = history
        .events
        .iter()
        .take_while(|event| event.sequence <= settlement.sequence)
        .cloned()
        .collect::<Vec<_>>();
    let accumulated = reduce(&prefix, &BTreeSet::from(["strong".into()]))?
        .health
        .settled_cost_micro_usd
        .context("priced prefix lost its cost")?;
    let result_id = done.receipt["projection"]["evaluation"]["reference"]["result_digest"]
        .as_str()
        .context("priced evaluation result missing")?;
    store
        .retry(|| async {
            let tx = store.begin().await?;
            let result = SharedEvaluation::new(store.namespace())?
                .result_in(&tx, OWNER, result_id)
                .await?
                .context("priced delivered result missing")?;
            for (name, value) in [
                ("cost.usd_micros", expected),
                ("trajectory.cost.usd_micros", accumulated),
            ] {
                let metric = result.metrics.get(name).context("priced metric missing")?;
                ensure!(
                    metric.unit == MetricUnit::MicroUsd && metric.value == i64::try_from(value)?
                );
            }
            tx.commit().await?;
            Ok(())
        })
        .await
}
