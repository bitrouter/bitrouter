//! Stock-process candidates for parts of P1-A03/A08, not acceptance evidence.
//!
//! All authority changes use the stock executable or authenticated HTTP. SQL
//! barriers in the UUID-owned fixture database hold a row or delay one status
//! transition; they never synthesize authority facts or invoke private recovery.

#[path = "recovery/client.rs"]
mod client;
#[path = "recovery/finalization.rs"]
mod finalization;
#[path = "recovery/provider.rs"]
mod provider;

use std::collections::BTreeMap;
use std::os::unix::process::ExitStatusExt;

use bitrouter_ai::gateway::execution::{ResolutionOutcome, ResolveAttemptRequest};
use sea_orm::TransactionTrait;
use tokio::time::Instant;

use super::*;
use client::Clients;
use provider::{End, Provider};

const RECOVERY_WAIT: Duration = Duration::from_secs(25);
const DRAIN_MS: u64 = 10_000;

#[derive(Clone, Copy)]
enum Case {
    KillJson,
    KillStream,
    GracefulStream,
    ForceSignal,
    ForceDeadline,
    BeforeDispatch,
    PreparedBeforeDispatch,
    AttemptTerminalBeforeExecution,
}

#[derive(Clone, Copy)]
enum Exit {
    Killed,
    Interrupted,
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_owner_killed_during_inference_retains_capacity_until_resolution() -> Result<()> {
    run(Case::KillJson).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_owner_killed_after_stream_delta_is_unknown_without_replay() -> Result<()> {
    run(Case::KillStream).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_graceful_drain_finishes_held_stream_while_replica_progresses() -> Result<()> {
    run(Case::GracefulStream).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_second_signal_interrupts_stream_without_success_terminal() -> Result<()> {
    run(Case::ForceSignal).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_drain_deadline_interrupts_stream_without_success_terminal() -> Result<()> {
    run(Case::ForceDeadline).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_owner_killed_at_owned_resource_lock_recovers_before_dispatch() -> Result<()> {
    run(Case::BeforeDispatch).await
}

async fn run(case: Case) -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let mut fixture = match empty_fixture(&mut database).await {
        Ok(fixture) => fixture,
        Err(error) => return database.finish(Err(error)).await,
    };
    let result = async {
        let mut exits = BTreeMap::new();
        let mut clients = Clients::default();
        let mut barrier = finalization::Barrier::new(case);
        let prepared = Provider::start().await;
        let mut provider = match prepared {
            Ok(provider) => provider,
            Err(error) => {
                let cleanup = stop_children(&mut fixture, &exits).await;
                return combine_results(Err(error), vec![cleanup]);
            }
        };
        // Owners live outside the timeout so cancellation cannot discard a
        // gateway, client, mock root, operation, or database cleanup obligation.
        let result = tokio::time::timeout(Duration::from_secs(90), async {
            let (world, owner) = start_world(&mut fixture, &provider.base).await?;
            world.configure_limits().await?;
            let expected = if matches!(case, Case::BeforeDispatch) {
                before_dispatch(
                    &mut fixture,
                    &world,
                    owner,
                    &mut provider,
                    &mut clients,
                    &mut exits,
                )
                .await?
            } else if matches!(
                case,
                Case::PreparedBeforeDispatch | Case::AttemptTerminalBeforeExecution
            ) {
                finalization::exercise(
                    &mut fixture,
                    &world,
                    owner,
                    &mut provider,
                    &mut clients,
                    &mut exits,
                    &mut barrier,
                )
                .await?
            } else {
                dispatched(
                    &mut fixture,
                    &world,
                    owner,
                    &mut provider,
                    &mut clients,
                    &mut exits,
                    case,
                )
                .await?
            };
            clients.consumed()?;
            fixture.no_local_files()?;
            Ok::<usize, anyhow::Error>(expected)
        })
        .await
        .context("stock recovery campaign deadline exceeded")
        .and_then(|result| result);
        let expected = result.as_ref().ok().copied();
        let clients_joined = clients.finish().await;
        let children_joined = stop_children(&mut fixture, &exits).await;
        // Keep the trigger barrier owned until children are joined on failures;
        // releasing it earlier could let a failed prepared case dispatch.
        let barrier_cleaned = barrier.finish(&fixture).await;
        let provider_joined = provider.finish(expected).await;
        combine_results(
            result.map(|_| ()),
            vec![
                clients_joined,
                children_joined,
                barrier_cleaned,
                provider_joined,
            ],
        )
    }
    .await;
    // Like finish_empty_fixture_with_database, preserve all final logs through
    // DB teardown. Its stock exit enum cannot describe a deliberate SIGKILL;
    // stop_children adds only that case and delegates other checks unchanged.
    let result = database.finish(result).await;
    retain_logs(&fixture, result)
}

// This child-local launcher is deliberately separate from the parent's moving
// helper: recovery requires resolve_execution and a 10s drain budget. It uses
// the same stock seed, Fixture, Running owner, config layout and environment.
async fn start_world(fixture: &mut Fixture, base: &str) -> Result<(World, usize)> {
    let keys = std::array::from_fn(|_| crate::auth::keys::generate());
    seed(fixture, &seed_plan(base, &keys)).await?;
    let mut replicas = Vec::new();
    let mut children = Vec::new();
    let mut operators = Vec::new();
    let mut owner = None;
    for index in 0..2 {
        let inference = available_address()?;
        let operator = (0..8)
            .map(|_| available_address())
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .find(|address| *address != inference)
            .context("no distinct operator address")?;
        let child = spawn_replica(fixture, inference, operator).await?;
        children.push(child);
        fixture.child(child)?.ready().await?;
        if index == 0 {
            owner = Some(child);
        }
        replicas.push(inference);
        operators.push(OperatorClient::new(
            &format!("http://{operator}"),
            OPERATOR_TOKEN,
        )?);
    }
    let world = World {
        http: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(40))
            .build()?,
        replicas: replicas
            .try_into()
            .map_err(|_| anyhow::anyhow!("missing replicas"))?,
        children: children
            .try_into()
            .map_err(|_| anyhow::anyhow!("missing child pair"))?,
        operators: operators
            .try_into()
            .map_err(|_| anyhow::anyhow!("missing operators"))?,
        keys,
    };
    Ok((world, owner.context("owner child missing")?))
}

async fn spawn_replica(
    fixture: &mut Fixture,
    address: SocketAddr,
    operator: SocketAddr,
) -> Result<usize> {
    let index = fixture.children.len();
    let config = fixture.root.path().join(format!("gateway-{index}.json"));
    let document = json!({
        "server":{"listen":address.to_string(),"log_level":"info"},
        "database":{"url":"${FIXTURE_SHARED_DATABASE}"},
        "gateway_state":{"mode":"shared","namespace":fixture.authority.store().namespace(),
            "deployment_key_env":"FIXTURE_SHARED_KEY","pool_connections":3,
            "transaction_timeout_ms":1000,"owner_lease_ms":10000,
            "heartbeat_ms":250,"worker_interval_ms":100,"drain_timeout_ms":DRAIN_MS},
        "operator":{"listen":operator.to_string(),"credentials":[{
            "id":"fixture-operator","token_env":"FIXTURE_OPERATOR_TOKEN",
            "grants":["read","policy_write","resolve_execution"]}]}
    });
    tokio::fs::write(&config, serde_json::to_vec(&document)?).await?;
    let log = fixture.root.path().join(format!("gateway-{index}.log"));
    let output = std::fs::File::create(&log)?;
    let child = Command::new(&fixture.binary)
        .arg("serve")
        .arg("--config")
        .arg(&config)
        .current_dir(fixture.root.path().join("launch"))
        .env("FIXTURE_SHARED_DATABASE", &fixture.url)
        .env("FIXTURE_SHARED_KEY", STANDARD.encode(fixture.key))
        .env("FIXTURE_OPERATOR_TOKEN", OPERATOR_TOKEN)
        .env(
            "BITROUTER_HOME",
            fixture.root.path().join("unexpected-home"),
        )
        .env("HOME", fixture.root.path().join("unexpected-user-home"))
        .env_remove("RUST_LOG")
        .env_remove("BITROUTER_WORKFLOW_TRACE_JSONL")
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(output))
        .kill_on_drop(true)
        .spawn()?;
    fixture.children.push(Running {
        child,
        log,
        address,
    });
    Ok(index)
}

fn request(
    w: &World,
    replica: usize,
    principal: usize,
    model: &str,
    tag: &str,
    streaming: bool,
) -> reqwest::RequestBuilder {
    w.post(replica, principal, model, tag, tag, tag)
        .json(&json!({
            "model":model,"messages":[{"role":"user","content":tag}],"stream":streaming
        }))
}

async fn dispatched(
    fixture: &mut Fixture,
    w: &World,
    owner: usize,
    provider: &mut Provider,
    clients: &mut Clients,
    exits: &mut BTreeMap<usize, Exit>,
    case: Case,
) -> Result<usize> {
    let streaming = !matches!(case, Case::KillJson);
    let mut interrupted = None;
    let first = clients.start(
        "held",
        request(w, 0, 0, "account-a:mock", "held", streaming),
    );
    let arrival = provider.arrival("held", "account", streaming).await?;
    let id = arrival.execution.clone();
    let running = view(w, &id).await?;
    ensure!(matches!(running.state, ExecutionState::Running) && running.attempts.len() == 1);
    let attempt = running
        .attempts
        .first()
        .context("dispatched attempt missing")?;
    ensure!(
        matches!(attempt.state, AttemptState::Dispatched)
            && attempt.resource_pool_id == ACCOUNT_POOL
    );
    if streaming {
        let head = tokio::time::timeout(Duration::from_secs(4), first)
            .await
            .context("client never received a stream delta")??;
        head.expect_status(StatusCode::OK)?;
        head.identity(&id, "held")?;
    }
    ensure!(provider.calls() == 1);
    match case {
        Case::KillJson | Case::KillStream => {
            exits.insert(owner, Exit::Killed);
            kill_owned(fixture.child(owner)?).await?;
        }
        Case::GracefulStream | Case::ForceSignal | Case::ForceDeadline => {
            let signalled = Instant::now();
            signal_term(fixture.child(owner)?).await?;
            withdrawn(fixture, w, owner, &id).await?;
            // Positive dispatch, full client response and durable success on
            // the survivor while the original provider operation remains held.
            progress(
                w,
                provider,
                clients,
                3,
                "other:mock",
                "other",
                "during-drain",
            )
            .await?;
            ensure!(
                fixture.child(owner)?.child.try_wait()?.is_none(),
                "owner exited before drain observation"
            );
            ensure!(matches!(view(w, &id).await?.state, ExecutionState::Running));
            if matches!(case, Case::GracefulStream) {
                arrival.end(End::Complete).await?;
                clients.finished("held").await?.success(&id, "held", true)?;
                let terminal = wait_view(w, &id, succeeded).await?;
                observed_success(&terminal)?;
                ensure!(
                    wait_exit(fixture.child(owner)?).await?.success(),
                    "graceful drain failed"
                );
                progress(
                    w,
                    provider,
                    clients,
                    0,
                    "account-b:mock",
                    "account",
                    "after-grace",
                )
                .await?;
                // Rejoin a new stock incarnation at the drained address. The
                // survivor remains live, and old terminal history is available
                // through the replacement without migrating the finished stream.
                let operator = (0..16)
                    .map(|_| available_address())
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .find(|address| *address != w.replicas[0])
                    .context("no distinct replacement operator address")?;
                let replacement = spawn_replica(fixture, w.replicas[0], operator).await?;
                fixture.child(replacement)?.ready().await?;
                let old = w.status(0, 0, &id).await?;
                old.expect_status(StatusCode::OK)?;
                same_facts(&old.execution()?, &terminal)?;
                clients.start(
                    "replacement",
                    request(w, 0, 0, "account-a:mock", "replacement", false),
                );
                let arrived = provider.arrival("replacement", "account", false).await?;
                let new_id = arrived.execution.clone();
                arrived.end(End::Complete).await?;
                clients
                    .finished("replacement")
                    .await?
                    .success(&new_id, "replacement", false)?;
                let new = w.wait_state(1, 0, &new_id, succeeded).await?;
                observed_success(&new)?;
                let store = fixture.authority.store();
                let old_owner = store
                    .get::<Value>(Kind::Execution, &id)
                    .await?
                    .context("old execution missing")?;
                let new_owner = store
                    .get::<Value>(Kind::Execution, &new_id)
                    .await?
                    .context("replacement execution missing")?;
                ensure!(old_owner.value["owner"] != new_owner.value["owner"]);
                ensure!(fixture.child(replacement)?.child.try_wait()?.is_none());
                ensure!(fixture.child(w.children[1])?.child.try_wait()?.is_none());
                return Ok(4);
            }
            exits.insert(owner, Exit::Interrupted);
            if matches!(case, Case::ForceSignal) {
                ensure!(
                    signalled.elapsed() < Duration::from_millis(DRAIN_MS - 2000),
                    "second signal missed grace window"
                );
                let second_signal = Instant::now();
                signal_term(fixture.child(owner)?).await?;
                let early = signalled + Duration::from_millis(DRAIN_MS - 1000);
                let trace = tokio::time::timeout_at(early, clients.finished("held"))
                    .await
                    .context(
                        "second signal did not interrupt before the original grace deadline",
                    )??;
                ensure!(trace.completed_at >= second_signal && trace.completed_at < early);
                trace.interrupted(&id, "held", true)?;
                interrupted = Some(trace);
            }
            // Force may be selected by the second signal or the original
            // absolute deadline. Do not turn a test-side client timeout into
            // evidence: first observe/reap the stock process's own exit.
            let status = wait_exit(fixture.child(owner)?).await?;
            check_exit(status, Some(Exit::Interrupted))?;
            let logs = fixture.child(owner)?.logs();
            ensure!(logs.contains("shared gateway shutdown completed"));
            ensure!(logs.contains("shared gateway stopped with interrupted or unresolved work"));
            if matches!(case, Case::ForceDeadline) {
                let trace = clients.finished("held").await?;
                ensure!(
                    trace.completed_at >= signalled + Duration::from_millis(DRAIN_MS),
                    "stream interrupted before the configured drain deadline"
                );
                interrupted = Some(trace);
            }
        }
        Case::BeforeDispatch
        | Case::PreparedBeforeDispatch
        | Case::AttemptTerminalBeforeExecution => {
            anyhow::bail!("wrong dispatched campaign")
        }
    }
    let interrupted = match interrupted {
        Some(trace) => trace,
        None => clients.finished("held").await?,
    };
    interrupted.interrupted(&id, "held", streaming)?;
    let retained = wait_view(w, &id, unknown).await?;
    ensure!(retained.attempts.len() == 1 && retained.attempts[0].attempt_id == attempt.attempt_id);
    ensure!(matches!(
        retained.attempts[0].state,
        AttemptState::Unknown { .. }
    ));
    if matches!(case, Case::KillJson | Case::KillStream) {
        ensure!(
            matches!(&retained.state, ExecutionState::Unknown { reason } if reason == "owner_lease_expired")
        );
        progress(w, provider, clients, 3, "other:mock", "other", "after-kill").await?;
    }
    retained_capacity(w, provider, &retained, streaming).await?;
    // This mock has acknowledged an explicit end of the independently owned
    // operation. A killed gateway / broken socket alone never authorizes this.
    let receipt = arrival.end(End::Terminate).await?;
    resolve(w, &retained, receipt).await?;
    progress(
        w,
        provider,
        clients,
        3,
        "account-b:mock",
        "account",
        "released-account",
    )
    .await?;
    progress(
        w,
        provider,
        clients,
        0,
        "tenant:mock",
        "tenant",
        "released-tenant",
    )
    .await?;
    let resolved = view(w, &id).await?;
    ensure!(matches!(resolved.state, ExecutionState::Resolved { .. }));
    ensure!(resolved.attempts.len() == 1);
    ensure!(provider.calls() == 4);
    Ok(4)
}

async fn view(w: &World, id: &str) -> Result<GatewayExecution> {
    let wire = w.status(1, 0, id).await?;
    wire.expect_status(StatusCode::OK)?;
    let view = wire.execution()?;
    wire.identity(id, &view.correlation_id)?;
    Ok(view)
}

async fn wait_view(
    w: &World,
    id: &str,
    predicate: fn(&ExecutionState) -> bool,
) -> Result<GatewayExecution> {
    tokio::time::timeout(RECOVERY_WAIT, async {
        loop {
            let current = view(w, id).await?;
            if predicate(&current.state) {
                return Ok(current);
            }
            ensure!(
                !matches!(
                    current.state,
                    ExecutionState::Terminal { .. } | ExecutionState::Resolved { .. }
                ),
                "unexpected final state: {:?}",
                current.state
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("surviving replica did not expose the required durable state within 25s")?
}

async fn progress(
    w: &World,
    provider: &mut Provider,
    clients: &mut Clients,
    principal: usize,
    model: &str,
    account: &str,
    tag: &str,
) -> Result<()> {
    let before = provider.calls();
    clients.start(tag, request(w, 1, principal, model, tag, false));
    let arrival = provider.arrival(tag, account, false).await?;
    let id = arrival.execution.clone();
    arrival.end(End::Complete).await?;
    clients.finished(tag).await?.success(&id, tag, false)?;
    let terminal = w.wait_state(1, principal, &id, succeeded).await?;
    observed_success(&terminal)?;
    ensure!(
        provider.calls() == before + 1,
        "progress dispatched more than once"
    );
    Ok(())
}

fn observed_success(view: &GatewayExecution) -> Result<()> {
    let ExecutionState::Terminal { result } = &view.state else {
        anyhow::bail!("success was not durable");
    };
    ensure!(matches!(result.outcome, TerminalOutcome::Succeeded));
    ensure!(
        result.usage.prompt_tokens == Some(7)
            && result.usage.completion_tokens == Some(3)
            && result.usage.total_tokens == Some(10),
        "observed provider usage was lost"
    );
    ensure!(
        result.usage.cost_micro_usd.is_none(),
        "fixture supplied no price"
    );
    Ok(())
}

async fn retained_capacity(
    w: &World,
    provider: &Provider,
    retained: &GatewayExecution,
    streaming: bool,
) -> Result<()> {
    let before = provider.calls();
    let duplicate = Wire::read(
        request(w, 1, 0, "account-a:mock", "held", streaming)
            .send()
            .await?,
    )
    .await?;
    duplicate.expect_status(StatusCode::ACCEPTED)?;
    duplicate.identity(&retained.execution_id, "held")?;
    same_facts(&duplicate.execution()?, retained)?;
    // Different principal, same tenant; and different tenant, aliased account.
    for (principal, model, tag) in [
        (1, "tenant:mock", "tenant-retained"),
        (3, "account-b:mock", "account-retained"),
    ] {
        let denied = Wire::read(request(w, 1, principal, model, tag, false).send().await?).await?;
        denied.expect_status(StatusCode::TOO_MANY_REQUESTS)?;
    }
    same_facts(&view(w, &retained.execution_id).await?, retained)?;
    ensure!(
        provider.calls() == before,
        "unknown/duplicate/capacity rejection caused redispatch"
    );
    Ok(())
}

async fn resolve(w: &World, retained: &GatewayExecution, receipt: provider::Receipt) -> Result<()> {
    ensure!(receipt.execution == retained.execution_id && receipt.end == End::Terminate);
    let attempt = retained
        .attempts
        .first()
        .context("unknown attempt missing")?;
    let mutation = Mutation {
        operation_id: format!("resolve-{}", retained.execution_id),
        command: MutationCommand::ResolveExecution {
            execution_id: retained.execution_id.clone(),
            attempt_id: attempt.attempt_id.clone(),
            resolution: ResolveAttemptRequest {
                expected_revision: retained.revision,
                outcome: ResolutionOutcome::Terminated,
                evidence_reference: receipt.reference.clone(),
            },
        },
    };
    let committed = w.operators[1].apply(&mutation).await?;
    ensure!(
        matches!(w.operators[1].operation(&mutation.operation_id).await?,
        OperationLookup::Committed { receipt } if *receipt == committed)
    );
    // Retrying the same operator operation must return the same receipt rather
    // than releasing capacity twice or creating new provider work.
    ensure!(w.operators[1].apply(&mutation).await? == committed);
    let resolved = view(w, &retained.execution_id).await?;
    let ExecutionState::Resolved { resolution } = &resolved.state else {
        anyhow::bail!("public status did not expose operator resolution");
    };
    ensure!(
        resolution.operator_id == "fixture-operator"
            && resolution.outcome == ResolutionOutcome::Terminated
            && resolution.evidence_reference == receipt.reference
    );
    ensure!(resolved.attempts.len() == 1 && resolved.attempts[0].attempt_id == attempt.attempt_id);
    ensure!(
        matches!(&resolved.attempts[0].state, AttemptState::Resolved { resolution: other } if other == resolution)
    );
    ensure!(
        resolved.revision == retained.revision + 1
            && resolved.correlation_id == retained.correlation_id
    );
    Ok(())
}

async fn withdrawn(fixture: &mut Fixture, w: &World, child: usize, id: &str) -> Result<()> {
    let record = fixture
        .authority
        .store()
        .get::<Value>(Kind::Execution, id)
        .await?
        .context("missing owned execution")?;
    let owner = record.value["owner"]
        .as_str()
        .context("execution owner missing")?
        .to_owned();
    let health = reqwest::Client::builder()
        .no_proxy()
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_millis(500))
        .build()?;
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            ensure!(
                fixture.child(child)?.child.try_wait()?.is_none(),
                "owner died instead of withdrawing readiness"
            );
            let instance = fixture
                .authority
                .store()
                .get::<Value>(Kind::Instance, &owner)
                .await?
                .context("owner record vanished")?;
            let front_door_closed = match health
                .get(format!("http://{}/health", w.replicas[0]))
                .send()
                .await
            {
                Ok(response) => {
                    ensure!(matches!(
                        response.status(),
                        StatusCode::OK | StatusCode::SERVICE_UNAVAILABLE
                    ));
                    response.status() == StatusCode::SERVICE_UNAVAILABLE
                }
                Err(error) => {
                    ensure!(
                        connection_refused(&error),
                        "health error is not readiness-withdrawal evidence: {error}"
                    );
                    true
                }
            };
            if instance.status() == "draining"
                && instance.value["accepting"] == false
                && front_door_closed
            {
                ensure!(fixture.child(child)?.child.try_wait()?.is_none());
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("no positive readiness withdrawal while the stream was held")?
}

fn connection_refused(error: &reqwest::Error) -> bool {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(error) = current {
        if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::ConnectionRefused)
        {
            return true;
        }
        current = error.source();
    }
    false
}

async fn signal_term(running: &mut Running) -> Result<()> {
    ensure!(
        running.child.try_wait()?.is_none(),
        "owned child exited before signal"
    );
    let pid = running.child.id().context("owned child has no PID")?;
    let status = tokio::time::timeout(
        Duration::from_secs(2),
        Command::new("/bin/kill")
            .args(["-TERM", &pid.to_string()])
            .kill_on_drop(true)
            .status(),
    )
    .await??;
    ensure!(status.success(), "could not signal owned child");
    Ok(())
}

async fn kill_owned(running: &mut Running) -> Result<()> {
    ensure!(
        running.child.try_wait()?.is_none(),
        "owned child exited before kill barrier"
    );
    running.child.start_kill()?;
    let status = tokio::time::timeout(Duration::from_secs(5), running.child.wait()).await??;
    check_exit(status, Some(Exit::Killed))
}

async fn wait_exit(running: &mut Running) -> Result<ExitStatus> {
    tokio::time::timeout(Duration::from_millis(DRAIN_MS + 5000), running.child.wait())
        .await
        .context("stock shutdown exceeded its drain/join observation budget")?
        .context("joining owned child")
}

fn check_exit(status: ExitStatus, expected: Option<Exit>) -> Result<()> {
    match expected {
        Some(Exit::Killed) => ensure!(
            status.signal() == Some(9),
            "owned hard kill did not produce SIGKILL: {status}"
        ),
        Some(Exit::Interrupted) => ensure!(
            status.code() == Some(1),
            "forced shared shutdown returned an unexpected status: {status}"
        ),
        None => ensure!(status.success(), "unexpected stock child exit: {status}"),
    }
    Ok(())
}

async fn stop_children(fixture: &mut Fixture, exits: &BTreeMap<usize, Exit>) -> Result<()> {
    // Join every retained owner before interpreting any failure. Intentional
    // hard kills have no shutdown report; all other children use Leibniz's
    // current strict flag/envelope validator, including bootstrap exit status.
    let stopped = futures::future::join_all(fixture.children.iter_mut().map(Running::stop)).await;
    let mut results = Vec::new();
    let prefix = failure_log_prefix();
    for (index, (child, status)) in fixture.children.iter_mut().zip(stopped).enumerate() {
        if matches!(exits.get(&index), Some(Exit::Killed)) {
            results.push(
                status
                    .and_then(|status| check_exit(status, Some(Exit::Killed)))
                    .with_context(|| format!("joining killed owner {index}")),
            );
        } else {
            let expectation = if matches!(exits.get(&index), Some(Exit::Interrupted)) {
                ExpectedExit::ObservedUnknown
            } else if child.address.port() == 0 {
                ExpectedExit::Success
            } else {
                ExpectedExit::CleanServe
            };
            let checked = status.and_then(|status| check_exit(status, exits.get(&index).copied()));
            let expected = ExitExpectations::from([(0, expectation)]);
            // stop() now observes the already-reaped Child; the parent's helper
            // still owns interpretation of completed shutdown diagnostics.
            results.push(
                finish_children(std::slice::from_mut(child), checked, &expected, &prefix)
                    .await
                    .with_context(|| format!("checking stock owner {index}")),
            );
        }
    }
    combine_results(Ok(()), results)
}

fn retain_logs(fixture: &Fixture, result: Result<()>) -> Result<()> {
    if result.is_ok() {
        return result;
    }
    let prefix = std::env::temp_dir().join(format!(
        "bitrouter-recovery-failure-{}",
        uuid::Uuid::new_v4()
    ));
    let copied = fixture
        .children
        .iter()
        .enumerate()
        .map(|(index, child)| {
            std::fs::copy(&child.log, prefix.with_extension(format!("{index}.log")))
                .map(|_| ())
                .context("retaining stock child failure log")
        })
        .collect();
    combine_results(result, copied)
        .with_context(|| format!("child logs: {}.*.log", prefix.display()))
}

async fn before_dispatch(
    fixture: &mut Fixture,
    w: &World,
    owner: usize,
    provider: &mut Provider,
    clients: &mut Clients,
    exits: &mut BTreeMap<usize, Exit>,
) -> Result<usize> {
    let tx = fixture.db.begin().await?;
    let gated = tokio::time::timeout(Duration::from_secs(8), async {
        tx.execute(Statement::from_string(
            DatabaseBackend::Postgres,
            "SET LOCAL statement_timeout = '3s'".to_owned(),
        ))
        .await?;
        tx.execute(Statement::from_string(
            DatabaseBackend::Postgres,
            "SET LOCAL idle_in_transaction_session_timeout = '10s'".to_owned(),
        ))
        .await?;
        let pid: i32 = tx
            .query_one(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT pg_backend_pid() AS pid".to_owned(),
            ))
            .await?
            .context("barrier PID missing")?
            .try_get("", "pid")?;
        let resource = fixture
            .authority
            .store()
            .lock::<Value>(&tx, Kind::Resource, ACCOUNT_POOL)
            .await?;
        ensure!(resource.value["active"] == 0);
        clients.start(
            "before",
            request(w, 0, 0, "account-a:mock", "before", false),
        );
        let id = accepted_id(fixture, "before").await?;
        wait_blocker(fixture, pid).await?;
        // Preparation holds principal 0's auth row while blocked. Observe with
        // a different key in the same tenant, without waiting on that row.
        let wire = w.status(1, 1, &id).await?;
        wire.expect_status(StatusCode::OK)?;
        let accepted = wire.execution()?;
        wire.identity(&id, &accepted.correlation_id)?;
        ensure!(matches!(accepted.state, ExecutionState::Accepted) && accepted.attempts.is_empty());
        ensure!(provider.calls() == 0);
        wait_blocker(fixture, pid).await?;
        exits.insert(owner, Exit::Killed);
        // The resource row stays locked through confirmed SIGKILL. Whether the
        // waiting transaction aborts before or after this kill, dispatch cannot
        // cross the barrier. The later exact failure code proves recovery of
        // admission rather than an ordinary pre-kill request rejection.
        kill_owned(fixture.child(owner)?).await?;
        Ok::<String, anyhow::Error>(id)
    })
    .await
    .context("pre-dispatch lock barrier was not established in time")
    .and_then(|result| result);
    let rollback = tokio::time::timeout(Duration::from_secs(3), tx.rollback())
        .await
        .context("fixture barrier rollback timed out")
        .and_then(|result| result.map_err(Into::into));
    let id = match gated {
        Ok(id) => {
            rollback?;
            id
        }
        Err(error) => {
            return combine_results(Err(error), vec![rollback]).map(|()| 0);
        }
    };
    clients
        .finished("before")
        .await?
        .interrupted(&id, "before", false)?;
    let terminal = wait_view(w, &id, |state| {
        matches!(state, ExecutionState::Terminal { .. })
    })
    .await?;
    let ExecutionState::Terminal { result } = &terminal.state else {
        anyhow::bail!("missing pre-dispatch terminal");
    };
    ensure!(
        matches!(&result.outcome, TerminalOutcome::Failed { code } if code == "owner_lost_before_dispatch")
    );
    ensure!(terminal.attempts.is_empty());
    ensure!(
        result.usage.prompt_tokens.is_none()
            && result.usage.completion_tokens.is_none()
            && result.usage.reasoning_tokens.is_none()
            && result.usage.cache_read_tokens.is_none()
            && result.usage.cache_write_tokens.is_none()
            && result.usage.total_tokens.is_none()
            && result.usage.cost_micro_usd.is_none(),
        "pre-dispatch recovery fabricated usage"
    );
    let duplicate = Wire::read(
        request(w, 1, 0, "account-a:mock", "before", false)
            .send()
            .await?,
    )
    .await?;
    duplicate.expect_status(StatusCode::ACCEPTED)?;
    duplicate.identity(&id, "before")?;
    same_facts(&duplicate.execution()?, &terminal)?;
    ensure!(provider.calls() == 0);
    progress(
        w,
        provider,
        clients,
        3,
        "other:mock",
        "other",
        "before-survivor",
    )
    .await?;
    progress(
        w,
        provider,
        clients,
        0,
        "account-b:mock",
        "account",
        "before-released",
    )
    .await?;
    Ok(2)
}

async fn accepted_id(fixture: &Fixture, tag: &str) -> Result<String> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let rows = fixture.db.query_all(Statement::from_sql_and_values(DatabaseBackend::Postgres,
                "SELECT key FROM gateway_shared_records WHERE namespace = $1 AND kind = 'execution' AND body::jsonb->'view'->>'correlation_id' = $2",
                [fixture.authority.store().namespace().into(), tag.into()])).await?;
            ensure!(rows.len() <= 1, "fixture correlation matched multiple admissions");
            if let Some(row) = rows.first() { return Ok(row.try_get("", "key")?); }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.context("no durable admission at pre-dispatch barrier")?
}

async fn wait_blocker(fixture: &Fixture, pid: i32) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            // The only lock held by this backend is the named resource row in
            // this fresh database. No inference/control request targets it from
            // the survivor. Require a real lock waiter; elapsed time is no proof.
            let row = fixture.db.query_one(Statement::from_sql_and_values(DatabaseBackend::Postgres,
                "SELECT count(*)::bigint AS n FROM pg_stat_activity WHERE datname = current_database() AND pid <> pg_backend_pid() AND state = 'active' AND wait_event_type = 'Lock' AND $1 = ANY(pg_blocking_pids(pid)) AND query LIKE '%gateway_shared_records%'",
                [pid.into()])).await?.context("lock waiter query missing")?;
            let count: i64 = row.try_get("", "n")?;
            ensure!(count <= 1, "ambiguous fixture lock waiters");
            if count == 1 { return Ok(()); }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.context("no database-observed provider reservation waiter")?
}
