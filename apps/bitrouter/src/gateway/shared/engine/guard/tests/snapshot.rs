//! A stale Serializable snapshot must fail before allocating an episode sequence.

use super::*;
use crate::gateway::shared::engine::EpisodeFence;

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege"]
async fn postgres_read_committed_terminal_invalidates_guard_snapshot() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let f = Fixture::new(&mut database).await?;
        let root = prompt("terminal-snapshot");
        let root_id = f.admit().await?;
        f.route(&root_id, &root, AT).await?;
        let binding = f.binding(&root_id).await?;
        let child_id = f.admit().await?;
        let setup = f.observer.begin().await?;
        let fence = f
            .engine
            .episode_fence(&setup, &binding, Store::now(&setup).await?)
            .await?;
        let key = fence.key().to_owned();
        setup.rollback().await?;
        let stale = f.observer.begin().await?;
        let result = async {
            let before: Stored<EpisodeFence> = f
                .observer
                .read(&stale, Kind::EpisodeFence, &key)
                .await?
                .context("missing fence")?;
            ensure!(before.value.pending.is_empty());
            // The production terminal writer uses Read Committed. Its atomic
            // episode-fence update must still invalidate the older Serializable
            // reader before that reader can authorize a new guarded start.
            f.engine
                .finish_with_inputs(
                    &root_id,
                    &TerminalOutcome::Failed {
                        code: "fixture_before_dispatch".into(),
                    },
                    None,
                )
                .await?;
            let error = f
                .engine
                .episode_fence(&stale, &binding, Store::now(&stale).await?)
                .await
                .err()
                .context("stale terminal fence remained lockable")?;
            ensure!(is_serialization_abort(&error));
            Ok(())
        }
        .await;
        let rollback = stale.rollback().await.map_err(anyhow::Error::from);
        combine_results(result, vec![rollback])?;
        let pending: Stored<EpisodeFence> = f
            .observer
            .get(Kind::EpisodeFence, &key)
            .await?
            .context("missing pending fence")?;
        ensure!(pending.value.pending.contains(&root_id));
        let next = f.route(&child_id, &continued(&root), LATER).await?;
        ensure!(next.0.episode_id == binding.episode_id);
        ensure!(next.1.snapshot.health.settled_request_count == 1);
        let settlement = next
            .0
            .prior_events
            .iter()
            .find(|event| {
                event.kind == TrajectoryEventKind::RequestSettled
                    && event.request_id.as_deref() == Some(binding.request_id.as_str())
            })
            .context("new guard omitted the committed terminal")?;
        ensure!(settlement.sequence < next.1.route_sequence);
        let settled: Stored<EpisodeFence> = f
            .observer
            .get(Kind::EpisodeFence, &key)
            .await?
            .context("missing settled fence")?;
        ensure!(settled.value.pending.is_empty());
        Ok(())
    })
    .await
    .context("terminal snapshot campaign deadline")
    .and_then(|result| result);
    database.finish(result).await
}

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL with PostgreSQL CREATEDB privilege"]
async fn postgres_new_guard_invalidates_an_already_taken_episode_snapshot() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let result = AssertUnwindSafe(tokio::time::timeout(Duration::from_secs(60), async {
        let fixture = Fixture::new(&mut database).await?;
        stale_snapshot(&fixture).await
    }))
    .catch_unwind()
    .await;
    let result = match result {
        Ok(result) => result
            .context("guard snapshot campaign timed out")
            .and_then(|result| result),
        Err(payload) => {
            drop(payload);
            Err(anyhow::anyhow!("guard snapshot campaign panicked"))
        }
    };
    database.finish(result).await
}

async fn stale_snapshot(f: &Fixture) -> Result<()> {
    let root = prompt("stale-guard-snapshot");
    let root_id = f.admit().await?;
    f.route(&root_id, &root, AT).await?;
    let binding = f.binding(&root_id).await?;
    // Admit both executions before fixing the old snapshot. Admission must not
    // supply an unrelated tenant-row update that masks the episode-fence bug.
    let first_id = f.admit().await?;
    let second_id = f.admit().await?;
    let first_prompt = continued(&root);
    let mut second_prompt = continued(&root);
    *second_prompt
        .messages
        .last_mut()
        .context("second branch has no follow-up")? =
        Message::text(Role::User, "independent second branch");
    let trajectory = SharedTrajectory::new(f.authority.store().namespace())?;
    let setup = f.observer.begin().await?;
    let fence = f
        .engine
        .episode_fence(&setup, &binding, Store::now(&setup).await?)
        .await?;
    ensure!(fence.value.pending.is_empty());
    let key = fence.key().to_owned();
    let revision = fence.revision()?;
    setup.rollback().await?;

    // An awaited MVCC read is the deterministic barrier: B's Serializable
    // snapshot exists before A can start, and B takes no row lock that blocks A.
    // https://www.postgresql.org/docs/current/transaction-iso.html
    let stale = f.observer.begin().await?;
    let result = async {
        let old: Stored<EpisodeFence> = f
            .observer
            .read(&stale, Kind::EpisodeFence, &key)
            .await?
            .context("old episode fence missing")?;
        ensure!(old.revision()? == revision && old.value.pending.is_empty());
        let history = trajectory
            .history_in(&stale, &f.owner, &binding.episode_id)
            .await?;

        // Only the actual authenticated guard operation writes the new start.
        // There is no terminal, projection catch-up, background worker or fake
        // fence update that could accidentally make this test pass.
        let first = f.route(&first_id, &first_prompt, LATER).await?;
        ensure!(first.0.episode_id == binding.episode_id);
        ensure!(first.0.prior_events == history);
        ensure!(first.1.snapshot.health.request_count == 2);
        ensure!(f.binding(&first_id).await?.episode_id == binding.episode_id);
        ensure!(
            trajectory
                .history_in(&stale, &f.owner, &binding.episode_id)
                .await?
                == history,
            "the deliberately stale snapshot changed"
        );
        let committed = f.snapshot().await?;
        let error = match f
            .engine
            .episode_fence(&stale, &binding, Store::now(&stale).await?)
            .await
        {
            Ok(_) => anyhow::bail!("new guard left the stale episode snapshot lockable"),
            Err(error) => error,
        };
        ensure!(
            is_serialization_abort(&error),
            "expected SQLSTATE 40001 at the stale fence, got {error:#}"
        );
        Ok(committed)
    }
    .await;
    // Release B even when an assertion fails. The outer database owner also
    // outlives timeout cancellation and closes every registered connection.
    let released = stale.rollback().await.map_err(anyhow::Error::from);
    let committed = match result {
        Ok(committed) => {
            released?;
            committed
        }
        Err(error) => return combine_results(Err(error), vec![released]),
    };
    ensure!(
        f.snapshot().await? == committed,
        "stale abort mutated durable state"
    );
    let advanced: Stored<EpisodeFence> = f
        .observer
        .get(Kind::EpisodeFence, &key)
        .await?
        .context("committed episode fence missing")?;
    ensure!(advanced.revision()? == revision + 1);
    ensure!(advanced.value.pending.is_empty());

    // A new transaction sees A's committed start and records B exactly once.
    // This is the normal engine call, not a fixture retry of a failed HTTP call.
    let second = f.route(&second_id, &second_prompt, LATER).await?;
    ensure!(second.0.episode_id == binding.episode_id);
    ensure!(second.1.snapshot.health.request_count == 3);
    ensure!(
        second.1.route_sequence > first_route_sequence(&second.0, &f.binding(&first_id).await?)?
    );
    let settled = f.snapshot().await?;
    ensure!(f.route(&second_id, &second_prompt, LATER).await? == second);
    ensure!(
        f.snapshot().await? == settled,
        "guard replay advanced the fence"
    );
    Ok(())
}

fn first_route_sequence(correlated: &CorrelatedRequest, first: &TrajectoryBinding) -> Result<u64> {
    let routes = correlated
        .prior_events
        .iter()
        .filter(|event| {
            event.kind == TrajectoryEventKind::RouteIntentRecorded
                && event.request_id.as_deref() == Some(first.request_id.as_str())
        })
        .collect::<Vec<_>>();
    ensure!(
        routes.len() == 1,
        "first guard route was missing or duplicated"
    );
    Ok(routes[0].sequence)
}

fn is_serialization_abort(error: &anyhow::Error) -> bool {
    use sea_orm::{DbErr, RuntimeErr, SqlxError};
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<DbErr>(),
            Some(DbErr::Exec(RuntimeErr::SqlxError(SqlxError::Database(database)))
                | DbErr::Query(RuntimeErr::SqlxError(SqlxError::Database(database))))
                if database.code().as_deref() == Some("40001")
        )
    })
}
