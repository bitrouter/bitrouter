//! The same scripted collaboration crosses local and authenticated remote ports.

use super::*;
use bitrouter_orchestrator::core::allocation::ContextKind;
use bitrouter_orchestrator::core::routing::ApplicationDisposition;
use bitrouter_orchestrator::core::session::{AgentStatus, SessionSnapshot};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Semaphore;

const GUARD: Duration = Duration::from_secs(60);
const ROOT: &str = "Compare two independent findings";
const CHILDREN: [&str; 2] = ["Inspect inherited evidence", "Inspect fresh evidence"];
const ANSWERS: [&str; 2] = ["inherited finding", "fresh finding"];

struct Script {
    seen: Semaphore,
    release: Semaphore,
    active: AtomicUsize,
    peak: AtomicUsize,
    root_calls: AtomicUsize,
    requests: Mutex<Vec<Value>>,
}

impl Script {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            seen: Semaphore::new(0),
            release: Semaphore::new(0),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            root_calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        })
    }

    async fn respond(&self, body: Value) -> Result<Value> {
        let items = body["input"].as_array().context("provider input")?;
        let child = CHILDREN.iter().position(|task| {
            items.iter().any(|item| {
                item["role"] == "user"
                    && item["content"]
                        .as_array()
                        .is_some_and(|content| content.iter().any(|part| part["text"] == *task))
            })
        });
        let inputs = items.clone();
        self.requests.lock().await.push(body);
        let output = if let Some(child) = child {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            self.seen.add_permits(1);
            let permit = tokio::time::timeout(GUARD, self.release.acquire()).await??;
            permit.forget();
            self.active.fetch_sub(1, Ordering::SeqCst);
            vec![message(ANSWERS[child])]
        } else {
            match self.root_calls.fetch_add(1, Ordering::SeqCst) {
                0 => vec![
                    call("spawn_agent", "spawn-a", json!({"task":work(0)})),
                    call("delegate_task", "delegate-b", json!({"task":work(1)})),
                ],
                1 => {
                    let mut agents = Vec::new();
                    for item in inputs {
                        if item["type"] == "function_call_output" {
                            let value: Value = serde_json::from_str(
                                item["output"].as_str().context("collaboration result")?,
                            )?;
                            if let Some(agent) = value["value"]["agent_id"].as_str() {
                                agents.push(agent.to_owned());
                            }
                        }
                    }
                    anyhow::ensure!(agents.len() == 2, "missing allocation results");
                    vec![call(
                        "wait_agent",
                        "wait-both",
                        json!({"agent_ids":agents,"timeout_ms":60000}),
                    )]
                }
                _ => vec![message("Combined both attributed findings")],
            }
        };
        // Fixture wire shapes follow the public Responses contract; core-owned
        // collaboration remains function calls, not hosted multi-agent items.
        // https://developers.openai.com/api/reference/resources/responses/methods/create
        Ok(
            json!({"id":"script-response","object":"response","status":"completed",
            "model":"served","output":output,
            "usage":{"input_tokens":20,"output_tokens":10,"total_tokens":30}}),
        )
    }

    async fn release_overlapping_children(&self) -> Result<()> {
        tokio::time::timeout(GUARD, self.seen.acquire_many(2))
            .await
            .context("two child provider requests must overlap")??
            .forget();
        assert_eq!(self.active.load(Ordering::SeqCst), 2);
        assert_eq!(self.peak.load(Ordering::SeqCst), 2);
        self.release.add_permits(2);
        Ok(())
    }
}

fn message(text: &str) -> Value {
    json!({"id":"message","type":"message","role":"assistant","status":"completed",
        "content":[{"type":"output_text","text":text,"annotations":[]}]})
}

fn call(name: &str, id: &str, arguments: Value) -> Value {
    json!({"id":id,"type":"function_call","call_id":id,"name":name,
        "arguments":arguments.to_string(),"status":"completed"})
}

fn work(index: usize) -> Value {
    json!({"text":CHILDREN[index],"model":null,"effort":null,
        "acceptance_criteria":[],"required_materials":[],"task_scope":CHILDREN[index],
        "fresh_context":index == 1,"independent_review":index == 1})
}

struct Provider {
    url: String,
    script: Arc<Script>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn provider() -> Result<Provider> {
    let script = Script::new();
    let router = axum::Router::new()
        .route(
            "/v1/responses/input_tokens",
            axum::routing::post(|| async {
                axum::Json(json!({"object":"response.input_tokens","input_tokens":20}))
            }),
        )
        .route(
            "/v1/responses",
            axum::routing::post(
                |axum::extract::State(script): axum::extract::State<Arc<Script>>,
                 axum::Json(body): axum::Json<Value>| async move {
                    script.respond(body).await.map(axum::Json).map_err(|error| {
                        (http::StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
                    })
                },
            ),
        )
        .with_state(script.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let task = tokio::spawn(async move { axum::serve(listener, router).await });
    Ok(Provider { url, script, task })
}

fn evidence(state: &SessionSnapshot, requests: &[Value]) -> Result<Value> {
    assert_eq!(
        state.run.as_ref().context("run")?.status,
        RunStatus::Completed
    );
    assert_eq!(state.agents.len(), 3);
    let root = &state.agents[&state.agent_id];
    let root_turn = root.turn.as_ref().context("root turn")?;
    assert_eq!(
        root_turn.final_answer.as_deref(),
        Some("Combined both attributed findings")
    );
    let mut proof = Vec::new();
    for (index, task) in CHILDREN.iter().enumerate() {
        let child = state
            .agents
            .values()
            .find(|agent| {
                agent
                    .turn
                    .as_ref()
                    .is_some_and(|turn| turn.input.text == *task)
            })
            .context("child task")?;
        let turn = child.turn.as_ref().context("child turn")?;
        assert_eq!(child.parent_id.as_deref(), Some(state.agent_id.as_str()));
        assert_eq!(turn.assigned_by, state.agent_id);
        assert_eq!(turn.status, AgentStatus::Completed);
        assert_eq!(turn.final_answer.as_deref(), Some(ANSWERS[index]));
        assert!(turn.notified);
        assert_eq!(turn.steps.len(), 1);
        let notices: Vec<_> = root
            .mailbox
            .iter()
            .filter(|mail| mail.sender_id == child.agent_id)
            .collect();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].consumed);
        assert_eq!(notices[0].sender_turn_id, turn.agent_turn_id);
        assert_eq!(notices[0].content["answer"], ANSWERS[index]);
        assert_eq!(notices[0].context_sources, child.context_sources);
        let allocation = &state.allocations[turn.allocation_id.as_ref().context("allocation")?];
        assert!(allocation.error.is_none() && allocation.application_error.is_none());
        assert_eq!(allocation.actor_id, state.agent_id);
        assert_eq!(
            allocation.selected_agent_id.as_deref(),
            Some(child.agent_id.as_str())
        );
        let selected = allocation
            .candidates
            .iter()
            .find(|candidate| {
                Some(&candidate.candidate_id) == allocation.selected_candidate_id.as_ref()
            })
            .context("chosen context")?;
        let kind = if index == 0 {
            ContextKind::Inherited
        } else {
            ContextKind::Fresh
        };
        assert_eq!(selected.kind, kind);
        assert!(selected.rejection_reasons.is_empty());
        let inherited_root = turn.steps[0].input_history.iter().any(|message| {
            serde_json::to_string(message).is_ok_and(|message| message.contains(ROOT))
        });
        assert_eq!(inherited_root, index == 0);
        let wires: Vec<_> = requests
            .iter()
            .filter(|request| {
                request["instructions"]
                    .as_str()
                    .is_some_and(|instructions| {
                        instructions.starts_with(&format!("You are agent {} ", child.agent_id))
                    })
            })
            .collect();
        assert_eq!(wires.len(), 1);
        let wire = wires[0]["input"]
            .as_array()
            .context("child provider input")?;
        assert!(wire.iter().any(|item| {
            item["content"]
                .as_array()
                .is_some_and(|parts| parts.iter().any(|part| part["text"] == *task))
        }));
        assert_eq!(
            wire.iter().any(|item| item["content"]
                .as_array()
                .is_some_and(|parts| { parts.iter().any(|part| part["text"] == ROOT) })),
            index == 0
        );
        proof.push(json!({"task":task,"kind":kind,"source":allocation.source,
            "answer":turn.final_answer,"routing":turn.input.routing}));
    }
    let mut executed = 0;
    for agent in state.agents.values() {
        let turn = agent.turn.as_ref().context("turn")?;
        assert!(
            turn.invocations.is_empty(),
            "collaboration cannot reach the harness"
        );
        for step in &turn.steps {
            assert!(step.settled);
            let decision = step.decision.as_ref().context("decision")?;
            let applied = step.application.as_ref().context("applied")?;
            assert_eq!(decision.decision_id, step.decision_id);
            assert_eq!(applied.decision_id, step.decision_id);
            assert_eq!(applied.step_id, step.step_id);
            assert_eq!(applied.agent_turn_id, turn.agent_turn_id);
            assert!(matches!(
                applied.disposition,
                ApplicationDisposition::Applied
            ));
            assert!(applied.reason.is_none());
            assert_eq!(decision.context.agent_id, agent.agent_id);
            assert_eq!(decision.context.agent_turn_id, turn.agent_turn_id);
            assert_eq!(decision.context.revision, step.context_revision);
            assert_eq!(decision.allocation_id, turn.allocation_id);
            assert!(
                decision
                    .candidate_ids
                    .contains(&decision.selected_candidate_id)
            );
            assert_eq!(decision.selected_model, "fixture-model");
            assert_eq!(decision.modes.context, ContextMode::Auto);
            assert_eq!(step.attempts.len(), 1);
            for attempt in &step.attempts {
                let receipt = attempt.receipt.as_ref().context("execution receipt")?;
                assert_eq!(receipt.decision_id, step.decision_id);
                assert_eq!(receipt.attempt_id, attempt.attempt_id);
                assert_eq!(receipt.report.actual_provider.as_deref(), Some("fixture"));
                assert_eq!(receipt.report.actual_model.as_deref(), Some("served"));
                assert!(receipt.report.error.is_none());
                assert!(receipt.report.result.is_some());
                executed += 1;
            }
        }
    }
    assert_eq!(executed, requests.len());
    let last_root = requests
        .iter()
        .rev()
        .find(|request| {
            request["instructions"]
                .as_str()
                .is_some_and(|instructions| {
                    instructions.starts_with(&format!("You are agent {} ", root.agent_id))
                })
        })
        .context("root joined provider input")?;
    for answer in ANSWERS {
        assert!(last_root["input"].to_string().contains(answer));
    }
    assert_eq!(root_turn.core_calls.len(), 3);
    assert_eq!(
        root_turn
            .core_calls
            .iter()
            .map(|call| call.action.name())
            .collect::<Vec<_>>(),
        ["spawn_agent", "delegate_task", "wait_agent"]
    );
    for call in &root_turn.core_calls {
        assert!(call.consumed);
        assert_eq!(
            call.result.as_ref().context("collaboration result")?["ok"],
            true
        );
    }
    let waited = root_turn.core_calls[2]
        .result
        .as_ref()
        .context("wait result")?;
    assert_eq!(waited["value"]["timed_out"], false);
    let targets = waited["value"]["agents"]
        .as_array()
        .context("wait targets")?;
    assert_eq!(targets.len(), 2);
    let expected: std::collections::BTreeSet<_> = state
        .agents
        .values()
        .filter(|agent| agent.parent_id.is_some())
        .map(|agent| agent.agent_id.as_str())
        .collect();
    let actual: std::collections::BTreeSet<_> = targets
        .iter()
        .filter_map(|target| target["agent_id"].as_str())
        .collect();
    assert_eq!(actual, expected);
    for target in targets {
        let child = state
            .agents
            .get(target["agent_id"].as_str().context("wait agent")?)
            .context("known wait agent")?;
        assert!(child.parent_id.is_some());
        assert_eq!(
            target["agent_turn_id"],
            child.turn.as_ref().context("wait turn")?.agent_turn_id
        );
    }
    Ok(json!(proof))
}

#[tokio::test]
async fn local_and_remote_collaboration_preserve_concurrency_context_and_attribution() -> Result<()>
{
    let mut proofs = Vec::new();
    for remote in [false, true] {
        let provider = provider().await?;
        let fixture = configured_fixture_with_provider(None, None, Some(&provider.url)).await?;
        let limits = Limits {
            active_models: 2,
            ..Limits::default()
        };
        let state = if remote {
            let (send, mut tools, store, task) = harness_with_limits(&fixture, limits).await?;
            let mut body = create("collaboration");
            body["input"] = json!(ROOT);
            body["bitrouter"]["routing"] = json!({"model":"fixed","context":"auto"});
            let exchange = async {
                let response = post(&fixture, &fixture.key, &body).await?;
                let status = response.status();
                let output: Value = response.json().await?;
                anyhow::ensure!(status == 200, "managed response {status}: {output}");
                Ok::<_, anyhow::Error>(output)
            };
            let (response, ()) = tokio::time::timeout(GUARD, async {
                tokio::try_join!(exchange, provider.script.release_overlapping_children())
            })
            .await??;
            assert_eq!(response["status"], "completed");
            assert_eq!(response["bitrouter"]["run_status"], "completed");
            assert_eq!(response["bitrouter"]["pending_invocations"], json!({}));
            let state: SessionSnapshot = serde_json::from_value(
                store
                    .lock()
                    .await
                    .batches
                    .last()
                    .context("checkpoint")?
                    .decode(&Limits::default())?
                    .checkpoint
                    .state,
            )?;
            for (index, child_task) in CHILDREN.iter().enumerate() {
                let child = state
                    .agents
                    .values()
                    .find(|agent| {
                        agent
                            .turn
                            .as_ref()
                            .is_some_and(|turn| turn.input.text == *child_task)
                    })
                    .context("projected child")?;
                let items: Vec<_> = response["output"]
                    .as_array()
                    .context("output")?
                    .iter()
                    .filter(|item| {
                        item["type"] == "message"
                            && item["agent"]["agent_name"] == child.display_path
                    })
                    .collect();
                assert_eq!(items.len(), 1, "child message attribution: {response}");
                assert_eq!(items[0]["phase"], "commentary");
                assert_eq!(items[0]["content"][0]["text"], ANSWERS[index]);
                let deliveries: Vec<_> = response["bitrouter"]["events"]
                    .as_array()
                    .context("events")?
                    .iter()
                    .filter(|item| {
                        item["event"]["type"] == "agent.result.delivered"
                            && item["event"]["agent_id"] == child.agent_id
                    })
                    .collect();
                assert_eq!(deliveries.len(), 1);
                assert_eq!(deliveries[0]["agent_name"], child.display_path);
                assert_eq!(
                    deliveries[0]["event"]["payload"]["parent_id"],
                    state.agent_id
                );
            }
            let finals: Vec<_> = response["output"]
                .as_array()
                .context("output")?
                .iter()
                .filter(|item| item["phase"] == "final_answer")
                .collect();
            assert_eq!(finals.len(), 1);
            assert_eq!(finals[0]["agent"]["agent_name"], "/root");
            assert_eq!(
                finals[0]["content"][0]["text"],
                "Combined both attributed findings"
            );
            let calls = provider.script.requests.lock().await.len();
            let replay: Value = post(&fixture, &fixture.key, &body).await?.json().await?;
            assert_eq!(replay, response);
            body["stream"] = json!(true);
            let stream = post(&fixture, &fixture.key, &body).await?.text().await?;
            let events = stream
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .map(serde_json::from_str::<Value>)
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for (sequence, event) in events.iter().enumerate() {
                assert_eq!(event["sequence_number"], sequence);
            }
            let terminal = events.last().context("SSE terminal")?;
            assert_eq!(terminal["type"], "response.completed");
            assert_eq!(terminal["response"], response);
            for child in state
                .agents
                .values()
                .filter(|agent| agent.parent_id.is_some())
            {
                let deltas: Vec<_> = events
                    .iter()
                    .filter(|event| {
                        event["type"] == "response.output_text.delta"
                            && event["agent"]["agent_name"] == child.display_path
                    })
                    .collect();
                assert_eq!(deltas.len(), 1);
                assert_eq!(
                    deltas[0]["delta"],
                    child
                        .turn
                        .as_ref()
                        .context("SSE child turn")?
                        .final_answer
                        .as_deref()
                        .context("SSE answer")?
                );
            }
            assert_eq!(provider.script.requests.lock().await.len(), calls);
            assert!(matches!(
                tools.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
            fixture.api.shutdown().await;
            drop(send);
            tokio::time::timeout(GUARD, task).await???;
            state
        } else {
            let binding = binding(limits)?;
            let port = Arc::new(crate::Harness {
                grant: binding.grant.clone(),
                store: Mutex::new(Store::default()),
            });
            let caps = Capabilities {
                version: 1,
                core_instance_id: binding.grant.core_instance_id.clone(),
                operations: Vec::new(),
                transports: vec!["in_process".into()],
                unsupported_features: Vec::new(),
                limits: Limits::default(),
                max_sessions: 16,
                max_host_model_attempts: 16,
            };
            let session = CoreSession::bind(
                binding,
                &caps,
                fixture.app.clone(),
                CallerContext::new("key_owner", "owner"),
                port,
            )
            .await?;
            let mut task = crate::input(ROOT);
            task.max_concurrent_subagents = Some(3);
            let receipt = session
                .start_response("collaboration", session.head().await.state_revision, task)
                .await?;
            let id = receipt
                .assigned_ids
                .get("response_id")
                .context("response id")?;
            tokio::time::timeout(GUARD, async {
                tokio::try_join!(
                    async { Ok::<_, anyhow::Error>(session.drive_response(id).await?) },
                    provider.script.release_overlapping_children()
                )
            })
            .await??;
            let state = session.snapshot().await;
            session
                .release("release", session.head().await.state_revision)
                .await?;
            fixture.api.shutdown().await;
            state
        };
        proofs.push(evidence(&state, &provider.script.requests.lock().await)?);
    }
    assert_eq!(proofs[0], proofs[1]);
    Ok(())
}
