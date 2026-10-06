//! Crash the shipped CLI, retaining only the independent harness's journal.

use super::*;
use bitrouter_orchestrator::core::accounting::work::{CostWorkKind, CostWorkState};
use bitrouter_orchestrator::core::protocol::{
    RunActivityReconciliation, ToolEffect, ToolObservation, ToolStatus,
};
use std::io::Write;
use std::path::PathBuf;
use std::process::Stdio;

#[path = "process/inflight.rs"]
mod inflight;

struct Host {
    child: tokio::process::Child,
    base: String,
    instance: String,
}

struct Environment {
    home: tempfile::TempDir,
    key: String,
    upstream: MockServer,
}

enum Recovery {
    NotStarted,
    RetainedResult,
    EffectUnknown,
}

impl Environment {
    async fn new() -> Result<Self> {
        let home = tempfile::tempdir()?;
        let database = bitrouter::db::connect(&format!(
            "sqlite://{}",
            home.path().join("auth.db").display()
        ))
        .await?;
        bitrouter::db::run_migrations(&database).await?;
        db::upsert_user(&database, "process-owner").await?;
        let key = keys::generate();
        db::insert_api_key(
            &database,
            &db::NewApiKey {
                id: "process-key".into(),
                key_hash: key.hash,
                user_id: "process-owner".into(),
                spend_limit_micro_usd: None,
                rpm_limit: None,
                policy_id: None,
            },
        )
        .await?;
        database.close().await?;
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses/input_tokens"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"object":"response.input_tokens","input_tokens":20})),
            )
            .mount(&upstream)
            .await;
        Mock::given(method("POST")).and(path("/v1/responses"))
            .respond_with(|request: &Request| {
                let done = String::from_utf8_lossy(&request.body).contains("function_call_output");
                ResponseTemplate::new(200).set_body_json(json!({
                    "id":"process-response", "object":"response", "status":"completed", "model":"served",
                    "output": if done {json!([{"id":"message","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"file effect verified","annotations":[]}]}])}
                    else {json!([{"id":"call","type":"function_call","call_id":"provider-call","name":"write","arguments":"{\"path\":\"file.txt\"}","status":"completed"}])},
                    "usage":{"input_tokens":20,"output_tokens":10,"total_tokens":30}
                }))
            }).mount(&upstream).await;
        Ok(Self {
            home,
            key: key.secret,
            upstream,
        })
    }

    async fn start(&self) -> Result<Host> {
        self.start_with_provider(&self.upstream.uri()).await
    }

    async fn start_with_provider(&self, endpoint: &str) -> Result<Host> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let config = self.home.path().join("bitrouter.yaml");
        tokio::fs::write(
            &config,
            format!(
                r#"
inherit_defaults: false
registry:
  enabled: false
server:
  listen: {address}
  skip_auth: false
database:
  url: 'sqlite://./auth.db'
providers:
  fixture:
    api_base: {endpoint}/v1
    api_key: process-provider-fixture
    models:
      - id: served
        api_protocol: responses
        capabilities: [tools]
        input_token_counting: responses
        token_limits:
          max_input_tokens: 4000
          max_output_tokens: 128
          context_window: 8000
models:
  fixture-model:
    endpoints:
      - provider: fixture
        service_id: served
"#
            ),
        )
        .await?;
        let log_path = self.home.path().join("core.log");
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        drop(listener);
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_bro"))
            .arg("serve")
            .arg("--config")
            .arg(&config)
            .current_dir(self.home.path())
            .env("BITROUTER_HOME", self.home.path())
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()?;
        let base = format!("http://{address}");
        let client = fixture_http_client()
            .timeout(Duration::from_secs(1))
            .build()?;
        let capability = tokio::time::timeout(GUARD, async {
            loop {
                if let Some(status) = child.try_wait()? {
                    anyhow::bail!(
                        "CLI exited during startup: {status}: {}",
                        tokio::fs::read_to_string(&log_path).await?
                    );
                }
                if let Ok(response) = client
                    .get(format!("{base}/v1/orchestrator/capabilities"))
                    .bearer_auth(&self.key)
                    .header("bitrouter-beta", "orchestrator_core=v1")
                    .send()
                    .await
                    && response.status().is_success()
                {
                    return Ok::<_, anyhow::Error>(response.json::<Capabilities>().await?);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await??;
        Ok(Host {
            child,
            base,
            instance: capability.core_instance_id,
        })
    }

    async fn peer(&self, host: &Host, epoch: u64, store: Store) -> Result<Peer> {
        let mut request = format!(
            "{}/v1/orchestrator/channel",
            host.base.replace("http://", "ws://")
        )
        .into_client_request()?;
        request
            .headers_mut()
            .insert("authorization", format!("Bearer {}", self.key).parse()?);
        request
            .headers_mut()
            .insert("bitrouter-beta", "orchestrator_core=v1".parse()?);
        let socket = tokio_tungstenite::connect_async(request).await?.0;
        let mut grant = binding(Limits::default())?.grant;
        grant.core_instance_id = host.instance.clone();
        grant.execution_epoch = epoch;
        // Grants come from the harness, independently of checkpoint contents.
        tokio::fs::write(
            self.home.path().join(format!("grant-{epoch}.json")),
            serde_json::to_vec(&grant)?,
        )
        .await?;
        Ok(Peer {
            socket,
            grant,
            store,
        })
    }
}

impl Host {
    async fn crash(&mut self) -> Result<()> {
        // kill(), unlike graceful shutdown, cannot checkpoint pending state.
        tokio::time::timeout(GUARD, self.child.kill()).await??;
        Ok(())
    }
}

async fn raw_post(host: &Host, key: &str, body: &Value) -> Result<reqwest::Response> {
    Ok(fixture_http_client()
        .timeout(GUARD)
        .build()?
        .post(format!("{}/v1/responses", host.base))
        .bearer_auth(key)
        .header("bitrouter-beta", "orchestrator_core=v1")
        .json(body)
        .send()
        .await?)
}

async fn post_host(host: &Host, key: &str, body: &Value) -> Result<Value> {
    let response = raw_post(host, key, body).await?;
    let status = response.status();
    let body: Value = response.json().await?;
    anyhow::ensure!(status.is_success(), "HTTP {status}: {body}");
    Ok(body)
}

fn snapshot(batch: &CheckpointBatch) -> Result<SessionSnapshot> {
    Ok(serde_json::from_value(
        batch.decode(&Limits::default())?.checkpoint.state,
    )?)
}

fn process_binding(grant: OwnershipGrant) -> Result<Bind> {
    let mut value = binding(Limits::default())?;
    value.grant = grant;
    value.manifest.tools[0].name = "write".into();
    value.manifest.tools[0].description = "Append one recorded effect to file.txt".into();
    value.manifest.tools[0].effect = ToolEffect::Write;
    value.manifest.tool_manifest_digest = HarnessManifest::digest(&value.manifest.tools)?;
    Ok(value)
}

async fn until_checkpoint(
    peer: &mut Peer,
    host: &Host,
    key: &str,
    request: &Value,
    kind: &str,
) -> Result<CheckpointBatch> {
    let response = post_host(host, key, request);
    tokio::pin!(response);
    tokio::time::timeout(GUARD, async {
        loop {
            tokio::select! {
                value = &mut response => anyhow::bail!("HTTP finished before {kind}: {value:?}"),
                message = peer.receive() => match message? {
                    ServerMessage::Checkpoint(batch) => {
                        if batch.decode(&Limits::default())?.events.iter().any(|event| event.kind == kind) {
                            return Ok(batch);
                        }
                        peer.acknowledge(batch).await?;
                    }
                    other => anyhow::bail!("unexpected message before {kind}: {other:?}"),
                }
            }
        }
    }).await?
}

async fn exchange(
    peer: &mut Peer,
    host: &Host,
    key: &str,
    request: &Value,
) -> Result<(Value, Vec<ToolExecute>)> {
    let response = post_host(host, key, request);
    tokio::pin!(response);
    let mut commands = Vec::new();
    tokio::time::timeout(GUARD, async {
        loop {
            tokio::select! {
                value = &mut response => return Ok((value?, commands)),
                message = peer.receive() => match message? {
                    ServerMessage::Checkpoint(batch) => peer.acknowledge(batch).await?,
                    ServerMessage::ToolExecute(command) => commands.push(*command),
                    other => anyhow::bail!("unexpected exchange message: {other:?}"),
                }
            }
        }
    })
    .await?
}

async fn command(peer: &mut Peer, commands: Vec<ToolExecute>) -> Result<ToolExecute> {
    anyhow::ensure!(commands.len() <= 1, "duplicate workspace command");
    if let Some(command) = commands.into_iter().next() {
        return Ok(command);
    }
    tokio::time::timeout(GUARD, async {
        loop {
            match peer.receive().await? {
                ServerMessage::Checkpoint(batch) => peer.acknowledge(batch).await?,
                ServerMessage::ToolExecute(command) => return Ok(*command),
                other => anyhow::bail!("expected workspace command: {other:?}"),
            }
        }
    })
    .await?
}

async fn status(peer: &mut Peer, command: &ToolExecute, status: ToolStatus) -> Result<()> {
    let operation = format!("status-{status:?}");
    let observation = ToolObservation {
        invocation_id: command.invocation_id.clone(),
        attempt_id: command.attempt_id.clone(),
        status,
        evidence: Vec::new(),
    };
    peer.send(peer.command(
        &operation,
        "tool.status",
        serde_json::to_value(observation)?,
    ))
    .await?;
    peer.receipt(&operation).await
}

async fn execute(env: &Environment, peer: &mut Peer, command: &ToolExecute) -> Result<ToolResult> {
    assert_eq!(command.tool, "write");
    assert!(command.authorizing_event_seq <= peer.store.head.event_seq);
    status(peer, command, ToolStatus::Running).await?;
    let path = env.home.path().join("file.txt");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(b"effect\n")?;
    file.sync_all()?;
    let result = ToolResult {
        invocation_id: command.invocation_id.clone(),
        attempt_id: command.attempt_id.clone(),
        status: ToolOutcome::Succeeded,
        output: "file effect verified".into(),
        evidence: Vec::new(),
        workspace_revision: None,
    };
    // The independent harness retains the actual result before any core ACK.
    tokio::fs::write(
        env.home.path().join("tool-result.json"),
        serde_json::to_vec(&result)?,
    )
    .await?;
    status(peer, command, ToolStatus::Stopped).await?;
    Ok(result)
}

fn continuation(first: &Value, result: &ToolResult, epoch: u64) -> Result<Value> {
    let call = first["output"]
        .as_array()
        .context("output")?
        .iter()
        .find(|item| item["type"] == "function_call")
        .context("function call")?;
    let mut request = create("continue");
    request["bitrouter"]["execution_epoch"] = json!(epoch);
    request["previous_response_id"] = first["id"].clone();
    request["input"] = json!([{"type":"function_call_output", "call_id":call["call_id"],
        "output":result.output,"bitrouter":{"operation_id":"result","status":"succeeded"}}]);
    Ok(request)
}

async fn replace(
    env: &Environment,
    original: &mut Host,
    mut peer: Peer,
    proposal: &CheckpointBatch,
    persisted: bool,
    recovery: Recovery,
) -> Result<(Host, Peer)> {
    if persisted {
        peer.persist(proposal.clone())?;
    }
    let quiescent = snapshot(proposal)?;
    let run = quiescent.run.as_ref().context("run")?;
    // Every selected barrier follows recorded provider/tool completion or
    // precedes dispatch. The withheld ACK prevents more work. This exercises
    // quiescent handoff, not the separately unsupported remote Running case.
    let active_ms = run.active_ms;
    let run_id = run.run_id.clone();
    let epoch = peer.grant.execution_epoch + 1;
    let retained = serde_json::to_vec(&peer.store.batches)?;
    let journal: PathBuf = env.home.path().join("harness-journal.json");
    tokio::fs::write(&journal, &retained).await?;
    original.crash().await?;
    drop(peer.socket);
    let replacement = env.start().await?;
    assert_ne!(replacement.instance, original.instance);
    let store = read_journal(env, &retained).await?;
    let state = snapshot(store.batches.last().context("checkpoint")?)?;
    let result = match recovery {
        Recovery::RetainedResult => Some(serde_json::from_slice::<ToolResult>(
            &tokio::fs::read(env.home.path().join("tool-result.json")).await?,
        )?),
        _ => None,
    };
    let mut tools = Vec::new();
    let mut results = Vec::new();
    for call in state
        .agents
        .values()
        .filter_map(|agent| agent.turn.as_ref())
        .flat_map(|turn| &turn.invocations)
        .filter(|call| {
            call.result.is_none()
                || (!call.consumed
                    && result
                        .as_ref()
                        .is_some_and(|result| result.invocation_id == call.dispatch.invocation_id))
        })
    {
        let known = result
            .as_ref()
            .filter(|result| result.invocation_id == call.dispatch.invocation_id);
        tools.push(ToolObservation {
            invocation_id: call.dispatch.invocation_id.clone(),
            attempt_id: call.dispatch.attempt_id.clone(),
            status: if known.is_some() {
                ToolStatus::Stopped
            } else if matches!(recovery, Recovery::EffectUnknown) {
                ToolStatus::EffectUnknown
            } else {
                ToolStatus::NotStarted
            },
            evidence: Vec::new(),
        });
        if let Some(result) = known {
            results.push(result.clone());
        }
    }
    let mut peer = env.peer(&replacement, epoch, store).await?;
    let restore = Restore {
        binding: Bind {
            grant: peer.grant.clone(),
            durable_head: peer.store.head.clone(),
            checkpoint: peer.store.batches.last().cloned(),
            manifest: state.manifest,
            limits: Limits::default(),
        },
        journal_tail: Vec::new(),
        tools,
        results,
        available_artifacts: Vec::new(),
        previous_owner_stopped: true,
        active_time: Some(RunActivityReconciliation {
            run_id,
            durable_head: peer.store.head.clone(),
            active_ms,
        }),
    };
    peer.send(peer.command("restore", "session.restore", serde_json::to_value(restore)?))
        .await?;
    peer.ready().await?;
    Ok((replacement, peer))
}

async fn read_journal(env: &Environment, retained: &[u8]) -> Result<Store> {
    let restored_bytes = tokio::fs::read(env.home.path().join("harness-journal.json")).await?;
    assert_eq!(restored_bytes, retained);
    let mut store = Store::default();
    for batch in serde_json::from_slice::<Vec<CheckpointBatch>>(&restored_bytes)? {
        let grant: OwnershipGrant = serde_json::from_slice(
            &tokio::fs::read(
                env.home
                    .path()
                    .join(format!("grant-{}.json", batch.identity.execution_epoch)),
            )
            .await?,
        )?;
        let ack = batch.validate_append(
            &grant,
            &store.head,
            &Limits::default(),
            &BTreeMap::new(),
            None,
        )?;
        store.head = ack.head();
        store
            .acknowledgements
            .insert(batch.identity.batch_id.clone(), ack);
        store.batches.push(batch);
    }
    Ok(store)
}

async fn crash_case(kind: &str, persisted: bool) -> Result<()> {
    let env = Environment::new().await?;
    let mut original = env.start().await?;
    let mut peer = env.peer(&original, 1, Store::default()).await?;
    let bind = process_binding(peer.grant.clone())?;
    peer.send(peer.command("bind", "session.bind", serde_json::to_value(bind)?))
        .await?;
    peer.ready().await?;
    let after_effect = matches!(kind, "tool.result" | "run.completed");
    let (first, result, proposal) = if after_effect {
        let (first, commands) = exchange(&mut peer, &original, &env.key, &create("task")).await?;
        let command = command(&mut peer, commands).await?;
        let result = execute(&env, &mut peer, &command).await?;
        let request = continuation(&first, &result, 1)?;
        let proposal = until_checkpoint(&mut peer, &original, &env.key, &request, kind).await?;
        (Some(first), Some(result), proposal)
    } else {
        let proposal =
            until_checkpoint(&mut peer, &original, &env.key, &create("task"), kind).await?;
        (None, None, proposal)
    };
    let initial_run = snapshot(&proposal)?.run.context("run")?.run_id;
    let (mut host, mut peer) = replace(
        &env,
        &mut original,
        peer,
        &proposal,
        persisted,
        if result.is_some() {
            Recovery::RetainedResult
        } else {
            Recovery::NotStarted
        },
    )
    .await?;
    let (first, result) = match (first, result) {
        (Some(first), Some(result)) => {
            let stored: ToolResult = serde_json::from_slice(
                &tokio::fs::read(env.home.path().join("tool-result.json")).await?,
            )?;
            assert_eq!(stored, result);
            (first, stored)
        }
        _ => {
            let mut replay = create("task");
            replay["bitrouter"]["execution_epoch"] = json!(2);
            let (first, commands) = exchange(&mut peer, &host, &env.key, &replay).await?;
            let command = command(&mut peer, commands).await?;
            assert_eq!(command.execution_epoch, 2);
            let result = execute(&env, &mut peer, &command).await?;
            (first, result)
        }
    };
    let request = continuation(&first, &result, 2)?;
    let (done, commands) = exchange(&mut peer, &host, &env.key, &request).await?;
    assert!(
        commands.is_empty(),
        "confirmed effect was re-dispatched: {kind}/{persisted}"
    );
    assert_eq!(
        done["bitrouter"]["run_status"], "completed",
        "{kind}/{persisted}: {done}"
    );
    assert_eq!(done["bitrouter"]["run_id"], initial_run);
    assert_eq!(
        tokio::fs::read(env.home.path().join("file.txt")).await?,
        b"effect\n"
    );
    let state = snapshot(peer.store.batches.last().context("checkpoint")?)?;
    let run = state.run.as_ref().context("run")?;
    let uncertain = (kind == "model.attempt.intent" && persisted)
        || (kind == "model.attempt.outcome" && !persisted);
    assert_eq!(
        run.model_attempts,
        if uncertain { 3 } else { 2 },
        "{kind}/{persisted}"
    );
    let unknown = state.cost_work[&run.run_id]
        .work
        .values()
        .filter(|work| {
            work.kind == CostWorkKind::ProviderAttempt
                && work.state == CostWorkState::IntentRecorded
        })
        .collect::<Vec<_>>();
    assert_eq!(unknown.len(), usize::from(uncertain));
    assert!(
        unknown
            .iter()
            .all(|work| !work.unknown_cost_reason.is_empty() && work.token_estimate.is_none())
    );
    assert_eq!(
        env.upstream
            .received_requests()
            .await
            .context("upstream")?
            .iter()
            .filter(|request| request.url.path() == "/v1/responses")
            .count(),
        if kind == "model.attempt.outcome" && !persisted {
            3
        } else {
            2
        },
        "{kind}/{persisted}"
    );
    let old = raw_post(&host, &env.key, &continuation(&first, &result, 1)?).await?;
    assert_eq!(old.status(), 409);
    assert_eq!(old.json::<Value>().await?["error"]["code"], "stale_epoch");
    let prior = peer.store.head.clone();
    let (replay, commands) = exchange(&mut peer, &host, &env.key, &request).await?;
    assert_eq!(replay, done);
    assert!(commands.is_empty());
    assert_eq!(peer.store.head, prior);
    peer.send(peer.command("release", "session.release", Value::Null))
        .await?;
    peer.receipt("release").await?;
    host.crash().await?;
    Ok(())
}

#[tokio::test]
async fn process_crash_reconciles_model_tool_and_terminal_boundaries() -> Result<()> {
    for kind in [
        "model.attempt.intent",
        "model.attempt.outcome",
        "model.output.applied",
        "response.completed",
        "tool.result",
        "run.completed",
    ] {
        for persisted in [false, true] {
            crash_case(kind, persisted)
                .await
                .with_context(|| format!("{kind}/{persisted}"))?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn process_crash_redelivers_an_unstarted_command_with_the_same_identity() -> Result<()> {
    let env = Environment::new().await?;
    let mut original = env.start().await?;
    let mut peer = env.peer(&original, 1, Store::default()).await?;
    let bind = process_binding(peer.grant.clone())?;
    peer.send(peer.command("bind", "session.bind", serde_json::to_value(bind)?))
        .await?;
    peer.ready().await?;
    let (first, commands) = exchange(&mut peer, &original, &env.key, &create("task")).await?;
    let delivered = command(&mut peer, commands).await?;
    assert!(!env.home.path().join("file.txt").exists());
    let head = peer.store.batches.last().context("checkpoint")?.clone();
    let (mut host, mut peer) = replace(
        &env,
        &mut original,
        peer,
        &head,
        false,
        Recovery::NotStarted,
    )
    .await?;
    let redelivered = command(&mut peer, Vec::new()).await?;
    assert_eq!(redelivered.invocation_id, delivered.invocation_id);
    assert_eq!(redelivered.attempt_id, delivered.attempt_id);
    assert_eq!(redelivered.arguments, delivered.arguments);
    assert_eq!(redelivered.execution_epoch, 2);
    assert_eq!(delivered.execution_epoch, 1);
    assert!(redelivered.authorizing_event_seq > delivered.authorizing_event_seq);
    let result = execute(&env, &mut peer, &redelivered).await?;
    let request = continuation(&first, &result, 2)?;
    let (done, commands) = exchange(&mut peer, &host, &env.key, &request).await?;
    assert!(commands.is_empty());
    assert_eq!(done["bitrouter"]["run_status"], "completed");
    assert_eq!(
        tokio::fs::read(env.home.path().join("file.txt")).await?,
        b"effect\n"
    );
    assert_eq!(
        snapshot(peer.store.batches.last().context("checkpoint")?)?
            .run
            .context("run")?
            .model_attempts,
        2
    );
    peer.send(peer.command("release", "session.release", Value::Null))
        .await?;
    peer.receipt("release").await?;
    host.crash().await?;
    Ok(())
}

#[tokio::test]
async fn process_crash_keeps_unknown_write_effect_blocked_until_restoration() -> Result<()> {
    let env = Environment::new().await?;
    let mut original = env.start().await?;
    let mut peer = env.peer(&original, 1, Store::default()).await?;
    let bind = process_binding(peer.grant.clone())?;
    peer.send(peer.command("bind", "session.bind", serde_json::to_value(bind)?))
        .await?;
    peer.ready().await?;
    let (first, commands) = exchange(&mut peer, &original, &env.key, &create("task")).await?;
    let delivered = command(&mut peer, commands).await?;
    let result = execute(&env, &mut peer, &delivered).await?;
    let head = peer.store.batches.last().context("checkpoint")?.clone();
    // The file effect has stopped, but the harness withholds its retained
    // outcome until it can reconcile the workspace. Unknown is never unstarted.
    let (mut host, mut peer) = replace(
        &env,
        &mut original,
        peer,
        &head,
        false,
        Recovery::EffectUnknown,
    )
    .await?;
    let state = snapshot(peer.store.batches.last().context("checkpoint")?)?;
    assert_eq!(
        state.run.context("run")?.status,
        RunStatus::RecoveryRequired
    );
    // A live result can be retained, but cannot clear authenticated recovery.
    let (blocked, commands) = exchange(
        &mut peer,
        &host,
        &env.key,
        &continuation(&first, &result, 2)?,
    )
    .await?;
    assert!(commands.is_empty());
    assert_eq!(blocked["bitrouter"]["run_status"], "recovery_required");
    peer.send(peer.command(
        "head",
        "session.head",
        json!({"durable_head":peer.store.head}),
    ))
    .await?;
    peer.ready().await?;
    assert_eq!(
        env.upstream
            .received_requests()
            .await
            .context("upstream")?
            .iter()
            .filter(|request| request.url.path() == "/v1/responses")
            .count(),
        1
    );
    assert_eq!(
        tokio::fs::read(env.home.path().join("file.txt")).await?,
        b"effect\n"
    );
    let head = peer.store.batches.last().context("checkpoint")?.clone();
    let (mut host, mut peer) = replace(
        &env,
        &mut host,
        peer,
        &head,
        false,
        Recovery::RetainedResult,
    )
    .await?;
    let mut request = create("resume");
    request["bitrouter"]["execution_epoch"] = json!(3);
    request["previous_response_id"] = blocked["id"].clone();
    request["input"] = json!([]);
    let (done, commands) = exchange(&mut peer, &host, &env.key, &request).await?;
    assert!(commands.is_empty());
    assert_eq!(done["bitrouter"]["run_status"], "completed");
    assert_eq!(done["bitrouter"]["run_id"], first["bitrouter"]["run_id"]);
    assert_eq!(
        tokio::fs::read(env.home.path().join("file.txt")).await?,
        b"effect\n"
    );
    let state = snapshot(peer.store.batches.last().context("checkpoint")?)?;
    let call = &state.root_turn().context("root turn")?.invocations[0];
    assert_eq!(call.result.as_ref(), Some(&result));
    assert!(call.prior_recovery_observations.iter().any(|observation| {
        observation.status == ToolStatus::EffectUnknown
            && observation.invocation_id == delivered.invocation_id
    }));
    assert_eq!(state.run.context("run")?.model_attempts, 2);
    peer.send(peer.command("release", "session.release", Value::Null))
        .await?;
    peer.receipt("release").await?;
    host.crash().await?;
    Ok(())
}
