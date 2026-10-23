//! Corruption and replay isolation at the authenticated guard transaction.

use super::*;
use crate::gateway::providers::auth::{credentials::Credential, store::CredentialKey};
use crate::gateway::shared::{capture::Inputs, credentials::Credentials};
use crate::metering::shared::evidence::{Attribution, ObservedTerminal};

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL with PostgreSQL CREATEDB privilege"]
async fn postgres_guard_rejects_capture_identity_and_replays_before_pending_terminal() -> Result<()>
{
    let mut database = EmptyDatabase::create().await?;
    let result = AssertUnwindSafe(tokio::time::timeout(Duration::from_secs(60), async {
        let fixture = Fixture::new(&mut database).await?;
        capture_identity(&fixture).await?;
        committed_replay(&fixture).await
    }))
    .catch_unwind()
    .await;
    let result = match result {
        Ok(result) => result
            .context("guard regression timed out")
            .and_then(|result| result),
        Err(payload) => {
            drop(payload);
            Err(anyhow::anyhow!("guard regression panicked"))
        }
    };
    database.finish(result).await
}

async fn replace_capture(f: &Fixture, id: &str, inputs: Inputs) -> Result<()> {
    let store = f.authority.store();
    let tx = store.begin().await?;
    let mut execution: Stored<Execution> = store.lock(&tx, Kind::Execution, id).await?;
    execution.value.projection = Some(Box::new(inputs));
    let status = execution.status().to_owned();
    let due = execution.due_at_ms();
    f.engine
        .save_execution(&tx, &mut execution, &status, due, Store::now(&tx).await?)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn capture_identity(f: &Fixture) -> Result<()> {
    for (field, value) in [
        (
            "execution_id",
            serde_json::json!(uuid::Uuid::new_v4().to_string()),
        ),
        ("owner", serde_json::json!(uuid::Uuid::new_v4().to_string())),
        (
            "operation",
            serde_json::to_value(ModelOperation::Decisions)?,
        ),
    ] {
        let id = f.admit().await?;
        let original = *f
            .execution(&id)
            .await?
            .value
            .projection
            .context("capture missing")?;
        let mut mutated = serde_json::to_value(&original)?;
        mutated[field] = value;
        let mutated: Inputs = serde_json::from_value(mutated)?;
        mutated.validate()?;
        replace_capture(f, &id, mutated).await?;
        let before = f.snapshot().await?;
        let p = prompt(&format!("capture-identity-{field}"));
        rejected(
            f.route(&id, &p, AT).await,
            "projection input identity mismatch",
        )?;
        ensure!(
            f.snapshot().await? == before,
            "{field} mismatch committed guard effects"
        );
        ensure!(f.execution(&id).await?.value.trajectory.is_none());
        replace_capture(f, &id, original).await?;
        f.route(&id, &p, AT).await?;
        ensure!(f.execution(&id).await?.value.trajectory.is_some());
    }
    Ok(())
}

async fn captured_route(f: &Fixture, id: &str) -> Result<Inputs> {
    let binding = f.binding(id).await?;
    let tx = f.observer.begin().await?;
    let events = SharedTrajectory::new(f.authority.store().namespace())?
        .history_in(&tx, &f.owner, &binding.episode_id)
        .await?;
    tx.commit().await?;
    let event = events
        .iter()
        .find(|event| {
            event.kind == TrajectoryEventKind::RouteIntentRecorded
                && event.request_id.as_deref() == Some(&binding.request_id)
        })
        .context("original route missing")?;
    let decision = crate::trajectory::evaluation::decode_eval_decision(event)?
        .context("original decision missing")?;
    // This fixture isolates replay/attempt transaction ordering. The separate
    // PolicyRuntime campaign covers the production policy-to-capture handoff.
    // Decode the actual committed decision rather than guessing its identity.
    let mut inputs = serde_json::to_value(
        f.execution(id)
            .await?
            .value
            .projection
            .context("capture missing")?,
    )?;
    inputs["phase"] = serde_json::json!({"state": "ready"});
    inputs["policy"] = serde_json::json!({
        "state": "evaluated", "policy": f.route.policy_name,
        "digest": f.route.policy_digest, "guarded": true,
        "treatment": {
            "decision": decision, "preset": "coding", "holdout": false,
            "attributes": {}, "observed_at": AT
        }
    });
    let inputs: Inputs = serde_json::from_value(inputs)?;
    inputs.require_ready()?;
    Ok(inputs)
}

async fn committed_replay(f: &Fixture) -> Result<()> {
    const POOL: &str = "guard-replay-pool";
    f.engine.set_resource(POOL, None, 4).await?;
    f.engine
        .set_tenant(
            &f.owner,
            Some(1),
            &TenantLimits {
                concurrent_executions: 64,
                requests_per_minute: 1000,
                resource_pools: [POOL.to_owned()].into(),
            },
        )
        .await?;
    let credential = Credentials::new(f.authority.clone())
        .import(
            &CredentialKey {
                provider: "fixture".into(),
                account: "replay".into(),
            },
            POOL,
            None,
            &Credential::api_key("owned-guard-replay-fixture"),
        )
        .await?;
    let a = f.admit().await?;
    let root = prompt("committed-replay-gap");
    let original = f.route(&a, &root, AT).await?;
    let b = f.admit().await?;
    let child = continued(&root);
    let second = f.route(&b, &child, LATER).await?;
    ensure!(second.0.episode_id == original.0.episode_id);
    let inputs = captured_route(f, &b).await?;
    let attribution = Attribution::fixture();
    let attempt = f
        .engine
        .prepare_observed_attempt(&f.key_hash, &b, &credential, &attribution, &inputs)
        .await?;
    f.engine
        .dispatch(&f.key_hash, &b, &attempt.attempt_id)
        .await?;
    let outcome = TerminalOutcome::Failed {
        code: "fixture_provider_rejection".into(),
    };
    f.engine
        .record_observed_terminal(
            &b,
            &attempt.attempt_id,
            &ObservedTerminal::capture(outcome.clone(), None, &attribution, None),
        )
        .await?;
    let binding = f.binding(&b).await?;
    let tx = f.observer.begin().await?;
    f.authority.fence(&tx).await?;
    let pending = f
        .engine
        .episode_fence(&tx, &binding, Store::now(&tx).await?)
        .await?;
    ensure!(pending.value.pending.contains(&b));
    tx.rollback().await?;
    ensure!(
        f.authority
            .store()
            .get::<Outbox>(Kind::Outbox, &format!("gateway-terminal-{b}"))
            .await?
            .is_none()
    );

    let c = f.admit().await?;
    let before = f.snapshot().await?;
    ensure!(f.route(&a, &root, AT).await? == original);
    ensure!(
        f.snapshot().await? == before,
        "exact replay changed later pending state"
    );
    ensure!(f.route(&a, &prompt("changed-replay"), AT).await.is_err());
    ensure!(f.snapshot().await? == before, "changed replay wrote state");
    rejected(
        f.route(&c, &continued(&child), LATER).await,
        "terminal event is missing",
    )?;
    ensure!(
        f.snapshot().await? == before,
        "fresh start bypassed pending terminal"
    );
    ensure!(f.execution(&c).await?.value.trajectory.is_none());

    f.engine.finish_with_inputs(&b, &outcome, None).await?;
    let next = f.route(&c, &continued(&child), LATER).await?;
    ensure!(next.0.episode_id == original.0.episode_id);
    ensure!(next.1.snapshot.health.settled_request_count == 1);
    let after = f.snapshot().await?;
    ensure!(f.route(&a, &root, AT).await? == original);
    ensure!(f.snapshot().await? == after);
    Ok(())
}
