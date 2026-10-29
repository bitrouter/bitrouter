//! Stock workers replay a delivered projection after a database-proven ACK abort.

use super::*;
use crate::gateway::shared::store::Stored;
use sea_orm::TransactionTrait as _;

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_outbox_retry_preserves_projected_terminal_effects() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let mut fixture = match empty_fixture(&mut database).await {
        Ok(fixture) => fixture,
        Err(error) => return database.finish(Err(error)).await,
    };
    let mut expected = ExitExpectations::new();
    let result = run_outbox(&mut fixture, &mut expected).await;
    finish_empty_fixture_with_database(database, fixture, result, &expected).await
}

async fn run_outbox(fixture: &mut Fixture, expected: &mut ExitExpectations) -> Result<()> {
    let mut mock = Mock::start().await?;
    let mut jobs = Requests::new();
    let mut fault = None;
    let result = async {
        let world = World::start(fixture, &mock.base).await?;
        for child in world.children {
            expected.insert(child, ExpectedExit::CleanServe);
        }
        fault = Some(ack_fault(fixture).await?);
        let mut executions = Vec::new();
        for replica in 0..1 {
            let label = format!("outbox-{replica}");
            start(
                &mut jobs,
                &label,
                world.post(replica, 2, "tenant:mock", &label, &label, &label),
            );
            let arrival = mock.arrival(&label, "tenant", SECRETS[0]).await?;
            executions.push(arrival.execution.clone());
            arrival.release(Reply::Success)?;
            let wire = completed(&mut jobs, &label).await?;
            wire.expect_status(StatusCode::OK)?;
            wire.identity(&executions[replica], &label)?;
        }
        ensure!(mock.calls() == 1);
        let name = fault.as_deref().context("ACK fault missing")?;
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                // nextval is deliberately not rolled back with the failed ACK.
                // This owned fixture sequence proves the trigger was reached.
                // https://www.postgresql.org/docs/current/functions-sequence.html
                let row = fixture
                    .db
                    .query_one(Statement::from_string(
                        DatabaseBackend::Postgres,
                        format!("SELECT is_called FROM {name}"),
                    ))
                    .await?
                    .context("ACK fault probe missing")?;
                let called: bool = row.try_get("", "is_called")?;
                if called && projections_delivered(fixture, &executions).await? {
                    return Ok::<(), anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("stock worker did not reach delivered-before-ACK fault")??;
        for id in &executions {
            let outbox = terminal(fixture, id).await?;
            ensure!(
                outbox.status() == "pending"
                    && matches!(outbox.value.get("acknowledged_at_ms"), Some(Value::Null))
            );
            let view = world.wait_state(1, 2, id, succeeded).await?;
            ensure!(view.attempts.len() == 1);
        }
        let original = sink_snapshot(fixture).await?;
        remove_ack_fault(fixture, fault.take().context("ACK fault disappeared")?).await?;
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let mut all = true;
                for id in &executions {
                    let outbox = terminal(fixture, id).await?;
                    all &= outbox.status() == "delivered"
                        && outbox.value["acknowledged_at_ms"].as_i64().is_some()
                        && outbox.value["deliveries"]
                            .as_u64()
                            .is_some_and(|count| count >= 2);
                }
                if all {
                    return Ok::<(), anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("stock outbox did not recover its aborted ACK")??;
        ensure!(
            sink_snapshot(fixture).await? == original,
            "outbox retry duplicated or changed terminal effects"
        );
        ensure!(mock.calls() == 1, "projection retry redispatched inference");
        fixture.no_local_files()?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    jobs.abort_all();
    let mut cleanup = Vec::new();
    while let Some(joined) = jobs.join_next().await {
        match joined {
            Ok(Ok((label, _))) => {
                cleanup.push(Err(anyhow::anyhow!("unconsumed outbox client {label}")))
            }
            Ok(Err(error)) => cleanup.push(Err(error)),
            Err(error) if error.is_cancelled() => {}
            Err(error) => cleanup.push(Err(error.into())),
        }
    }
    if let Some(name) = fault {
        cleanup.push(remove_ack_fault(fixture, name).await);
    }
    cleanup.push(mock.finish().await);
    combine_results(result, cleanup)
}

async fn terminal(fixture: &Fixture, id: &str) -> Result<Stored<Value>> {
    fixture
        .authority
        .store()
        .get(Kind::Outbox, &format!("gateway-terminal-{id}"))
        .await?
        .context("terminal outbox missing")
}

async fn projections_delivered(fixture: &Fixture, ids: &[String]) -> Result<bool> {
    for id in ids {
        let receipt = fixture
            .authority
            .store()
            .get::<Value>(Kind::ProjectionReceipt, &format!("gateway-terminal-{id}"))
            .await?;
        let Some(receipt) = receipt else {
            return Ok(false);
        };
        if receipt.status() != "delivered" {
            return Ok(false);
        }
        ensure!(receipt.value["completion"]["sink"] == "none");
    }
    Ok(true)
}

async fn sink_snapshot(fixture: &Fixture) -> Result<Vec<String>> {
    let mut snapshot = Vec::new();
    for table in [
        "gateway_shared_metering_requests",
        "gateway_shared_eval_subjects",
        "gateway_shared_eval_results",
        "gateway_shared_eval_admission_events",
    ] {
        // Fixed table allowlist and bound namespace; all rows are fixture-owned.
        // https://www.postgresql.org/docs/current/functions-aggregate.html
        let row = fixture.db.query_one(Statement::from_sql_and_values(DatabaseBackend::Postgres,
            format!("SELECT count(*)::bigint AS count, COALESCE(jsonb_agg(to_jsonb(r) ORDER BY to_jsonb(r)::text), '[]'::jsonb)::text AS snapshot FROM {table} r WHERE namespace=$1"),
            [fixture.authority.store().namespace().into()],
        )).await?.context("sink snapshot missing")?;
        let count: i64 = row.try_get("", "count")?;
        ensure!(
            count
                == if table == "gateway_shared_metering_requests" {
                    1
                } else {
                    0
                }
        );
        snapshot.push(row.try_get("", "snapshot")?);
    }
    Ok(snapshot)
}

async fn ack_fault(fixture: &Fixture) -> Result<String> {
    let name = format!("fixture_ack_{}", uuid::Uuid::new_v4().simple());
    // Generated identifiers, a new physical database, and fixed trigger literals.
    // https://www.postgresql.org/docs/current/plpgsql-trigger.html
    let sql = format!(
        "CREATE SEQUENCE {name}; CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $body$ BEGIN IF NEW.kind='outbox' AND NEW.status='delivered' THEN PERFORM nextval('{name}'); RAISE EXCEPTION 'fixture terminal ACK abort'; END IF; RETURN NEW; END $body$; CREATE TRIGGER {name} BEFORE UPDATE ON gateway_shared_records FOR EACH ROW EXECUTE FUNCTION {name}()"
    );
    let tx = fixture.db.begin().await?;
    tx.execute_unprepared(&sql).await?;
    tx.commit().await?;
    Ok(name)
}

async fn remove_ack_fault(fixture: &Fixture, name: String) -> Result<()> {
    let tx = fixture.db.begin().await?;
    tx.execute_unprepared(&format!("DROP TRIGGER {name} ON gateway_shared_records; DROP FUNCTION {name}(); DROP SEQUENCE {name}")).await?;
    tx.commit().await?;
    Ok(())
}
