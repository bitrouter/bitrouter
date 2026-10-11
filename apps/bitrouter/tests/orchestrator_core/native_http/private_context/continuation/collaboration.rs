//! Joint native provider-state and core allocation conformance through real HTTP.

use super::*;
use bitrouter_orchestrator::core::allocation::ContextKind;
use bitrouter_orchestrator::core::collaboration::{Action, Work};
use bitrouter_orchestrator::core::session::SessionSnapshot;
use bitrouter_sdk::language_model::native_continuation::ContinuationFailure;

#[derive(Clone)]
struct Wire {
    body: Value,
    response_id: String,
}

type Wires = Arc<std::sync::Mutex<Vec<Wire>>>;

fn wires(observed: &Wires) -> Result<Vec<Wire>> {
    observed
        .lock()
        .map(|rows| rows.clone())
        .map_err(|_| anyhow::anyhow!("wire fixture lock poisoned"))
}

async fn fixture(replayable: bool, spawn: Option<Work>) -> Result<(PrivateFixture, Wires)> {
    let fixture = stored_fixture(false, replayable, false).await?;
    fixture.upstream.reset().await;
    let observed: Wires = Default::default();
    let capture = observed.clone();
    let spawn_arguments = spawn
        .map(|task| serde_json::to_string(&json!({"task":task})))
        .transpose()?;
    Mock::given(method("POST")).and(path("/responses")).respond_with(move |request: &wiremock::Request| {
        let body = match serde_json::from_slice::<Value>(&request.body) {
            Ok(body) => body,
            Err(_) => return ResponseTemplate::new(400),
        };
        let spawning = body["input"].as_array().is_some_and(|items| items.len() == 1)
            && body.pointer("/input/0/content/0/text").and_then(Value::as_str) == Some("root-two");
        let id = match capture.lock() {
            Ok(mut rows) => {
                let id = format!("resp_joint_{}", rows.len() + 1);
                rows.push(Wire { body, response_id:id.clone() });
                id
            },
            Err(_) => return ResponseTemplate::new(500),
        };
        let mut output = vec![json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":"joint answer"} ]})];
        if spawning && let Some(arguments) = spawn_arguments.as_ref() {
            output.push(json!({"type":"function_call","call_id":"call_joint_spawn","name":"spawn_agent","arguments":arguments}));
        }
        if !replayable { output.push(json!({"type":"unrepresented_state","payload":format!("private {id}")})); }
        ResponseTemplate::new(200).set_body_json(json!({"id":id,"status":"completed","store":true,"output":output,"usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}}))
    }).mount(&fixture.upstream).await;
    Ok((fixture, observed))
}

async fn bind(app: bitrouter_sdk::App) -> Result<(CoreSession, Arc<crate::Harness>)> {
    let grant = OwnershipGrant {
        session_id: "joint-session".into(),
        harness_id: "joint-harness".into(),
        core_instance_id: "joint-core".into(),
        execution_epoch: 1,
    };
    let harness = Arc::new(crate::Harness {
        grant: grant.clone(),
        store: Mutex::new(crate::Store::default()),
    });
    let manifest = HarnessManifest {
        tool_manifest_digest: HarnessManifest::digest(&[])?,
        tools: Vec::new(),
        workspace_id: "workspace".into(),
        workspace_revision: Some("workspace-v1".into()),
        permission_revision: 1,
        max_tool_output_bytes: 8192,
        artifact_quota_bytes: 1024 * 1024,
        max_artifact_chunk_bytes: 8192,
        required_features: Vec::new(),
    };
    let caps = Capabilities {
        version: 1,
        core_instance_id: "joint-core".into(),
        operations: Vec::new(),
        transports: vec!["in_process".into()],
        unsupported_features: Vec::new(),
        limits: Limits::default(),
        max_sessions: 16,
        max_host_model_attempts: 16,
    };
    let session = CoreSession::bind(
        Bind {
            grant,
            durable_head: DurableHead::default(),
            checkpoint: None,
            manifest: manifest.clone(),
            limits: Limits::default(),
        },
        &caps,
        Arc::new(app),
        owner(),
        harness.clone(),
    )
    .await?;
    session
        .signals(
            "workspace",
            crate::SignalUpdate {
                signal_revision: 1,
                observed_at: "2026-10-02T12:00:00Z".into(),
                scope: "joint-session".into(),
                source: "joint-harness".into(),
                workspace_revision: manifest.workspace_revision.clone(),
                manifest,
                facts: Default::default(),
                materials: Vec::new(),
            },
        )
        .await?;
    Ok((session, harness))
}

async fn start(session: &CoreSession, text: &str) -> Result<String> {
    let mut input = crate::input(text);
    input.model = "bitrouter/private".into();
    let receipt = session
        .start(text, session.head().await.state_revision, input)
        .await?;
    Ok(receipt.assigned_ids["agent_id"].clone())
}

fn work(text: &str, fresh: bool) -> Work {
    Work {
        text: text.into(),
        model: None,
        effort: None,
        acceptance_criteria: Vec::new(),
        required_materials: Vec::new(),
        task_scope: Some("joint-scope".into()),
        fresh_context: fresh,
        independent_review: false,
    }
}

fn receipt<'a>(
    snapshot: &'a SessionSnapshot,
    agent: &str,
) -> Result<&'a bitrouter_orchestrator::core::routing::ExecutionReceipt> {
    snapshot
        .agents
        .get(agent)
        .and_then(|agent| agent.turn.as_ref())
        .and_then(|turn| turn.steps.first())
        .and_then(|step| step.attempts.first())
        .and_then(|attempt| attempt.receipt.as_ref())
        .context("missing execution receipt")
}

fn agent_wire<'a>(rows: &'a [Wire], agent: &str, task: &str) -> Result<&'a Wire> {
    rows.iter()
        .find(|row| {
            row.body["instructions"]
                .as_str()
                .is_some_and(|text| text.starts_with(&format!("You are agent {agent} ")))
                && row.body["input"].to_string().contains(task)
        })
        .context("missing agent wire")
}

#[tokio::test]
async fn native_continuation_inherited_and_fresh_children_keep_their_selected_context() -> Result<()>
{
    for replayable in [false, true] {
        for fresh in [false, true] {
            for switch_model in [false, true] {
                let mut task = work("child-task", fresh);
                if switch_model {
                    task.model = Some("fixture:changed".into());
                }
                let (fixture, observed) = fixture(replayable, Some(task)).await?;
                let (session, _) = bind(fixture.app.app).await?;
                let root = start(&session, "root-one").await?;
                session.drive().await?;
                start(&session, "root-two").await?;
                let state = session.drive().await?;
                let inherited = &state.agents[&root]
                    .turn
                    .as_ref()
                    .context("root turn")?
                    .steps[0]
                    .input_history;
                let child = &state
                    .agents
                    .values()
                    .find(|agent| agent.parent_id.as_ref() == Some(&root))
                    .context("spawned child")?
                    .agent_id;
                let allocation_id = state.agents[child]
                    .turn
                    .as_ref()
                    .context("child turn")?
                    .allocation_id
                    .as_ref()
                    .context("allocation")?;
                let allocation = &state.allocations[allocation_id];
                assert_eq!(
                    allocation.candidates[0].kind,
                    if fresh {
                        ContextKind::Fresh
                    } else {
                        ContextKind::Inherited
                    }
                );
                let turn = state.agents[child].turn.as_ref().context("child turn")?;
                let mut expected_history = if fresh { Vec::new() } else { inherited.clone() };
                expected_history.push(Message::text(Role::User, "child-task"));
                assert_eq!(turn.steps[0].input_history, expected_history);
                let rows = wires(&observed)?;
                if switch_model && !fresh && !replayable {
                    assert!(!rows.iter().any(|row| {
                        row.body["instructions"].as_str().is_some_and(|text| {
                            text.starts_with(&format!("You are agent {child} "))
                        })
                    }));
                    assert!(turn.steps[0].attempts.is_empty());
                    assert_eq!(
                        turn.steps[0].plan.as_ref().context("plan")?.routes[0].continuation,
                        NativeContinuationInput::Rejected {
                            reason: ContinuationFailure::TargetMismatch
                        }
                    );
                    continue;
                }
                assert_eq!(
                    rows.iter()
                        .filter(|row| row.body["instructions"].as_str().is_some_and(
                            |text| text.starts_with(&format!("You are agent {child} "))
                        ))
                        .count(),
                    1
                );
                let sent = agent_wire(&rows, child, "child-task")?;
                assert_eq!(
                    sent.body["model"],
                    if switch_model { "changed" } else { "served" }
                );
                let report = &receipt(&state, child)?.report;
                assert_eq!(report.continuation.output, NativeContinuationOutput::Issued);
                if fresh {
                    assert!(sent.body.get("previous_response_id").is_none());
                    assert_eq!(
                        report.continuation.input,
                        NativeContinuationInput::FullHistory {
                            reason: FullHistoryReason::NoHandle
                        }
                    );
                    assert_eq!(
                        sent.body["input"],
                        json!([
                            {"type":"message", "role":"user", "content":[{"type":"input_text", "text":"child-task"}]}
                        ])
                    );
                } else if switch_model {
                    assert!(sent.body.get("previous_response_id").is_none());
                    assert_eq!(
                        report.continuation.input,
                        NativeContinuationInput::FullHistory {
                            reason: FullHistoryReason::TargetChanged
                        }
                    );
                    assert_eq!(
                        sent.body["input"],
                        json!([
                            {"type":"message", "role":"user", "content":[{"type":"input_text", "text":"root-one"}]},
                            {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"joint answer"}]},
                            {"type":"message", "role":"user", "content":[{"type":"input_text", "text":"root-two"}]},
                            {"type":"message", "role":"user", "content":[{"type":"input_text", "text":"child-task"}]}
                        ])
                    );
                } else {
                    // The inherited input predates root-two's complete output. It
                    // retains root-one's anchor, not the newer excluded response.
                    assert_eq!(sent.body["previous_response_id"], rows[0].response_id);
                    assert_ne!(sent.body["previous_response_id"], rows[1].response_id);
                    assert_eq!(
                        report.continuation.input,
                        NativeContinuationInput::Resumed { prefix_messages: 2 }
                    );
                    assert_eq!(
                        sent.body["input"],
                        json!([
                            {"type":"message", "role":"user", "content":[{"type":"input_text", "text":"root-two"}]},
                            {"type":"message", "role":"user", "content":[{"type":"input_text", "text":"child-task"}]}
                        ])
                    );
                }
                assert!(
                    sent.body["instructions"]
                        .as_str()
                        .is_some_and(|text| text.contains("root-one") && text.contains("root-two"))
                );
                assert_eq!(
                    turn.steps[0]
                        .decision
                        .as_ref()
                        .context("decision")?
                        .allocation_id
                        .as_ref(),
                    Some(allocation_id)
                );
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn native_continuation_worker_reuse_resumes_worker_history_with_current_instructions()
-> Result<()> {
    let (fixture, observed) = fixture(false, None).await?;
    let (session, harness) = bind(fixture.app.app).await?;
    let root = start(&session, "root-one").await?;
    let spawned = session
        .collaborate(
            "spawn",
            session.head().await.state_revision,
            &root,
            Action::Spawn {
                task: work("worker-first", true),
            },
        )
        .await?;
    let child = &spawned.assigned_ids["agent_id"];
    let first = session.drive().await?;
    let history = first.agents[child].history.clone();
    let first_wires = wires(&observed)?;
    let worker_id = agent_wire(&first_wires, child, "worker-first")?
        .response_id
        .clone();
    start(&session, "root-current-read-only").await?;
    let delegated = session
        .collaborate(
            "delegate",
            session.head().await.state_revision,
            &root,
            Action::Delegate {
                task: work("worker-followup", false),
                agent_id: None,
            },
        )
        .await?;
    assert_eq!(&delegated.assigned_ids["agent_id"], child);
    let done = session.drive().await?;
    assert_eq!(
        done.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(done.agents.len(), 2);
    let allocation = &done.allocations[&delegated.assigned_ids["allocation_id"]];
    assert_eq!(allocation.candidates[0].kind, ContextKind::Reuse);
    assert!(allocation.applied_state_revision.is_some());
    assert!(done.agents[child].history.starts_with(&history));
    let rows = wires(&observed)?;
    let sent = agent_wire(&rows, child, "worker-followup")?;
    assert_eq!(sent.body["previous_response_id"], worker_id);
    assert!(!sent.body["input"].to_string().contains("worker-first"));
    assert!(
        sent.body["instructions"]
            .as_str()
            .is_some_and(|text| text.contains("root-current-read-only"))
    );
    let report = &receipt(&done, child)?.report;
    assert_eq!(
        report.continuation.input,
        NativeContinuationInput::Resumed {
            prefix_messages: history.len() as u64
        }
    );
    assert_eq!(report.continuation.output, NativeContinuationOutput::Issued);
    let store = harness.store.lock().await;
    let durable: SessionSnapshot = serde_json::from_value(
        store
            .batches
            .last()
            .context("checkpoint")?
            .decode(&Limits::default())?
            .checkpoint
            .state,
    )?;
    assert_eq!(durable.agents[child].history, done.agents[child].history);
    assert_eq!(receipt(&durable, child)?.report, report.clone());
    Ok(())
}

#[tokio::test]
async fn native_continuation_fallback_detaches_only_complete_history_and_seals_actual_source()
-> Result<()> {
    use wiremock::matchers::body_partial_json;
    for replayable in [true, false] {
        let mut fixture = stored_fixture(false, replayable, false).await?;
        let backup = MockServer::start().await;
        Mock::given(method("POST")).and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"resp_backup", "status":"completed", "store":true,
                "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"backup answer"}]}],
                "usage":{"input_tokens":20,"output_tokens":3,"total_tokens":23}
            }))).mount(&backup).await;
        fixture.source = fixture.source.replace("\nmodels:\n  private:",&format!("\n  backup:\n    api_base: {}\n    api_key: backup-key\n    models:\n      - id: served\n        api_protocol: responses\n        capabilities: [tools]\nmodels:\n  private:",backup.uri()))
            .replace("      - {provider: fixture, service_id: served}","      - {provider: fixture, service_id: served}\n      - {provider: backup, service_id: served}");
        fixture.app = assemble(&fixture.source, &fixture.home, fixture.checked.clone()).await?;
        let (session, _) = bind(fixture.app.app).await?;
        let root = start(&session, "initial").await?;
        session.drive().await?;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .and(body_partial_json(
                json!({"previous_response_id":"resp_native_first"}),
            ))
            .respond_with(ResponseTemplate::new(503).set_body_json(
                json!({"error":{"type":"server_error","message":"fixture resume unavailable"}}),
            ))
            .with_priority(1)
            .mount(&fixture.upstream)
            .await;
        start(&session, "next").await?;
        let second = session.drive().await?;
        let step = &second.agents[&root].turn.as_ref().context("turn")?.steps[0];
        let first_report = &step.attempts[0]
            .receipt
            .as_ref()
            .context("failed attempt")?
            .report;
        assert!(first_report.result.is_none());
        assert!(first_report.error.is_some());
        assert_eq!(
            first_report.continuation.input,
            NativeContinuationInput::Resumed { prefix_messages: 2 }
        );
        assert_eq!(
            first_report.continuation.output,
            NativeContinuationOutput::Unknown
        );
        assert_eq!(
            fixture
                .upstream
                .received_requests()
                .await
                .context("primary requests")?
                .len(),
            2
        );
        let backup_requests = backup
            .received_requests()
            .await
            .context("backup requests")?;
        if !replayable {
            assert_eq!(step.attempts.len(), 1);
            assert!(backup_requests.is_empty());
            assert_ne!(
                second.run.as_ref().map(|run| run.status),
                Some(RunStatus::Completed)
            );
            assert_eq!(
                step.plan.as_ref().context("plan")?.routes[1].continuation,
                NativeContinuationInput::Rejected {
                    reason: ContinuationFailure::TargetMismatch
                }
            );
            continue;
        }
        assert_eq!(step.attempts.len(), 2);
        assert_eq!(backup_requests.len(), 1);
        let wire: Value = serde_json::from_slice(&backup_requests[0].body)?;
        assert!(wire.get("previous_response_id").is_none());
        assert!(wire["input"].to_string().contains("initial"));
        assert!(wire["input"].to_string().contains("first answer"));
        assert!(wire["input"].to_string().contains("next"));
        let served = &step.attempts[1]
            .receipt
            .as_ref()
            .context("backup receipt")?
            .report;
        assert_eq!(served.actual_provider.as_deref(), Some("backup"));
        assert_eq!(
            served.continuation.input,
            NativeContinuationInput::FullHistory {
                reason: FullHistoryReason::TargetChanged
            }
        );
        assert_eq!(served.continuation.output, NativeContinuationOutput::Issued);
        assert_ne!(step.attempts[0].attempt_id, step.attempts[1].attempt_id);
        let mut next = crate::input("confirm-backup-source");
        next.model = "backup:served".into();
        session
            .start("confirm", session.head().await.state_revision, next)
            .await?;
        let third = session.drive().await?;
        assert_eq!(
            third.run.as_ref().map(|run| run.status),
            Some(RunStatus::Completed)
        );
        let requests = backup
            .received_requests()
            .await
            .context("backup requests")?;
        assert_eq!(requests.len(), 2);
        let wire: Value = serde_json::from_slice(&requests[1].body)?;
        assert_eq!(wire["previous_response_id"], "resp_backup");
        assert!(!wire["input"].to_string().contains("first answer"));
        assert!(!wire["input"].to_string().contains("backup answer"));
        assert!(wire["input"].to_string().contains("confirm-backup-source"));
        assert_eq!(
            receipt(&third, &root)?.report.continuation.input,
            NativeContinuationInput::Resumed { prefix_messages: 4 }
        );
    }
    Ok(())
}
