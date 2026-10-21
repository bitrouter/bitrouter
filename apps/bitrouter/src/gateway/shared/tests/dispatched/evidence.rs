//! Cancelled terminal writes retain the first observation, not a later duration.

use super::*;
use crate::gateway::shared::{credentials::CredentialStamp, dispatched::Attempt, store::Stored};
use bitrouter_ai::types::{Usage, UsageOrigin};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

pub(super) async fn exercise(
    fixture: &Fixture,
    store: &Store,
    stamp: &CredentialStamp,
) -> Result<()> {
    let (admission, ctx, _) = fixture.context().await?;
    let mut invalid = fixture.target("ok");
    invalid.service_id = "x".repeat(513);
    ensure!(
        Attempt::begin(admission.clone(), stamp, &invalid, &ctx)
            .await
            .is_err()
    );
    ensure!(!admission.blocks_fallback());
    ensure!(
        fixture
            .engine
            .owned_execution(ctx.request_id())
            .await?
            .attempts
            .is_empty()
    );

    let mut attempt = Attempt::begin(admission.clone(), stamp, &fixture.target("ok"), &ctx).await?;
    let view = fixture.engine.owned_execution(ctx.request_id()).await?;
    let attempt_id = &view.attempts.first().context("missing attempt")?.attempt_id;
    let blocker = store.begin().await?;
    let _: Stored<serde_json::Value> = store
        .lock(&blocker, Kind::Execution, ctx.request_id())
        .await?;
    let pid: i32 = blocker
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT pg_backend_pid() AS pid",
        ))
        .await?
        .context("missing backend pid")?
        .try_get("", "pid")?;
    let usage = Usage {
        prompt_tokens: 7,
        completion_tokens: 3,
        origin: UsageOrigin::ProviderReported,
        ..Usage::default()
    };
    let mut terminal = Box::pin(attempt.complete(TerminalOutcome::Succeeded, Some(&usage)));
    let waiting = async {
        loop {
            // Observe a real SQL wait rather than assume scheduling from a sleep.
            // https://www.postgresql.org/docs/current/functions-info.html
            let blocked: bool = store.database().query_one(Statement::from_sql_and_values(DatabaseBackend::Postgres,
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))) AS waiting", [pid.into()]
            )).await?.context("missing blocker result")?.try_get("", "waiting")?;
            if blocked {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::select! {
        result = &mut terminal => anyhow::bail!("terminal completed before cancellation: {result:?}"),
        result = tokio::time::timeout(Duration::from_secs(1), waiting) => result??,
    }
    drop(terminal);
    let original = attempt
        .captured_terminal()
        .context("first terminal observation missing")?
        .clone();
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(attempt);
    blocker.rollback().await?;
    admission.reconcile_dispatch().await?;
    let facts = store
        .get::<serde_json::Value>(Kind::Attempt, attempt_id)
        .await?
        .context("missing attempt facts")?
        .value;
    ensure!(
        facts["terminal_facts"] == serde_json::to_value(&original.facts)?,
        "drop changed the first terminal facts"
    );
    ensure!(
        facts["view"]["state"]["result"]["usage"]
            == serde_json::to_value(&original.observation.usage)?
    );
    admission.reconcile_dispatch().await?;
    ensure!(
        store
            .get::<serde_json::Value>(Kind::Attempt, attempt_id)
            .await?
            .context("missing replay facts")?
            .value
            == facts
    );
    fixture
        .engine
        .finish(ctx.request_id(), &TerminalOutcome::Succeeded)
        .await?;

    // An older prepared record without attribution must not authorize a send.
    let (_admission, ctx, _) = fixture.context().await?;
    let attempt = fixture
        .engine
        .prepare_attempt(&fixture.key.hash, ctx.request_id(), stamp)
        .await?;
    let tx = store.begin().await?;
    let mut row: Stored<serde_json::Value> =
        store.lock(&tx, Kind::Attempt, &attempt.attempt_id).await?;
    row.value
        .as_object_mut()
        .context("attempt is not an object")?
        .remove("attribution");
    let due = row.due_at_ms();
    store
        .save(&tx, &mut row, "prepared", due, Store::now(&tx).await?)
        .await?;
    let original = row.value.clone();
    tx.commit().await?;
    ensure!(
        fixture
            .engine
            .dispatch(&fixture.key.hash, ctx.request_id(), &attempt.attempt_id)
            .await
            .is_err()
    );
    ensure!(
        store
            .get::<serde_json::Value>(Kind::Attempt, &attempt.attempt_id)
            .await?
            .context("prepared row missing")?
            .value
            == original
    );
    fixture
        .engine
        .abandon_prepared(ctx.request_id(), &attempt.attempt_id)
        .await?;
    fixture
        .engine
        .finish(
            ctx.request_id(),
            &TerminalOutcome::Failed {
                code: "fixture_abort".into(),
            },
        )
        .await?;
    Ok(())
}
