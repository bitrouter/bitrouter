//! Real pinned adapters, BitRouter binary, controller, native proxy and database.
//! Native transports are deterministic fixtures; no model API is called.
//! ACP stdio: https://agentclientprotocol.com/protocol/transports
#![cfg(unix)]

use anyhow::{Context, Result, ensure};
use bitrouter::session_evidence::adapter_bridge::{Event, PromptEvidence};
use bitrouter::session_evidence::execution::input_runs::InputOutcome;
use bitrouter::session_evidence::native_inputs::NativeInputEvidence;
use bitrouter::session_evidence::service::{EvidenceHandle, EvidenceLaunch};
use bitrouter::session_evidence::store::EvidenceStore;
use bitrouter::session_evidence::types::{RECORD_PAGE_SIZE, SourceFormat, SourceRange};
use bitrouter_sdk::acp::controller::ControllerIdentity;
use bitrouter_sdk::acp::transport::{AcpAgentConfig, AcpTransport};
use bitrouter_sdk::config::Config;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{ChildStdin, ChildStdout, Command};

#[tokio::test]
#[ignore = "requires npm access, Node >=22.15 and Python 3; runs both pinned adapters"]
async fn pinned_adapters_bind_actual_native_inputs_through_both_controllers_and_reopen()
-> Result<()> {
    for key in ["codex", "claude"] {
        for serve in [true, false] {
            tokio::time::timeout(Duration::from_secs(90), check_adapter(key, serve)).await??;
        }
    }
    Ok(())
}

struct Fixture {
    home: PathBuf,
    config: PathBuf,
    native_records: PathBuf,
    identity: ControllerIdentity,
    env: HashMap<String, String>,
}

async fn check_adapter(key: &str, serve: bool) -> Result<()> {
    let pins: Value = serde_json::from_str(include_str!(
        "../src/session_evidence/adapter_bridge/pins.json"
    ))?;
    let pin = &pins[key];
    let directory = tempfile::tempdir()?;
    let root = tokio::fs::canonicalize(directory.path()).await?;
    let home = root.join("router");
    let native = root.join("native");
    tokio::fs::create_dir_all(&native).await?;
    tokio::fs::create_dir_all(&home).await?;
    let executable = root.join(key);
    let native_fixture = if key == "codex" {
        include_str!("fixtures/native_bridge/codex.py")
    } else {
        include_str!("fixtures/native_bridge/claude.py")
    };
    tokio::fs::write(&executable, native_fixture).await?;
    tokio::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).await?;
    let native_records = root.join("native.jsonl");
    let env = HashMap::from([
        ("CODEX_HOME".into(), native.to_string_lossy().into_owned()),
        (
            "CLAUDE_CONFIG_DIR".into(),
            native.to_string_lossy().into_owned(),
        ),
        (
            "CODEX_PATH".into(),
            executable.to_string_lossy().into_owned(),
        ),
        (
            "CLAUDE_CODE_EXECUTABLE".into(),
            executable.to_string_lossy().into_owned(),
        ),
        (
            "BITROUTER_TEST_NATIVE_RECORDS".into(),
            native_records.to_string_lossy().into_owned(),
        ),
    ]);
    let package = pin["package"].as_str().context("package")?;
    let version = pin["version"].as_str().context("version")?;
    let mut config = Config::default();
    config.database.url = "sqlite:evidence.db?mode=rwc".into();
    config.agents.insert(
        "fixture".into(),
        AcpAgentConfig {
            name: "fixture".into(),
            transport: AcpTransport::Stdio {
                command: "npx".into(),
                args: vec!["-y".into(), format!("{package}@{version}")],
                env: env.clone(),
            },
        },
    );
    let config_path = home.join("bitrouter.yaml");
    tokio::fs::write(
        &config_path,
        serde_json::to_vec(&json!({
            "database":{"url":config.database.url}, "agents":config.agents,
        }))?,
    )
    .await?;
    let fixture = Fixture {
        home,
        config: config_path,
        native_records,
        identity: ControllerIdentity::new(
            if key == "codex" {
                "codex-acp"
            } else {
                "claude-acp"
            },
            package,
            version,
        ),
        env,
    };
    let expected = if serve { 2 } else { 1 };
    if serve {
        drive_serve(&fixture, &root).await?;
    } else {
        drive_run(&fixture, &root).await?;
    }
    let (evidence, inputs) = read_bindings(&fixture, expected).await?;
    let bytes = tokio::fs::read_to_string(&fixture.native_records).await?;
    assert!(
        !bytes.contains("bitrouter/native-evidence"),
        "private prompt metadata reached native input"
    );
    let records = bytes
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()?;
    verify_native_inputs(key, &evidence, &records, expected)?;
    if key == "codex" {
        verify_codex_raw_inputs(&fixture, &records).await?;
    }
    let reopened = read_bindings(&fixture, expected).await?;
    assert_eq!(
        serde_json::to_value((evidence, inputs))?,
        serde_json::to_value(reopened)?
    );
    Ok(())
}

async fn read_bindings(
    fixture: &Fixture,
    expected: usize,
) -> Result<(PromptEvidence, NativeInputEvidence)> {
    let mut env = fixture.env.clone();
    let mut handle = EvidenceHandle::open(EvidenceLaunch {
        home: &fixture.home,
        database_url: "sqlite:evidence.db?mode=rwc",
        identity: &fixture.identity,
        env: &mut env,
        strip_inherited_env: &[],
    })
    .await?
    .context("evidence reader")?;
    // Use the completed public shutdown boundary before manually advancing
    // recovery. Otherwise the worker can finish a sweep between our calls and
    // the next explicit reconcile starts another epoch instead of reading it.
    handle.shutdown().await?;
    let mut passes = 0;
    let snapshot = loop {
        let snapshot = handle.service.reconcile().await?;
        if !snapshot.gaps.contains("native_recovery_backlog")
            && !snapshot.gaps.contains("native_spool_backlog")
        {
            break snapshot;
        }
        passes += 1;
        ensure!(
            passes < 256,
            "native fixture inventory did not finish: {:?}",
            snapshot.gaps
        );
    };
    assert_eq!(snapshot.attempts.len(), 1, "one application attempt");
    let attempt = snapshot.attempts.first().context("attempt")?;
    let evidence = snapshot
        .prompt_bindings
        .get(&attempt.id)
        .context("prompt bindings")?
        .clone();
    assert!(
        evidence.gaps.is_empty(),
        "{}: {evidence:?}",
        fixture.identity.harness_id
    );
    assert_eq!(evidence.observations.len(), 3 * expected, "{evidence:?}");
    let inputs = snapshot
        .native_inputs
        .get(&attempt.id)
        .context("corroborated native inputs")?
        .clone();
    assert!(
        inputs.gaps.is_empty(),
        "{}: {:?}",
        fixture.identity.harness_id,
        inputs.gaps
    );
    assert_eq!(inputs.bindings.len(), expected, "{:?}", inputs.gaps);
    let store = EvidenceStore::new(
        bitrouter::db::connect(&format!(
            "sqlite:{}",
            fixture.home.join("evidence.db").display()
        ))
        .await?,
        "local",
    )?;
    let execution_id = attempt
        .execution_snapshot
        .as_ref()
        .context("execution snapshot")?;
    let membership = store.attempt_executions(execution_id).await?;
    assert_eq!(
        serde_json::to_value(&membership.inputs)?,
        serde_json::to_value(&inputs)?
    );
    assert_eq!(
        membership.members(),
        inputs
            .bindings
            .iter()
            .map(|input| input.node.clone())
            .collect()
    );
    assert_eq!(membership.members(), attempt.members);
    assert!(membership.descendants.is_empty());
    assert!(
        membership
            .gaps
            .contains("native_attempt_execution_coverage_incomplete")
    );
    assert!(attempt.effective_manifest.is_none());
    assert_ne!(
        attempt.phase,
        bitrouter::session_evidence::types::AttemptPhase::Ready
    );
    let raw_attempt = store.attempt(&attempt.id).await?.context("raw attempt")?;
    assert!(raw_attempt.members.is_empty());
    assert!(raw_attempt.execution_snapshot.is_none());
    for binding in &inputs.bindings {
        assert!(
            binding.execution.gaps.is_empty(),
            "{:?}",
            binding.execution.gaps
        );
        assert_eq!(binding.execution.outcome, Some(InputOutcome::Completed));
        assert_eq!(binding.execution.starts.len(), 1);
        assert_eq!(binding.execution.terminations.len(), 1);
        for reference in &binding.execution.records {
            let stored = store
                .records(&reference.range)
                .await?
                .into_iter()
                .next()
                .context("original execution event")?;
            assert_eq!(stored.id, reference.record_id);
            assert_eq!(stored.digest, reference.record_digest);
            assert_eq!(reference.range.source_id, binding.input.range.source_id);
            let payload = &stored.input.raw["payload"];
            if fixture.identity.harness_id == "codex-acp" {
                assert_eq!(payload["threadId"], binding.node.native_id);
                let turn = payload
                    .get("turnId")
                    .or_else(|| payload.pointer("/turn/id"));
                assert_eq!(
                    turn.and_then(Value::as_str),
                    Some(binding.native_id.as_str())
                );
            } else {
                assert_eq!(stored.input.raw["process_id"], binding.process_id);
                assert_eq!(payload["session_id"], binding.node.native_id);
                let command = payload
                    .get("command_uuid")
                    .or_else(|| payload.get("user_message_uuid"));
                assert_eq!(
                    command.and_then(Value::as_str),
                    Some(binding.native_id.as_str())
                );
            }
        }
        let request = store
            .records(&binding.input.range)
            .await?
            .into_iter()
            .next()
            .context("original native input")?;
        assert_eq!(request.id, binding.input.record_id);
        assert_eq!(request.digest, binding.input.record_digest);
        let raw = &request.input.raw;
        if fixture.identity.harness_id == "codex-acp" {
            assert_eq!(raw["payload"]["threadId"], binding.node.native_id);
            assert_eq!(binding.acknowledgements.len(), 1);
            let acknowledgement = &binding.acknowledgements[0];
            let reply = store
                .records(&acknowledgement.record.range)
                .await?
                .into_iter()
                .next()
                .context("native acceptance")?;
            assert_eq!(raw["operation_id"], reply.input.raw["operation_id"]);
            assert_eq!(reply.input.raw["payload"]["turn"]["id"], binding.native_id);
        } else {
            assert_eq!(raw["payload"]["uuid"], binding.native_id);
            assert!(binding.configuration.is_some() && binding.session_response.is_some());
            assert_eq!(
                binding
                    .acknowledgements
                    .iter()
                    .map(|ack| ack.state.as_str())
                    .collect::<Vec<_>>(),
                ["started", "completed"]
            );
            for acknowledgement in &binding.acknowledgements {
                let reply = store
                    .records(&acknowledgement.record.range)
                    .await?
                    .into_iter()
                    .next()
                    .context("native command acknowledgement")?;
                assert_eq!(reply.input.raw["process_id"], binding.process_id);
                assert_eq!(
                    reply.input.raw["payload"]["command_uuid"],
                    binding.native_id
                );
                assert_eq!(
                    reply.input.raw["payload"]["session_id"],
                    binding.node.native_id
                );
            }
        }
    }
    Ok((evidence, inputs))
}

async fn drive_run(fixture: &Fixture, root: &Path) -> Result<()> {
    let output = Command::new(env!("CARGO_BIN_EXE_bitrouter"))
        .args(["run", "fixture", "Fixture input.", "--direct", "--config"])
        .arg(&fixture.config)
        .current_dir(root)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

async fn drive_serve(fixture: &Fixture, root: &Path) -> Result<()> {
    let error_path = root.join("serve.stderr");
    let mut child = Command::new(env!("CARGO_BIN_EXE_bitrouter"))
        .args(["acp", "serve", "fixture", "--direct", "--config"])
        .arg(&fixture.config)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(&error_path)?)
        .kill_on_drop(true)
        .spawn()?;
    let mut input = child.stdin.take().context("stdin")?;
    let mut output = BufReader::new(child.stdout.take().context("stdout")?).lines();
    let outcome = async {
        rpc(
            &mut input,
            &mut output,
            1,
            "initialize",
            json!({"protocolVersion":1,"clientCapabilities":{}}),
        )
        .await?;
        let opened = rpc(
            &mut input,
            &mut output,
            2,
            "session/new",
            json!({"cwd":root,"mcpServers":[]}),
        )
        .await?;
        let id = opened["sessionId"].as_str().context("session")?;
        for index in 3..5 {
            let result = rpc(
                &mut input,
                &mut output,
                index,
                "session/prompt",
                json!({"sessionId":id,"prompt":[{"type":"text","text":"Fixture input."}]}),
            )
            .await?;
            assert_eq!(result["stopReason"], "end_turn");
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    drop(input);
    let stopped = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    let errors = tokio::fs::read_to_string(&error_path).await?;
    outcome.with_context(|| format!("serve failed: {errors}"))?;
    ensure!(
        stopped.context("serve did not stop")??.success(),
        "serve failed on shutdown: {errors}"
    );
    Ok(())
}

async fn rpc(
    input: &mut ChildStdin,
    output: &mut Lines<BufReader<ChildStdout>>,
    id: u32,
    method: &str,
    params: Value,
) -> Result<Value> {
    let mut bytes =
        serde_json::to_vec(&json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}))?;
    bytes.push(b'\n');
    input.write_all(&bytes).await?;
    input.flush().await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(line) = output.next_line().await? {
            let value: Value = serde_json::from_str(&line)?;
            if value["id"] == id {
                ensure!(value.get("error").is_none(), "{method}: {value}");
                return value.get("result").cloned().context("missing RPC result");
            }
        }
        anyhow::bail!("{method}: controller output closed")
    })
    .await?
}

fn verify_native_inputs(
    key: &str,
    evidence: &PromptEvidence,
    records: &[Value],
    expected: usize,
) -> Result<()> {
    let mut operations = BTreeSet::new();
    let mut native_ids = BTreeSet::new();
    for proven in &evidence.observations {
        let observation = &proven.observation;
        match &observation.event {
            Event::CodexAccepted {
                thread_id,
                turn_id,
                role,
            } => {
                assert_eq!(role, "prompt");
                assert_eq!(thread_id, &observation.session_id);
                let response = records
                    .iter()
                    .find(|record| {
                        record["direction"] == "out"
                            && record["body"]["result"]["turn"]["id"] == *turn_id
                    })
                    .context("accepted native response")?;
                let request = records
                    .iter()
                    .find(|record| {
                        record["direction"] == "in"
                            && record["body"]["id"] == response["body"]["id"]
                    })
                    .context("original native request")?;
                assert_eq!(request["body"]["method"], "turn/start");
                assert_eq!(request["body"]["params"]["threadId"], *thread_id);
                assert_eq!(
                    request["body"]["params"]["input"][0]["text"],
                    "Fixture input."
                );
                native_ids.insert(turn_id.clone());
                operations.insert(observation.origin.operation_id.clone());
            }
            Event::ClaudeEnqueued { command_id } => {
                assert!(records.iter().any(|record| record["direction"] == "in"
                    && record["body"]["type"] == "user"
                    && record["body"]["uuid"] == *command_id));
                native_ids.insert(command_id.clone());
                operations.insert(observation.origin.operation_id.clone());
            }
            _ => {}
        }
    }
    assert_eq!(operations.len(), expected, "{key}: distinct ACP operations");
    assert_eq!(native_ids.len(), expected, "{key}: distinct native inputs");
    let inputs: Vec<_> = records
        .iter()
        .filter(|record| {
            record["direction"] == "in"
                && if key == "codex" {
                    record["body"]["method"] == "turn/start"
                        && evidence.observations.iter().any(|proven| {
                            match &proven.observation.event {
                                Event::CodexAccepted { thread_id, .. } => {
                                    record["body"]["params"]["threadId"] == *thread_id
                                }
                                _ => false,
                            }
                        })
                } else {
                    record["body"]["type"] == "user"
                }
        })
        .collect();
    assert_eq!(inputs.len(), expected, "{key}: ordinary native input count");
    for input in inputs {
        let content = if key == "codex" {
            &input["body"]["params"]["input"]
        } else {
            &input["body"]["message"]["content"]
        };
        assert_eq!(content.as_array().context("native content")?.len(), 1);
        assert_eq!(
            content[0]["text"], "Fixture input.",
            "{key}: native text changed"
        );
    }
    // The pinned Codex adapter also starts a fire-and-forget title turn in a
    // separate ephemeral thread. It must remain outside ordinary prompt proof.
    // https://github.com/agentclientprotocol/codex-acp/blob/061f9a4a2e463a220d7a3ab2ae5e9732837085ef/src/TitleGenerator.ts
    let mut titles = 0;
    for record in records.iter().filter(|record| {
        record["direction"] == "in"
            && record["body"]["method"] == "turn/start"
            && !record["body"]["params"]["outputSchema"]["properties"]["title"].is_null()
    }) {
        titles += 1;
        let accepted = records
            .iter()
            .find(|response| {
                response["direction"] == "out" && response["body"]["id"] == record["body"]["id"]
            })
            .context("title accepted")?;
        assert!(
            !native_ids.contains(
                accepted["body"]["result"]["turn"]["id"]
                    .as_str()
                    .context("title turn")?
            )
        );
    }
    if key == "codex" {
        if expected > 1 {
            assert_eq!(titles, 1, "fixture must exercise auxiliary title work");
        }
        assert_eq!(
            records
                .iter()
                .filter(|record| record["direction"] == "in"
                    && record["body"]["method"] == "turn/start")
                .count(),
            expected + titles
        );
    }
    Ok(())
}

async fn verify_codex_raw_inputs(fixture: &Fixture, native: &[Value]) -> Result<()> {
    let store = EvidenceStore::new(
        bitrouter::db::connect(&format!(
            "sqlite:{}",
            fixture.home.join("evidence.db").display()
        ))
        .await?,
        "local",
    )?;
    let mut raw_turns = BTreeSet::new();
    let mut after = None;
    loop {
        let sources = store.sources(after.as_deref(), 16).await?;
        if sources.is_empty() {
            break;
        }
        for source in &sources {
            if source.descriptor.format != SourceFormat::CodexAppServer {
                continue;
            }
            let mut start = 0;
            while start < source.cursor.next_sequence {
                let end = (start + RECORD_PAGE_SIZE).min(source.cursor.next_sequence);
                for record in store
                    .records(&SourceRange {
                        source_id: source.id.clone(),
                        generation: source.cursor.generation.clone(),
                        start,
                        end,
                    })
                    .await?
                {
                    if record.input.raw["method"] == "turn/start"
                        && record.input.raw["phase"] == "response"
                        && let Some(id) = record.input.raw["payload"]["turn"]["id"].as_str()
                    {
                        raw_turns.insert(id.to_owned());
                    }
                }
                start = end;
            }
        }
        after = sources.last().map(|source| source.id.clone());
    }
    for response in native.iter().filter(|record| record["direction"] == "out") {
        if let Some(turn) = response["body"]["result"]["turn"]["id"].as_str() {
            assert!(
                raw_turns.contains(turn),
                "native acceptance, including auxiliary title, was not retained"
            );
        }
    }
    Ok(())
}
