use super::*;
use bitrouter_orchestrator::core::accounting::work::{CostWorkKind, CostWorkState};
use bitrouter_orchestrator::core::context_router::FEATURE;
use bitrouter_orchestrator::core::protocol::ContextMode;
use bitrouter_sdk::decision_model::DecisionRuntime;
use bitrouter_sdk::decision_model::policy::{DecisionPolicy, DecisionPricing};
use bitrouter_sdk::decision_model::typesafe::TypeSafeExecutor;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn decide(request: &Request) -> ResponseTemplate {
    let Ok(body) = serde_json::from_slice::<serde_json::Value>(&request.body) else {
        return ResponseTemplate::new(400);
    };
    let Some(questions) = body.get("questions").and_then(serde_json::Value::as_object) else {
        return ResponseTemplate::new(400);
    };
    let answers: serde_json::Map<String, serde_json::Value> = questions
        .iter()
        .map(|(id, question)| {
            let criteria = question.get("criteria").and_then(serde_json::Value::as_object);
            let selected = if body["state"]["task"].as_str() == Some("shared-context child") { "full" }
                else if criteria.is_some_and(|criteria| criteria.contains_key("summary")) { "summary" }
                else if criteria.is_some_and(|criteria| criteria.contains_key("extract")) { "extract" } else { "hide" };
            let probabilities: serde_json::Map<String, serde_json::Value> = criteria.into_iter()
                .flat_map(|criteria| criteria.keys()).map(|name| (name.clone(), json!(if name == selected { 1.0 } else { 0.0 }))).collect();
            (
                id.clone(),
                json!({"type":"choice","choice":selected,"confidence":0.99,"probabilities":probabilities}),
            )
        })
        .collect();
    ResponseTemplate::new(200).set_body_json(json!({
        "model":"fixture-decision-1","answers":answers,
        "usage":{"input_tokens":100,"output_tokens":4}
    }))
}

async fn setup_decision(
    server: &MockServer,
    harness: Arc<Harness>,
    responses: Vec<MockResponse>,
) -> Result<(CoreSession, Arc<RecordingExecutor>), Box<dyn std::error::Error>> {
    let (app, executor) = decision_app(server, responses)?;
    let session =
        bind_app_with_features(app, harness, Limits::default(), vec![FEATURE.into()]).await?;
    Ok((session, executor))
}

fn decision_app(
    server: &MockServer,
    responses: Vec<MockResponse>,
) -> Result<(Arc<App>, Arc<RecordingExecutor>), Box<dyn std::error::Error>> {
    let executor = Arc::new(RecordingExecutor {
        mock: MockExecutor::new(responses),
        agent_once: Mutex::new(Default::default()),
        prompts: Mutex::new(Vec::new()),
        calls: AtomicUsize::new(0),
    });
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target("first")]);
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(executor.clone());
        })
        .decision_model(DecisionRuntime {
            model: "fixture-decision".into(),
            executor: Arc::new(TypeSafeExecutor::new(
                &server.uri(),
                "fixture-key",
                Duration::from_secs(10),
                8192,
            )?),
            policy: DecisionPolicy {
                target_context_bytes: 1,
                retain_recent_groups: 0,
                ..Default::default()
            },
            pricing: Some(DecisionPricing {
                input_usd_per_million: 0.1,
                output_usd_per_million: 0.0,
            }),
        })
        .build()?;
    Ok((Arc::new(app), executor))
}

fn task() -> TaskInput {
    let mut task = input();
    task.routing.context = ContextMode::Auto;
    task.context_limit_bytes = Some(512 * 1024);
    task
}

async fn read_result(session: &CoreSession, harness: &Harness) -> TestResult {
    session
        .start("start", session.head().await.state_revision, task())
        .await?;
    session.drive().await?;
    let command = harness
        .sent
        .lock()
        .await
        .first()
        .ok_or("missing tool dispatch")?
        .clone();
    let mut observation = result(&command);
    if session
        .snapshot()
        .await
        .manifest
        .workspace_revision
        .is_none()
    {
        observation.workspace_revision = None;
    }
    observation.output = "optional-archive-sentinel: unrelated migration notes".into();
    session.tool_result("read_result", observation).await?;
    Ok(())
}

#[tokio::test]
async fn workers_compile_shared_references_and_return_recallable_conclusions() -> TestResult {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(decide)
        .mount(&server)
        .await;
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor) = setup_decision(
        &server,
        harness.clone(),
        vec![
            output(vec![call("read")]),
            output(vec![call("hold-root")]),
            output(vec![text("parent complete")]),
        ],
    )
    .await?;
    read_result(&session, &harness).await?;
    session.drive().await?;
    let before = session.snapshot().await;
    let root = before.agent_id.clone();
    let root_task = before
        .root_turn()
        .ok_or("missing root")?
        .agent_turn_id
        .clone();
    let shared = before.context_store.work[&root_task]
        .evidence
        .iter()
        .find(|id| {
            before.context_store.evidence.get(*id).is_some_and(|block| {
                serde_json::to_string(block)
                    .is_ok_and(|value| value.contains("optional-archive-sentinel"))
            })
        })
        .cloned()
        .ok_or("missing source evidence")?;
    let mut task = work("shared-context child");
    task.fresh_context = false;
    let child = session
        .collaborate(
            "spawn",
            session.head().await.state_revision,
            &root,
            Action::Spawn { task },
        )
        .await?
        .assigned_ids["agent_id"]
        .clone();
    executor.agent_once.lock().await.insert(
        child.clone(),
        vec![text("child observed the retained source")],
    );
    session.drive().await?;
    let state = session.snapshot().await;
    let child_state = &state.agents[&child];
    let child_task = &child_state
        .turn
        .as_ref()
        .ok_or("missing child")?
        .agent_turn_id;
    let child_work = &state.context_store.work[child_task];
    assert!(child_work.shared_evidence.contains(&shared));
    assert!(!serde_json::to_string(&child_state.history)?.contains("optional-archive-sentinel"));
    let prompts = executor.prompts.lock().await;
    let child_prompt = prompts
        .iter()
        .find(|prompt| {
            prompt.system.as_ref().is_some_and(|system| {
                system.starts_with(&format!("You are agent {child} for this task."))
            })
        })
        .ok_or("missing child prompt")?;
    assert!(serde_json::to_string(child_prompt)?.contains("optional-archive-sentinel"));
    assert!(serde_json::to_string(child_prompt)?.contains("Related-task evidence"));
    assert_shared_result(&state, &root_task, child_task)?;
    drop(prompts);
    // The root is deliberately still awaiting a tool. Context delivery alone
    // cannot execute it or inject the worker's transcript into root history.
    assert_eq!(harness.sent.lock().await.len(), 2);
    assert!(
        state.agents[&root]
            .turn
            .as_ref()
            .is_some_and(|turn| turn.invocations.iter().any(|call| call.result.is_none()))
    );
    session.disconnect().await;
    Ok(())
}

fn assert_shared_result(state: &SessionSnapshot, root_task: &str, child_task: &str) -> TestResult {
    let child = &state.context_store.work[child_task];
    assert_eq!(child.result_evidence.len(), 1);
    let reference = &child.result_evidence[0];
    assert!(
        state.context_store.work[root_task]
            .shared_evidence
            .contains(reference)
    );
    assert!(state.agents[&state.agent_id].mailbox.iter().any(|mail| {
        mail.content["evidence_refs"]
            .as_array()
            .is_some_and(|refs| refs.iter().any(|value| value.as_str() == Some(reference)))
    }));
    Ok(())
}

#[tokio::test]
async fn real_decision_http_selects_prompt_and_retains_raw_source_and_cost() -> TestResult {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(decide)
        .expect(1)
        .mount(&server)
        .await;
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor) = setup_decision(
        &server,
        harness.clone(),
        vec![output(vec![call("read")]), output(vec![text("completed")])],
    )
    .await?;
    read_result(&session, &harness).await?;
    session.drive().await?;
    let prompts = executor.prompts.lock().await;
    assert_eq!(prompts.len(), 2);
    let prompt = prompts.last().ok_or("missing final prompt")?;
    assert!(!serde_json::to_string(prompt)?.contains("optional-archive-sentinel"));
    assert!(
        prompt
            .system
            .as_ref()
            .is_some_and(|system| system.contains(&task().text))
    );
    let snapshot = session.snapshot().await;
    let root = snapshot
        .agents
        .get(&snapshot.agent_id)
        .ok_or("missing root")?;
    assert!(serde_json::to_string(&root.history)?.contains("optional-archive-sentinel"));
    assert!(
        serde_json::to_string(&snapshot.context_store.evidence)?
            .contains("optional-archive-sentinel")
    );
    let receipt = snapshot
        .context_store
        .decisions
        .values()
        .next()
        .ok_or("missing receipt")?;
    assert!(receipt.outcome.as_ref().is_some_and(Result::is_ok));
    let cost = snapshot
        .cost_work
        .get(&receipt.run_id)
        .and_then(|run| run.work.get(&receipt.decision_id))
        .ok_or("missing decision cost")?;
    assert_eq!(cost.kind, CostWorkKind::DecisionModel);
    assert_eq!(cost.state, CostWorkState::OutcomeRecorded);
    assert_eq!(
        cost.decision_usage.map(|usage| usage.input_tokens),
        Some(100)
    );
    assert_eq!(cost.decision_estimate_micro_usd, Some(10));
    assert_eq!(
        snapshot.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    Ok(())
}

#[tokio::test]
async fn failed_decision_uses_full_context_with_unknown_cost() -> TestResult {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&server)
        .await;
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor) = setup_decision(
        &server,
        harness.clone(),
        vec![output(vec![call("read")]), output(vec![text("completed")])],
    )
    .await?;
    read_result(&session, &harness).await?;
    session.drive().await?;
    let prompts = executor.prompts.lock().await;
    assert!(
        serde_json::to_string(prompts.last().ok_or("missing prompt")?)?
            .contains("optional-archive-sentinel")
    );
    let snapshot = session.snapshot().await;
    let receipt = snapshot
        .context_store
        .decisions
        .values()
        .next()
        .ok_or("missing receipt")?;
    assert!(receipt.outcome.as_ref().is_some_and(Result::is_err));
    let cost = snapshot
        .cost_work
        .get(&receipt.run_id)
        .and_then(|run| run.work.get(&receipt.decision_id))
        .ok_or("missing cost")?;
    assert!(cost.decision_usage.is_none());
    assert!(cost.decision_estimate_micro_usd.is_none());
    Ok(())
}

#[tokio::test]
async fn unacknowledged_decision_intent_never_dispatches() -> TestResult {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(decide)
        .expect(0)
        .mount(&server)
        .await;
    let harness = Arc::new(Harness::new(Some("context.decision.intent"), None));
    let (session, executor) = setup_decision(
        &server,
        harness.clone(),
        vec![
            output(vec![call("read")]),
            output(vec![text("must not generate")]),
        ],
    )
    .await?;
    read_result(&session, &harness).await?;
    let _ = session.drive().await;
    assert_eq!(executor.prompts.lock().await.len(), 1);
    assert!(session.snapshot().await.context_store.decisions.is_empty());
    Ok(())
}

#[tokio::test]
async fn unacknowledged_decision_outcome_cannot_select_or_generate() -> TestResult {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(decide)
        .expect(1)
        .mount(&server)
        .await;
    let harness = Arc::new(Harness::new(Some("context.decision.outcome"), None));
    let (session, executor) = setup_decision(
        &server,
        harness.clone(),
        vec![
            output(vec![call("read")]),
            output(vec![text("must not generate")]),
        ],
    )
    .await?;
    read_result(&session, &harness).await?;
    let _ = session.drive().await;
    assert_eq!(executor.prompts.lock().await.len(), 1);
    let snapshot = session.snapshot().await;
    let receipt = snapshot
        .context_store
        .decisions
        .values()
        .next()
        .ok_or("missing acknowledged intent")?;
    assert!(receipt.outcome.is_none());
    let cost = snapshot
        .cost_work
        .get(&receipt.run_id)
        .and_then(|run| run.work.get(&receipt.decision_id))
        .ok_or("missing intent cost")?;
    assert_eq!(cost.state, CostWorkState::IntentRecorded);
    Ok(())
}

#[tokio::test]
async fn committed_decision_survives_restart_without_second_provider_call() -> TestResult {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(decide)
        .expect(1)
        .mount(&server)
        .await;
    let harness = Arc::new(Harness::new(None, None));
    let (session, _) = setup_decision(
        &server,
        harness.clone(),
        vec![output(vec![call("read")]), output(vec![text("completed")])],
    )
    .await?;
    read_result(&session, &harness).await?;
    session.drive().await?;
    session.disconnect().await;
    let store = recovery::prefix(&*harness.store.lock().await, "context.decision.outcome")?;
    let replacement = recovery::harness_at(store).await;
    let request = recovery::request(&*replacement.store.lock().await, true)?;
    let mut capabilities = recovery::capabilities(&request.binding.grant.core_instance_id);
    capabilities.operations.push(FEATURE.into());
    let (app, executor) = decision_app(&server, vec![output(vec![text("resumed")])])?;
    let restored = CoreSession::restore(
        request,
        &capabilities,
        app,
        CallerContext::local(),
        replacement,
    )
    .await?;
    restored.drive().await?;
    let prompts = executor.prompts.lock().await;
    assert_eq!(prompts.len(), 1);
    assert!(
        !serde_json::to_string(prompts.first().ok_or("missing resumed prompt")?)?
            .contains("optional-archive-sentinel")
    );
    assert_eq!(restored.snapshot().await.context_store.decisions.len(), 1);
    Ok(())
}

#[tokio::test]
async fn signal_change_invalidates_inflight_decision_and_replans() -> TestResult {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &Request| decide(request).set_delay(Duration::from_millis(150)))
        .expect(2)
        .mount(&server)
        .await;
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor) = setup_decision(
        &server,
        harness.clone(),
        vec![output(vec![call("read")]), output(vec![text("completed")])],
    )
    .await?;
    read_result(&session, &harness).await?;
    let driver = {
        let session = session.clone();
        tokio::spawn(async move { session.drive().await })
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        while server
            .received_requests()
            .await
            .is_none_or(|requests| requests.is_empty())
        {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await?;
    session
        .signals("new_facts", signal_update(&session, Vec::new()).await)
        .await?;
    driver.await??;
    let snapshot = session.snapshot().await;
    assert_eq!(
        snapshot
            .context_store
            .decisions
            .values()
            .filter(|receipt| receipt.stale)
            .count(),
        1
    );
    assert_eq!(
        snapshot
            .context_store
            .decisions
            .values()
            .filter(|receipt| !receipt.stale)
            .count(),
        1
    );
    assert_eq!(executor.prompts.lock().await.len(), 2);
    assert_eq!(
        snapshot.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    Ok(())
}

#[tokio::test]
async fn omitted_tool_exchange_can_be_recalled_as_one_group() -> TestResult {
    use bitrouter_orchestrator::core::context_router::tools::ContextAction;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(decide)
        .expect(2)
        .mount(&server)
        .await;
    let harness = Arc::new(Harness::new(None, None));
    let (session, executor) = setup_decision(
        &server,
        harness.clone(),
        vec![
            output(vec![call("first_read")]),
            output(vec![call("second_read")]),
            output(vec![text("completed")]),
        ],
    )
    .await?;
    read_result(&session, &harness).await?;
    session.drive().await?;
    {
        let prompts = executor.prompts.lock().await;
        assert!(
            !serde_json::to_string(prompts.last().ok_or("missing selected prompt")?)?
                .contains("optional-archive-sentinel")
        );
    }
    let snapshot = session.snapshot().await;
    let block = snapshot
        .context_store
        .evidence
        .values()
        .find(|block| {
            block
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .any(|content| matches!(content, Content::ToolCall { .. }))
        })
        .ok_or("missing source group")?;
    assert_eq!(block.messages.len(), 2);
    session
        .collaborate(
            "recall",
            session.head().await.state_revision,
            &snapshot.agent_id,
            Action::Context(ContextAction::Recall {
                block_ids: vec![block.block_id.clone()],
            }),
        )
        .await?;
    let second = harness
        .sent
        .lock()
        .await
        .last()
        .ok_or("missing second tool")?
        .clone();
    session
        .tool_result("second_result", result(&second))
        .await?;
    session.drive().await?;
    let prompts = executor.prompts.lock().await;
    let last = prompts.last().ok_or("missing recalled prompt")?;
    let wire = serde_json::to_string(last)?;
    assert!(wire.contains("optional-archive-sentinel"));
    assert!(wire.contains("first_read"));
    Ok(())
}

#[tokio::test]
async fn derived_representations_keep_exact_sources_and_expire_with_workspace() -> TestResult {
    use bitrouter_orchestrator::core::context_router::evidence::SourceSpan;
    use bitrouter_orchestrator::core::context_router::tools::ContextAction;
    for (extract, changed, unknown) in [
        (false, false, false),
        (true, false, false),
        (false, true, false),
        (false, false, true),
        (true, false, true),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(decide)
            .expect(2)
            .mount(&server)
            .await;
        let harness = Arc::new(Harness::new(None, None));
        let (session, executor) = setup_decision(
            &server,
            harness.clone(),
            vec![
                output(vec![call("first_read")]),
                output(vec![call("second_read")]),
                output(vec![text("completed")]),
            ],
        )
        .await?;
        if unknown {
            let mut update = signal_update(&session, Vec::new()).await;
            update.workspace_revision = None;
            update.manifest.workspace_revision = None;
            session.signals("unknown_workspace", update).await?;
        }
        read_result(&session, &harness).await?;
        session.drive().await?;
        let snapshot = session.snapshot().await;
        let block = snapshot
            .context_store
            .evidence
            .values()
            .find(|block| !block.protected)
            .ok_or("missing optional group")?;
        let action = if extract {
            ContextAction::Extract {
                block_id: block.block_id.clone(),
                spans: vec![SourceSpan {
                    message_index: 1,
                    content_index: 0,
                    start: 0,
                    end: 8,
                }],
            }
        } else {
            ContextAction::Publish {
                block_ids: vec![block.block_id.clone()],
                summary: "summary-only-sentinel".into(),
            }
        };
        session
            .collaborate(
                "derive",
                session.head().await.state_revision,
                &snapshot.agent_id,
                Action::Context(action),
            )
            .await?;
        let second = harness
            .sent
            .lock()
            .await
            .last()
            .ok_or("missing second read")?
            .clone();
        let mut observation = result(&second);
        if unknown {
            observation.workspace_revision = None;
        }
        if changed {
            observation.workspace_revision = Some("workspace-v3".into());
        }
        session.tool_result("second_result", observation).await?;
        if changed {
            let mut update = signal_update(&session, Vec::new()).await;
            update.workspace_revision = Some("workspace-v3".into());
            update.manifest.workspace_revision = update.workspace_revision.clone();
            session.signals("workspace_changed", update).await?;
        }
        session.drive().await?;
        let prompts = executor.prompts.lock().await;
        let wire = serde_json::to_string(prompts.last().ok_or("missing final prompt")?)?;
        if extract {
            assert!(wire.contains("Exact source extract"));
            assert!(wire.contains("optional"));
        } else {
            assert_eq!(wire.contains("summary-only-sentinel"), !changed);
        }
        assert!(!wire.contains("optional-archive-sentinel"));
        let final_state = session.snapshot().await;
        assert_eq!(
            final_state.context_store.evidence.get(&block.block_id),
            Some(block)
        );
    }
    Ok(())
}
