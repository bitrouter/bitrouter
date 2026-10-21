//! Stock A03 candidates at committed Prepared and attempt-Terminal boundaries.
//!
//! The fixture owns a trigger and transaction advisory lock in its fresh PG
//! database. The trigger delays only the selected status transition; it never
//! inserts or edits authority facts. Only the surviving stock worker reconciles.

use bitrouter_ai::gateway::execution::{ExecutionUsage, GatewayAttempt};
use sea_orm::DatabaseTransaction;

use super::*;
use crate::metering::shared::SharedMetering;

const POST_KILL_WAIT: Duration = Duration::from_secs(24);

#[derive(Clone, Copy)]
enum Boundary {
    Prepared,
    AttemptTerminal,
}

impl Boundary {
    fn tag(self) -> &'static str {
        match self {
            Self::Prepared => "kill-committed-prepared",
            Self::AttemptTerminal => "kill-observed-attempt-terminal",
        }
    }

    fn calls(self) -> usize {
        match self {
            Self::Prepared => 0,
            Self::AttemptTerminal => 1,
        }
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_owner_killed_after_prepared_commit_never_dispatches() -> Result<()> {
    run(Case::PreparedBeforeDispatch).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_owner_killed_after_attempt_terminal_recovers_original_success() -> Result<()> {
    run(Case::AttemptTerminalBeforeExecution).await
}

/// Retained by recovery::run outside its campaign timeout. A cancelled arm or
/// exercise cannot discard the trigger name or the owned lock transaction.
pub(super) struct Barrier {
    boundary: Option<Boundary>,
    name: Option<String>,
    held: Option<DatabaseTransaction>,
    holder_pid: Option<i32>,
    key: i64,
}

impl Barrier {
    pub(super) fn new(case: Case) -> Self {
        let boundary = match case {
            Case::PreparedBeforeDispatch => Some(Boundary::Prepared),
            Case::AttemptTerminalBeforeExecution => Some(Boundary::AttemptTerminal),
            _ => None,
        };
        Self {
            boundary,
            name: None,
            held: None,
            holder_pid: None,
            key: 0x5031_0000_0000_0000_i64 | i64::from(rand::random::<u32>()),
        }
    }

    async fn arm(&mut self, fixture: &Fixture) -> Result<()> {
        let boundary = self.boundary.context("inactive finalization barrier")?;
        ensure!(self.name.is_none() && self.held.is_none());
        let name = format!("fixture_final_{}", uuid::Uuid::new_v4().simple());
        // Retain the name before DDL: a cancelled/uncertain commit still leaves
        // a concrete IF EXISTS cleanup obligation with the outer owner.
        self.name = Some(name.clone());
        let namespace = fixture.authority.store().namespace();
        ensure!(
            namespace
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        );
        let predicate = match boundary {
            Boundary::Prepared => {
                "NEW.kind='attempt' AND OLD.status='prepared' AND NEW.status='dispatched' AND EXISTS (SELECT 1 FROM gateway_shared_records e WHERE e.namespace=NEW.namespace AND e.kind='execution' AND e.key=NEW.body::jsonb->>'execution_id' AND e.body::jsonb->'view'->>'correlation_id'=TG_ARGV[1])"
            }
            Boundary::AttemptTerminal => {
                "NEW.kind='execution' AND OLD.status='running' AND NEW.status='terminal' AND NEW.body::jsonb->'view'->>'correlation_id'=TG_ARGV[1]"
            }
        };
        // Fixed predicates, generated SQL identifiers, validated namespace and
        // fixed tags. No application body is interpolated into DDL.
        // https://www.postgresql.org/docs/current/plpgsql-trigger.html
        let sql = format!(
            "CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $body$ BEGIN IF NEW.namespace=TG_ARGV[0] AND ({predicate}) THEN PERFORM pg_advisory_xact_lock(TG_ARGV[2]::bigint); END IF; RETURN NEW; END $body$; CREATE TRIGGER {name} BEFORE UPDATE ON gateway_shared_records FOR EACH ROW EXECUTE FUNCTION {name}('{namespace}','{}','{}')",
            boundary.tag(),
            self.key,
        );
        let tx = fixture.db.begin().await?;
        bounded_ddl(&tx).await?;
        tx.execute_unprepared(&sql).await?;
        tx.commit().await?;

        // This transaction owns only the advisory lock, not any authority row.
        // Plain MVCC reads therefore see the exact committed pre-transition state.
        self.held = Some(fixture.db.begin().await?);
        let held = self.held.as_ref().context("barrier transaction missing")?;
        held.execute_unprepared(
            "SET LOCAL statement_timeout='3s'; SET LOCAL idle_in_transaction_session_timeout='30s'",
        )
        .await?;
        let row = held
            .query_one(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT pg_backend_pid() AS pid, pg_advisory_xact_lock($1)",
                [self.key.into()],
            ))
            .await?
            .context("barrier backend missing")?;
        self.holder_pid = Some(row.try_get("", "pid")?);
        Ok(())
    }

    async fn proof(&self, fixture: &Fixture) -> Result<Proof> {
        let boundary = self.boundary.context("inactive finalization barrier")?;
        let holder = self.holder_pid.context("barrier backend missing")?;
        // One statement samples the exact advisory waiter and the visible
        // execution/attempt/capacity/outbox state. No HTTP read intervenes between
        // a successful sample and the owned child's SIGKILL.
        // https://www.postgresql.org/docs/current/view-pg-locks.html
        // https://www.postgresql.org/docs/current/functions-info.html
        const SQL: &str = r#"
WITH waiter AS (
    SELECT l.pid FROM pg_locks l JOIN pg_stat_activity s ON s.pid=l.pid
    WHERE l.locktype='advisory' AND NOT l.granted AND l.objsubid=1
      AND l.database=(SELECT oid FROM pg_database WHERE datname=current_database())
      AND l.classid::bigint=(($2::bigint >> 32) & 4294967295)
      AND l.objid::bigint=($2::bigint & 4294967295)
      AND $1=ANY(pg_blocking_pids(l.pid))
      AND s.state='active' AND s.wait_event_type='Lock'
)
SELECT e.key AS execution_id, e.body AS execution_body,
       a.key AS attempt_id, a.body AS attempt_body, a.revision AS attempt_revision,
       a.status AS attempt_status, e.status AS execution_status,
       (SELECT count(*)::bigint FROM waiter) AS waiters,
       (SELECT body::jsonb->>'active' FROM gateway_shared_records
        WHERE namespace=$3 AND kind='tenant' AND key='limited') AS tenant_active,
       (SELECT body::jsonb->>'active' FROM gateway_shared_records
        WHERE namespace=$3 AND kind='resource' AND key='account-pool') AS resource_active,
       (SELECT count(*)::bigint FROM gateway_shared_records o WHERE o.namespace=$3
        AND o.kind='outbox' AND o.body::jsonb->'event'->'execution'->>'execution_id'=e.key) AS outboxes,
       (SELECT count(*)::bigint FROM gateway_shared_metering_requests m
        WHERE m.namespace=$3 AND m.execution_id=e.key) AS metering
FROM gateway_shared_records e JOIN gateway_shared_records a
  ON a.namespace=e.namespace AND a.kind='attempt' AND a.body::jsonb->>'execution_id'=e.key
WHERE e.namespace=$3 AND e.kind='execution'
  AND e.body::jsonb->'view'->>'correlation_id'=$4 AND EXISTS (SELECT 1 FROM waiter)
LIMIT 2
"#;
        tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                let rows = fixture
                    .db
                    .query_all(Statement::from_sql_and_values(
                        DatabaseBackend::Postgres,
                        SQL,
                        [
                            holder.into(),
                            self.key.into(),
                            fixture.authority.store().namespace().into(),
                            boundary.tag().into(),
                        ],
                    ))
                    .await?;
                ensure!(rows.len() <= 1, "ambiguous execution/attempt barrier");
                if let Some(row) = rows.first() {
                    ensure!(
                        row.try_get::<i64>("", "waiters")? == 1,
                        "ambiguous advisory waiters"
                    );
                    ensure!(row.try_get::<String>("", "execution_status")? == "running");
                    ensure!(row.try_get::<String>("", "tenant_active")? == "1");
                    let expected_resource = if matches!(boundary, Boundary::Prepared) {
                        "1"
                    } else {
                        "0"
                    };
                    ensure!(row.try_get::<String>("", "resource_active")? == expected_resource);
                    ensure!(row.try_get::<i64>("", "outboxes")? == 0);
                    ensure!(row.try_get::<i64>("", "metering")? == 0);
                    let execution: String = row.try_get("", "execution_body")?;
                    let attempt: String = row.try_get("", "attempt_body")?;
                    ensure!(execution.len() <= 65536 && attempt.len() <= 65536);
                    let proof = Proof {
                        id: row.try_get("", "execution_id")?,
                        execution: serde_json::from_str(&execution)?,
                        attempt_id: row.try_get("", "attempt_id")?,
                        attempt: serde_json::from_str(&attempt)?,
                        attempt_revision: u64::try_from(
                            row.try_get::<i64>("", "attempt_revision")?,
                        )?,
                    };
                    let status = row.try_get::<String>("", "attempt_status")?;
                    ensure!(
                        status
                            == if matches!(boundary, Boundary::Prepared) {
                                "prepared"
                            } else {
                                "terminal"
                            }
                    );
                    proof.check(boundary)?;
                    return Ok(proof);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .context("no exact advisory waiter with committed boundary state")?
    }

    /// Also called unconditionally after child joins on every outer exit path.
    /// Release first, then remove the fixture-only DDL; both failures are kept.
    pub(super) async fn finish(&mut self, fixture: &Fixture) -> Result<()> {
        let released = if let Some(tx) = self.held.take() {
            tokio::time::timeout(Duration::from_secs(3), tx.rollback())
                .await
                .context("advisory barrier rollback timed out")
                .and_then(|result| result.map_err(Into::into))
        } else {
            Ok(())
        };
        self.holder_pid = None;
        let removed = if let Some(name) = &self.name {
            let result = tokio::time::timeout(Duration::from_secs(4), async {
                let tx = fixture.db.begin().await?;
                bounded_ddl(&tx).await?;
                tx.execute_unprepared(&format!(
                    "DROP TRIGGER IF EXISTS {name} ON gateway_shared_records; DROP FUNCTION IF EXISTS {name}()"
                )).await?;
                tx.commit().await?;
                Ok::<(), anyhow::Error>(())
            }).await.context("fixture trigger removal timed out").and_then(|result| result);
            if result.is_ok() {
                self.name = None;
            }
            result
        } else {
            Ok(())
        };
        combine_results(released, vec![removed])
    }
}

async fn bounded_ddl(tx: &DatabaseTransaction) -> Result<()> {
    tx.execute_unprepared("SET LOCAL statement_timeout='3s'; SET LOCAL lock_timeout='2s'; SET LOCAL idle_in_transaction_session_timeout='5s'").await?;
    Ok(())
}

struct Proof {
    id: String,
    execution: Value,
    attempt_id: String,
    attempt: Value,
    attempt_revision: u64,
}

impl Proof {
    fn view(&self) -> Result<GatewayExecution> {
        Ok(serde_json::from_value(self.execution["view"].clone())?)
    }

    fn check(&self, boundary: Boundary) -> Result<()> {
        let view = self.view()?;
        ensure!(view.execution_id == self.id && view.correlation_id == boundary.tag());
        ensure!(matches!(view.state, ExecutionState::Running));
        ensure!(view.attempts.len() == 1);
        let attempt: GatewayAttempt = serde_json::from_value(self.attempt["view"].clone())?;
        ensure!(view.attempts.first() == Some(&attempt));
        ensure!(attempt.attempt_id == self.attempt_id && attempt.resource_pool_id == ACCOUNT_POOL);
        ensure!(self.attempt["execution_id"] == self.id);
        match boundary {
            Boundary::Prepared => {
                ensure!(matches!(attempt.state, AttemptState::Prepared));
                ensure!(self.attempt["terminal_facts"].is_null());
            }
            Boundary::AttemptTerminal => {
                let AttemptState::Terminal { result } = &attempt.state else {
                    anyhow::bail!("provider terminal is not durable at kill boundary");
                };
                ensure!(result.outcome == TerminalOutcome::Succeeded);
                provider_usage(&result.usage)?;
                ensure!(self.attempt["terminal_facts"].is_object());
            }
        }
        Ok(())
    }

    async fn still_at_boundary(&self, fixture: &Fixture, boundary: Boundary) -> Result<()> {
        let store = fixture.authority.store();
        let execution = store
            .get::<Value>(Kind::Execution, &self.id)
            .await?
            .context("execution vanished after SIGKILL")?;
        let attempt = store
            .get::<Value>(Kind::Attempt, &self.attempt_id)
            .await?
            .context("attempt vanished after SIGKILL")?;
        // Recovery deferral can legitimately advance execution metadata; the
        // visible state, attempt evidence and provider facts must not change.
        let view: GatewayExecution = serde_json::from_value(execution.value["view"].clone())?;
        same_facts(&view, &self.view()?)?;
        ensure!(execution.status() == "running");
        ensure!(attempt.value == self.attempt && attempt.revision()? == self.attempt_revision);
        ensure!(
            attempt.status()
                == if matches!(boundary, Boundary::Prepared) {
                    "prepared"
                } else {
                    "terminal"
                }
        );
        ensure!(
            store
                .get::<Value>(Kind::Outbox, &format!("gateway-terminal-{}", self.id))
                .await?
                .is_none()
        );
        Ok(())
    }
}

pub(super) async fn exercise(
    fixture: &mut Fixture,
    world: &World,
    owner: usize,
    provider: &mut Provider,
    clients: &mut Clients,
    exits: &mut BTreeMap<usize, Exit>,
    barrier: &mut Barrier,
) -> Result<usize> {
    let boundary = barrier.boundary.context("wrong finalization campaign")?;
    barrier.arm(fixture).await?;
    let tag = boundary.tag();
    clients.start(tag, request(world, 0, 0, "account-a:mock", tag, false));
    let provider_execution = match boundary {
        Boundary::Prepared => None,
        Boundary::AttemptTerminal => {
            let arrival = provider.arrival(tag, "account", false).await?;
            let id = arrival.execution.clone();
            let receipt = arrival.end(End::Complete).await?;
            ensure!(receipt.end == End::Complete && receipt.execution == id);
            Some(id)
        }
    };
    let proof = barrier.proof(fixture).await?;
    if let Some(id) = provider_execution {
        ensure!(id == proof.id);
    }
    ensure!(provider.calls() == boundary.calls());
    exits.insert(owner, Exit::Killed);
    // No awaited HTTP/DB read between the positive waiter sample and start_kill.
    // The advisory lock remains owned until after reap and a second state check.
    let deadline = Instant::now() + POST_KILL_WAIT;
    tokio::time::timeout_at(deadline, async {
        kill_owned(fixture.child(owner)?).await?;
        proof.still_at_boundary(fixture, boundary).await?;
        barrier.finish(fixture).await?;
        clients
            .finished(tag)
            .await?
            .interrupted(&proof.id, tag, false)?;
        let terminal = wait_view(world, &proof.id, |state| {
            matches!(state, ExecutionState::Terminal { .. })
        })
        .await?;
        recovered(fixture, &proof, &terminal, boundary).await?;
        let original = effects(fixture, &proof, &terminal, boundary).await?;
        let duplicate = Wire::read(
            request(world, 1, 0, "account-a:mock", tag, false)
                .send()
                .await?,
        )
        .await?;
        duplicate.expect_status(StatusCode::ACCEPTED)?;
        duplicate.identity(&proof.id, tag)?;
        same_facts(&duplicate.execution()?, &terminal)?;
        ensure!(
            effects(fixture, &proof, &terminal, boundary).await? == original,
            "terminal status replay changed immutable terminal effects"
        );
        for (kind, key) in [(Kind::Tenant, "limited"), (Kind::Resource, ACCOUNT_POOL)] {
            let row = fixture
                .authority
                .store()
                .get::<Value>(kind, key)
                .await?
                .context("capacity row missing")?;
            ensure!(
                row.value["active"] == 0,
                "recovered capacity was not released"
            );
        }
        ensure!(
            provider.calls() == boundary.calls(),
            "recovery/replay dispatched provider work"
        );
        Ok(boundary.calls())
    })
    .await
    .context("SIGKILL, recovery and projection exceeded 24s (owner lease is 10s)")?
}

async fn recovered(
    fixture: &Fixture,
    proof: &Proof,
    terminal: &GatewayExecution,
    boundary: Boundary,
) -> Result<()> {
    ensure!(terminal.execution_id == proof.id && terminal.correlation_id == boundary.tag());
    ensure!(terminal.attempts.len() == 1);
    let attempt = terminal
        .attempts
        .first()
        .context("recovered attempt missing")?;
    ensure!(attempt.attempt_id == proof.attempt_id);
    let row = fixture
        .authority
        .store()
        .get::<Value>(Kind::Attempt, &proof.attempt_id)
        .await?
        .context("recovered attempt authority missing")?;
    ensure!(row.value["view"] == serde_json::to_value(attempt)?);
    let ExecutionState::Terminal { result } = &terminal.state else {
        anyhow::bail!("survivor did not finalize the execution");
    };
    match boundary {
        Boundary::Prepared => {
            ensure!(matches!(attempt.state, AttemptState::NotDispatched));
            ensure!(row.status() == "not_dispatched");
            ensure!(row.value["terminal_facts"].is_null());
            ensure!(row.value["attribution"] == proof.attempt["attribution"]);
            ensure!(matches!(&result.outcome, TerminalOutcome::Failed { code }
                if code == "owner_lost_before_dispatch"));
            absent_usage(&result.usage)?;
        }
        Boundary::AttemptTerminal => {
            observed_success(terminal)?;
            ensure!(
                terminal.attempts == proof.view()?.attempts,
                "recovery replaced the original succeeded attempt/usage/timestamp"
            );
            ensure!(row.status() == "terminal" && row.value == proof.attempt);
            ensure!(row.revision()? == proof.attempt_revision);
            let AttemptState::Terminal { result: observed } = &attempt.state else {
                anyhow::bail!("recovered provider terminal missing");
            };
            ensure!(result.usage == observed.usage);
        }
    }
    Ok(())
}

fn absent_usage(usage: &ExecutionUsage) -> Result<()> {
    ensure!(
        [
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.reasoning_tokens,
            usage.cache_read_tokens,
            usage.cache_write_tokens,
            usage.total_tokens,
            usage.cost_micro_usd
        ]
        .into_iter()
        .all(|value| value.is_none()),
        "unsent preparation fabricated usage"
    );
    Ok(())
}

fn provider_usage(usage: &ExecutionUsage) -> Result<()> {
    ensure!(
        usage.prompt_tokens == Some(7)
            && usage.completion_tokens == Some(3)
            && usage.total_tokens == Some(10)
    );
    ensure!(usage.cost_micro_usd.is_none(), "fixture supplied no tariff");
    Ok(())
}

#[derive(PartialEq)]
struct Effects {
    event: Value,
    metering: Value,
    projection: Value,
    projection_revision: u64,
}

async fn effects(
    fixture: &Fixture,
    proof: &Proof,
    terminal: &GatewayExecution,
    boundary: Boundary,
) -> Result<Effects> {
    let store = fixture.authority.store();
    let event_id = format!("gateway-terminal-{}", proof.id);
    loop {
        let outbox = store.get::<Value>(Kind::Outbox, &event_id).await?;
        let projection = store
            .get::<Value>(Kind::ProjectionReceipt, &event_id)
            .await?;
        if let (Some(outbox), Some(projection)) = (outbox, projection)
            && outbox.status() == "delivered"
            && projection.status() == "delivered"
        {
            ensure!(outbox.value["acknowledged_at_ms"].as_i64().is_some());
            ensure!(outbox.value["event"]["event_id"] == event_id);
            ensure!(outbox.value["event"]["tenant_id"] == "limited");
            ensure!(outbox.value["event"]["execution"] == serde_json::to_value(terminal)?);
            ensure!(projection.value["completion"]["sink"] == "none");
            let metering = store.retry(|| async {
                let tx = store.begin().await?;
                let record = SharedMetering::new(store.namespace())?
                    .get_in(&tx, "limited", "fixture-principal-0", &proof.id).await?
                    .context("stock worker did not project recovered metering")?;
                ensure!(record.event.execution == *terminal && record.event.event_id == event_id);
                ensure!(record.attempts.len() == 1 && record.request_id.is_none());
                let attempt = record.attempts.first().context("metering attempt missing")?;
                ensure!(attempt.execution_id == proof.id && attempt.attempt_id == proof.attempt_id);
                ensure!(serde_json::to_value(&attempt.attribution)? == proof.attempt["attribution"]);
                match boundary {
                    Boundary::Prepared => {
                        ensure!(attempt.terminal.is_none());
                        let accounting = record.accounting()?;
                        ensure!(accounting.not_dispatched_attempts == 1);
                        ensure!(accounting.known_attempts == 0 && accounting.unknown_attempts == 0);
                        absent_usage(&accounting.usage)?;
                    }
                    Boundary::AttemptTerminal => {
                        ensure!(serde_json::to_value(&attempt.terminal)? == proof.attempt["terminal_facts"]);
                        let accounting = record.accounting()?;
                        ensure!(accounting.not_dispatched_attempts == 0);
                        provider_usage(&accounting.usage)?;
                    }
                }
                let counts = tx.query_one(Statement::from_sql_and_values(DatabaseBackend::Postgres,
                    "SELECT (SELECT count(*)::bigint FROM gateway_shared_records WHERE namespace=$1 AND kind='outbox') AS outboxes, (SELECT count(*)::bigint FROM gateway_shared_metering_requests WHERE namespace=$1) AS metering",
                    [store.namespace().into()])).await?.context("terminal effect count missing")?;
                ensure!(counts.try_get::<i64>("", "outboxes")? == 1);
                ensure!(counts.try_get::<i64>("", "metering")? == 1);
                tx.commit().await?;
                Ok(serde_json::to_value(record)?)
            }).await?;
            return Ok(Effects {
                event: outbox.value["event"].clone(),
                metering,
                projection_revision: projection.revision()?,
                projection: projection.value,
            });
        }
        // Bounded by the one post-kill deadline in exercise; no separate 25s
        // window can accumulate after lease expiry or the public status wait.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
