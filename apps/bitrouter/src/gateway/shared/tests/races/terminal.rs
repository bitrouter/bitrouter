//! Terminal winners, whole-transaction rollback and bounded episode barriers.

use std::{collections::BTreeSet, time::Duration};

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::execution::{
    ExecutionState, ExecutionUsage, ResolutionOutcome, ResolveAttemptRequest, TerminalObservation,
    TerminalOutcome,
};
use bitrouter_ai::types::UsageOrigin;
use serde_json::Value;
use sha2::Digest;

use super::{Fixture, accepted, row};
use crate::gateway::shared::{
    engine::TrajectoryBinding,
    store::{Kind, NewRecord, Store, Stored},
};

pub(super) async fn exercise(first: &Store, second: &Store) -> Result<()> {
    competing_resolutions(first, second).await?;
    for provider_first in [true, false] {
        resolution_and_observation(first, second, provider_first).await?;
    }
    resolution_rollback(first, second).await?;
    projection_saturation(first, second).await
}

fn observation() -> TerminalObservation {
    TerminalObservation {
        outcome: TerminalOutcome::Succeeded,
        recorded_at_ms: 0,
        usage: ExecutionUsage {
            origin: UsageOrigin::ProviderReported,
            prompt_tokens: Some(2),
            completion_tokens: Some(1),
            reasoning_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            total_tokens: Some(3),
            cost_micro_usd: Some(5),
        },
    }
}

async fn dispatched(fixture: &Fixture) -> Result<(String, String)> {
    let id = accepted(&fixture.a, &fixture.first, fixture.revision).await?;
    let attempt = fixture
        .a
        .prepare_attempt(&fixture.first.hash, &id, &fixture.stamp)
        .await?;
    fixture
        .a
        .dispatch(&fixture.first.hash, &id, &attempt.attempt_id)
        .await?;
    Ok((id, attempt.attempt_id))
}

async fn unknown(fixture: &Fixture, id: &str, attempt: &str) -> Result<ResolveAttemptRequest> {
    fixture.a.mark_unknown(id, attempt).await?;
    Ok(ResolveAttemptRequest {
        expected_revision: fixture.b.query(&fixture.first.hash, id).await?.revision,
        outcome: ResolutionOutcome::Terminated,
        evidence_reference: "fixture-provider-case".into(),
    })
}

async fn released_once(store: &Store, fixture: &Fixture, id: &str) -> Result<Value> {
    ensure!(row(store, Kind::Resource, &fixture.pool).await?["active"] == 0);
    ensure!(row(store, Kind::Tenant, &fixture.first.tenant).await?["active"] == 0);
    let event = store
        .get::<Value>(Kind::Outbox, &format!("gateway-terminal-{id}"))
        .await?
        .context("terminal event missing")?;
    ensure!(event.revision()? == 0, "terminal event was overwritten");
    Ok(event.value)
}

async fn competing_resolutions(first: &Store, second: &Store) -> Result<()> {
    let fixture = Fixture::new(first, second, 1).await?;
    let (id, attempt) = dispatched(&fixture).await?;
    let request = unknown(&fixture, &id, &attempt).await?;
    let (a, b) = tokio::join!(
        fixture
            .a
            .resolve_unknown("operator-a", &id, &attempt, &request),
        fixture
            .b
            .resolve_unknown("operator-b", &id, &attempt, &request)
    );
    ensure!(
        a.is_ok() != b.is_ok(),
        "two conflicting assertions committed"
    );
    let winner = a
        .ok()
        .or_else(|| b.ok())
        .context("missing resolution winner")?;
    ensure!(matches!(winner.state, ExecutionState::Resolved { .. }));
    let event = released_once(first, &fixture, &id).await?;
    ensure!(event["event"]["execution"] == serde_json::to_value(&winner)?);
    Ok(())
}

async fn resolution_and_observation(
    first: &Store,
    second: &Store,
    provider_first: bool,
) -> Result<()> {
    let fixture = Fixture::new(first, second, 1).await?;
    let (id, attempt) = dispatched(&fixture).await?;
    let request = unknown(&fixture, &id, &attempt).await?;
    let blocker = second.begin().await?;
    let _: Stored<Value> = second.lock(&blocker, Kind::Resource, &fixture.pool).await?;
    let owner = fixture.a.clone();
    let resolver = fixture.b.clone();
    let execution_id = id.clone();
    let attempt_id = attempt.clone();
    let asserted = request.clone();
    let mut winner = tokio::spawn(async move {
        if provider_first {
            owner
                .record_terminal(&execution_id, &attempt_id, &observation())
                .await
        } else {
            resolver
                .resolve_unknown("operator-a", &execution_id, &attempt_id, &asserted)
                .await
                .map(|_| ())
        }
    });
    ensure!(
        tokio::time::timeout(Duration::from_millis(100), &mut winner)
            .await
            .is_err()
    );
    let owner = fixture.a.clone();
    let resolver = fixture.b.clone();
    let execution_id = id.clone();
    let attempt_id = attempt.clone();
    let asserted = request.clone();
    let loser = tokio::spawn(async move {
        if provider_first {
            resolver
                .resolve_unknown("operator-a", &execution_id, &attempt_id, &asserted)
                .await
                .map(|_| ())
        } else {
            owner
                .record_terminal(&execution_id, &attempt_id, &observation())
                .await
        }
    });
    blocker.commit().await?;
    winner.await??;
    ensure!(
        loser.await?.is_err(),
        "conflicting evidence overwrote the committed winner"
    );
    if provider_first {
        fixture.a.finish(&id, &TerminalOutcome::Succeeded).await?;
    }
    let before = released_once(first, &fixture, &id).await?;
    if provider_first {
        fixture
            .a
            .record_terminal(&id, &attempt, &observation())
            .await?;
        fixture.a.finish(&id, &TerminalOutcome::Succeeded).await?;
    } else {
        fixture
            .b
            .resolve_unknown("operator-a", &id, &attempt, &request)
            .await?;
    }
    ensure!(released_once(first, &fixture, &id).await? == before);
    Ok(())
}

async fn bind(store: &Store, fixture: &Fixture, id: &str) -> Result<String> {
    let binding = TrajectoryBinding {
        owner_user_id: fixture.first.tenant.clone(),
        episode_id: uuid::Uuid::new_v4().to_string(),
        request_id: id.into(),
        correlation_epoch: "fixture-epoch".into(),
    };
    let tx = store.begin().await?;
    fixture.a.bind_trajectory(&tx, id, &binding).await?;
    tx.commit().await?;
    Ok(hex::encode(sha2::Sha256::digest(serde_json::to_vec(&(
        "gateway-episode-fence-v1",
        &binding.owner_user_id,
        &binding.episode_id,
    ))?)))
}

async fn resolution_rollback(first: &Store, second: &Store) -> Result<()> {
    let fixture = Fixture::new(first, second, 1).await?;
    let id = accepted(&fixture.a, &fixture.first, fixture.revision).await?;
    let fence = bind(first, &fixture, &id).await?;
    let attempt = fixture
        .a
        .prepare_attempt(&fixture.first.hash, &id, &fixture.stamp)
        .await?;
    fixture
        .a
        .dispatch(&fixture.first.hash, &id, &attempt.attempt_id)
        .await?;
    let request = unknown(&fixture, &id, &attempt.attempt_id).await?;
    let event_id = format!("gateway-terminal-{id}");
    let tx = first.begin().await?;
    ensure!(
        first
            .create(
                &tx,
                NewRecord {
                    kind: Kind::Outbox,
                    key: &event_id,
                    status: "fixture",
                    due_at_ms: None,
                    now_ms: Store::now(&tx).await?,
                    value: &serde_json::json!({"fault": "collision"}),
                }
            )
            .await?
    );
    tx.commit().await?;
    let keys = [
        (Kind::Execution, id.as_str()),
        (Kind::Attempt, attempt.attempt_id.as_str()),
        (Kind::Resource, fixture.pool.as_str()),
        (Kind::Tenant, fixture.first.tenant.as_str()),
        (Kind::EpisodeFence, fence.as_str()),
        (Kind::Outbox, event_id.as_str()),
    ];
    let mut before = Vec::new();
    for (kind, key) in keys {
        before.push(row(first, kind, key).await?);
    }
    ensure!(
        fixture
            .b
            .resolve_unknown("operator-a", &id, &attempt.attempt_id, &request)
            .await
            .is_err()
    );
    for ((kind, key), expected) in keys.into_iter().zip(before) {
        ensure!(
            row(first, kind, key).await? == expected,
            "resolution partially committed {key}"
        );
    }
    Ok(())
}

async fn projection_saturation(first: &Store, second: &Store) -> Result<()> {
    for already_pending in [true, false] {
        let fixture = Fixture::new(first, second, 1).await?;
        let id = accepted(&fixture.a, &fixture.first, fixture.revision).await?;
        let fence_key = bind(first, &fixture, &id).await?;
        let attempt = fixture
            .a
            .prepare_attempt(&fixture.first.hash, &id, &fixture.stamp)
            .await?;
        fixture
            .a
            .dispatch(&fixture.first.hash, &id, &attempt.attempt_id)
            .await?;
        let tx = first.begin().await?;
        let mut fence: Stored<Value> = first.lock(&tx, Kind::EpisodeFence, &fence_key).await?;
        let mut pending: BTreeSet<String> = (0..256).map(|i| format!("pending-{i}")).collect();
        if already_pending {
            pending.remove("pending-0");
            pending.insert(id.clone());
        }
        fence.value["pending"] = serde_json::to_value(pending)?;
        first
            .save(&tx, &mut fence, "pending", None, Store::now(&tx).await?)
            .await?;
        tx.commit().await?;
        let execution_before = row(first, Kind::Execution, &id).await?;
        let attempt_before = row(first, Kind::Attempt, &attempt.attempt_id).await?;
        let terminal = fixture
            .a
            .record_terminal(&id, &attempt.attempt_id, &observation())
            .await;
        if already_pending {
            terminal?;
            fixture.a.finish(&id, &TerminalOutcome::Succeeded).await?;
            released_once(first, &fixture, &id).await?;
        } else {
            ensure!(
                terminal.is_err(),
                "full episode barrier admitted a new pending execution"
            );
            ensure!(row(first, Kind::Execution, &id).await? == execution_before);
            ensure!(row(first, Kind::Attempt, &attempt.attempt_id).await? == attempt_before);
            ensure!(row(first, Kind::Resource, &fixture.pool).await?["active"] == 1);
            ensure!(row(first, Kind::Tenant, &fixture.first.tenant).await?["active"] == 1);
        }
        ensure!(row(first, Kind::EpisodeFence, &fence_key).await? == fence.value);
    }
    Ok(())
}
