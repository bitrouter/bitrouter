use super::*;
use bitrouter_ai::types::Prompt;
use bitrouter_sdk::decision_model::DecisionRuntime;
use bitrouter_sdk::decision_model::policy::DecisionPolicy;
use bitrouter_sdk::decision_model::typesafe::TypeSafeExecutor;
use bitrouter_sdk::language_model::ExecutionResult;
use bitrouter_sdk::language_model::context::PipelineContext;
use bitrouter_sdk::language_model::executor::{Executor, StreamPartStream};
use tokio::sync::Mutex;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

#[tokio::test]
async fn oversized_tool_evidence_survives_replay_and_is_readable_only_in_its_task_scope()
-> Result<(), Box<dyn std::error::Error>> {
    use base64::Engine;
    let workspace = TempDir::new()?;
    let source = format!(
        "{}\nretained-evidence-tail",
        "a long source line with exact immutable content\n".repeat(600)
    );
    std::fs::write(workspace.path().join("large.txt"), source)?;
    let runner = super::agent(
        &workspace,
        vec![
            turn(vec![call(
                "large",
                "read",
                serde_json::json!({"path":"large.txt"}),
            )]),
            turn(vec![text("retained")]),
        ],
        |_| {},
    )?;
    let (commits, owner) = commit_recorder();
    let report = runner
        .run_with_approvals(
            "retain large.txt",
            CancellationToken::new(),
            None,
            None,
            Some(commits),
            None,
        )
        .await;
    assert_eq!(report.status, RunStatus::Completed, "{}", report.detail);
    let reference: crate::core::protocol::ArtifactRef = report
        .events
        .iter()
        .find_map(|event| match event {
            RunEvent::ToolFinished {
                output: ToolResultOutput::Json { value },
                ..
            } => value.get("artifact").cloned(),
            _ => None,
        })
        .ok_or("large tool result was not offloaded")
        .and_then(|value| {
            serde_json::from_value(value).map_err(|_| "invalid artifact reference")
        })?;
    let facts = owner.await?;
    let mut body = Vec::new();
    let mut recovered = None;
    for fact in &facts {
        crate::agent::native::Saved::replay(&mut recovered, fact)?;
        if let ExecutionRecord::CoreArtifact {
            reference: artifact,
            offset,
            content_base64,
        } = fact
            && artifact == &reference
        {
            assert_eq!(*offset, body.len() as u64);
            body.extend(base64::engine::general_purpose::STANDARD.decode(content_base64)?);
        }
    }
    assert_eq!(body.len() as u64, reference.bytes);
    assert_eq!(crate::core::checkpoint::sha256(&body), reference.sha256);
    assert!(String::from_utf8(body)?.contains("retained-evidence-tail"));
    let read = call(
        "artifact-page",
        "context_read_artifact",
        serde_json::json!({"artifact_id":reference.artifact_id,"offset":reference.bytes.saturating_sub(128)}),
    );
    let continued = super::agent(
        &workspace,
        vec![turn(vec![read.clone()]), turn(vec![text("recalled")])],
        |_| {},
    )?
    .with_native(recovered);
    let continued = continued
        .run_context(
            RunInput {
                prompt: "recall the retained tail".into(),
                messages: report.messages,
                user_item_id: "recall".into(),
                context_version: report.context_version,
                checkpoint: None,
                complete_checkpoint: false,
                restored_verification: None,
            },
            CancellationToken::new(),
            RunChannels {
                events: None,
                approvals: None,
                commits: None,
                control: None,
            },
        )
        .await;
    assert_eq!(
        continued.status,
        RunStatus::Completed,
        "{}",
        continued.detail
    );
    assert!(continued.events.iter().any(|event| matches!(event, RunEvent::ToolFinished { name, output: ToolResultOutput::Json { value }, .. } if name == "context_read_artifact" && value["content"].as_str().is_some_and(|text| text.contains("retained-evidence-tail")) && value["next_offset"].is_null())));
    let foreign = super::agent(
        &workspace,
        vec![turn(vec![read]), turn(vec![text("denied")])],
        |_| {},
    )?
    .run(
        "guess another task's artifact ID",
        CancellationToken::new(),
        None,
    )
    .await;
    assert_eq!(foreign.status, RunStatus::Completed, "{}", foreign.detail);
    assert!(foreign.events.iter().any(|event| matches!(event, RunEvent::ToolFinished { output: ToolResultOutput::ErrorJson { value }, .. } if value["error"].as_str().is_some_and(|error| error.contains("outside the task")))));
    Ok(())
}

struct Recording {
    mock: MockExecutor,
    prompts: Mutex<Vec<Prompt>>,
    output_limit_support: Option<bool>,
}

#[async_trait::async_trait]
impl Executor for Recording {
    fn output_token_limit_support(&self, _: &RoutingTarget) -> Option<bool> {
        self.output_limit_support
    }
    async fn execute(
        &self,
        _: &RoutingTarget,
        _: &Prompt,
        _: &PipelineContext,
    ) -> bitrouter_sdk::Result<ExecutionResult> {
        Err(bitrouter_sdk::error::BitrouterError::internal(
            "native fixture requires streaming",
        ))
    }
    async fn execute_stream(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        context: &PipelineContext,
    ) -> bitrouter_sdk::Result<StreamPartStream> {
        self.prompts.lock().await.push(prompt.clone());
        self.mock.execute_stream(target, prompt, context).await
    }
}

fn decision(request: &Request) -> ResponseTemplate {
    judgment(request, 0.99)
}

fn judgment(request: &Request, suitability: f64) -> ResponseTemplate {
    let Ok(request) = serde_json::from_slice::<serde_json::Value>(&request.body) else {
        return ResponseTemplate::new(400);
    };
    let Some(questions) = request["questions"].as_object() else {
        return ResponseTemplate::new(400);
    };
    let answers = questions.iter().map(|(id, question)| {
        if question["type"] == "noul" {
            return (id.clone(), serde_json::json!({"type":"noul", "noul":suitability}));
        }
        let selected = if id.starts_with("routing_") { "unknown" } else { "hide" };
        let probabilities = question["criteria"].as_object().into_iter().flat_map(|criteria| criteria.keys())
            .map(|key| (key.clone(), serde_json::json!(if key == selected { 1.0 } else { 0.0 })))
            .collect::<serde_json::Map<_, _>>();
        (id.clone(), serde_json::json!({"type":"choice", "choice":selected, "confidence":1.0, "probabilities":probabilities}))
    }).collect::<serde_json::Map<_, _>>();
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "model":"decision-serving-model", "answers":answers, "usage":{"input_tokens":30,"output_tokens":3}
    }))
}

fn app(
    server: &MockServer,
    turns: Vec<GenerateResult>,
) -> Result<(Arc<App>, Arc<Recording>), Box<dyn std::error::Error>> {
    app_with_policy(
        server,
        turns,
        DecisionPolicy {
            target_context_bytes: 1,
            retain_recent_groups: 0,
            ..Default::default()
        },
    )
}

fn app_with_policy(
    server: &MockServer,
    turns: Vec<GenerateResult>,
    policy: DecisionPolicy,
) -> Result<(Arc<App>, Arc<Recording>), Box<dyn std::error::Error>> {
    let executor = Arc::new(Recording {
        mock: MockExecutor::new(turns.into_iter().map(mock_stream).collect()),
        prompts: Mutex::new(Vec::new()),
        output_limit_support: None,
    });
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target()]);
    table.insert("efficient-model", vec![target()]);
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(executor.clone());
        })
        .decision_model(DecisionRuntime {
            model: "decision-alias".into(),
            executor: Arc::new(TypeSafeExecutor::new(
                &server.uri(),
                "fixture",
                Duration::from_secs(5),
                8192,
            )?),
            policy,
            pricing: None,
        })
        .build()?;
    Ok((Arc::new(app), executor))
}

#[tokio::test]
async fn native_subscription_reservation_survives_configuration_restore()
-> Result<(), Box<dyn std::error::Error>> {
    for (capacity, reservation, admitted) in [
        (Some(128_000), None, true),
        (Some(32_000), None, true),
        (Some(128_000), Some(128_000), true),
        (Some(128_000), Some(4096), false),
        (None, None, false),
    ] {
        let workspace = TempDir::new()?;
        let executor = Arc::new(Recording {
            mock: MockExecutor::new(vec![mock_stream(turn(vec![text("completed")]))]),
            prompts: Mutex::new(Vec::new()),
            output_limit_support: Some(false),
        });
        let table = StaticRoutingTable::new();
        let mut route = target();
        route.model_constraints.token_limits.max_output_tokens = capacity;
        table.insert("subscription-model", vec![route]);
        let app = Arc::new(
            App::builder()
                .language_model(|builder| {
                    builder
                        .routing_table(Arc::new(table))
                        .executor(executor.clone());
                })
                .build()?,
        );
        let config =
            AgentConfig::fixed("subscription-model", None).with_output_reservation(reservation);
        let restored = serde_json::from_slice(&serde_json::to_vec(&config)?)?;
        let report = Agent::new(app, CallerContext::local(), workspace.path(), restored)?
            .run("Reply briefly", CancellationToken::new(), None)
            .await;
        let prompts = executor.prompts.lock().await;
        if admitted {
            assert_eq!(report.status, RunStatus::Completed, "{}", report.detail);
            assert_eq!(prompts.len(), 1);
            assert_eq!(prompts[0].params.max_tokens.map(u64::from), capacity);
        } else {
            assert_eq!(report.status, RunStatus::Failed);
            assert!(
                prompts.is_empty(),
                "an insufficient reservation must not dispatch"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn native_output_budget_follows_the_effort_eligible_route()
-> Result<(), Box<dyn std::error::Error>> {
    use bitrouter_ai::types::{ReasoningEffort, ReasoningEffortConfig};
    for (ineligible, eligible) in [(128_000, 32_000), (32_000, 128_000)] {
        let workspace = TempDir::new()?;
        let executor = Arc::new(Recording {
            mock: MockExecutor::new(vec![mock_stream(turn(vec![text("completed")]))]),
            prompts: Mutex::new(Vec::new()),
            output_limit_support: Some(false),
        });
        let mut routes = Vec::new();
        for (capacity, effort) in [
            (ineligible, ReasoningEffort::Low),
            (eligible, ReasoningEffort::High),
        ] {
            let mut route = target();
            route.reasoning_effort = Some(ReasoningEffortConfig {
                levels: vec![effort],
                default: None,
            });
            route.model_constraints.token_limits.max_output_tokens = Some(capacity);
            routes.push(route);
        }
        let table = StaticRoutingTable::new();
        table.insert("subscription-model", routes);
        let app = Arc::new(
            App::builder()
                .language_model(|builder| {
                    builder
                        .routing_table(Arc::new(table))
                        .executor(executor.clone());
                })
                .build()?,
        );
        let report = Agent::new(
            app,
            CallerContext::local(),
            workspace.path(),
            AgentConfig::fixed("subscription-model", Some(ReasoningEffort::High)),
        )?
        .run("Reply briefly", CancellationToken::new(), None)
        .await;
        assert_eq!(report.status, RunStatus::Completed, "{}", report.detail);
        let prompts = executor.prompts.lock().await;
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].params.max_tokens.map(u64::from), Some(eligible));
    }
    Ok(())
}

#[tokio::test]
async fn native_cumulative_tool_limit_rejects_later_effects()
-> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(decision)
        .mount(&server)
        .await;
    let workspace = TempDir::new()?;
    std::fs::write(workspace.path().join("input.txt"), "source")?;
    let (app, executor) = app(
        &server,
        vec![
            turn(vec![call(
                "read",
                "read",
                serde_json::json!({"path":"input.txt"}),
            )]),
            turn(vec![call(
                "write",
                "write",
                serde_json::json!({"path":"over-budget.txt","content":"forbidden"}),
            )]),
        ],
    )?;
    let mut config = AgentConfig::fixed("fixture-model", None);
    config.max_tool_calls = 1;
    let report = Agent::new(app, CallerContext::local(), workspace.path(), config)?
        .run("Read and then write", CancellationToken::new(), None)
        .await;
    assert_ne!(report.status, RunStatus::Completed);
    assert!(
        report.detail.contains("tool-call budget"),
        "{}",
        report.detail
    );
    assert_eq!(report.tool_calls, 1);
    assert_eq!(executor.prompts.lock().await.len(), 2);
    assert!(!workspace.path().join("over-budget.txt").exists());
    Ok(())
}

#[tokio::test]
async fn native_spend_limit_stops_new_model_work_without_losing_settlement()
-> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    let workspace = TempDir::new()?;
    std::fs::write(workspace.path().join("input.txt"), "source")?;
    let (app, executor) = app(
        &server,
        vec![
            turn(vec![call(
                "read",
                "read",
                serde_json::json!({"path":"input.txt"}),
            )]),
            turn(vec![Content::Text {
                text: "must not be generated".into(),
                provider_metadata: Default::default(),
            }]),
        ],
    )?;
    let mut config = AgentConfig::fixed("fixture-model", None);
    config.estimate_rates = Some(EstimateRates {
        prompt: 1_000_000,
        completion: 1_000_000,
    });
    config.max_spend_microusd = Some(1);
    let report = Agent::new(app, CallerContext::local(), workspace.path(), config)?
        .run("Read then report", CancellationToken::new(), None)
        .await;
    assert_eq!(report.status, RunStatus::BoundExceeded, "{}", report.detail);
    assert!(report.estimated_spend_microusd >= 1);
    assert_eq!(executor.prompts.lock().await.len(), 1);
    assert!(!report.unknown_effect);
    assert!(
        report.native.is_some(),
        "Core must retain a settled checkpoint: {}",
        report.detail
    );
    assert!(
        server
            .received_requests()
            .await
            .ok_or("missing request log")?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn native_agent_executes_context_decision_through_core_and_thread_commit_channel()
-> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(decision)
        .expect(1)
        .mount(&server)
        .await;
    let workspace = TempDir::new()?;
    std::fs::write(
        workspace.path().join("archive.txt"),
        "native-optional-evidence-sentinel",
    )?;
    let (app, executor) = app(
        &server,
        vec![
            turn(vec![call(
                "read-archive",
                "read",
                serde_json::json!({"path":"archive.txt"}),
            )]),
            turn(vec![Content::Text {
                text: "native result".into(),
                provider_metadata: Default::default(),
            }]),
        ],
    )?;
    let agent = Agent::new(
        app,
        CallerContext::new("native-key", "native-user"),
        workspace.path(),
        AgentConfig::fixed("fixture-model", None),
    )?;
    let (sender, owner) = commit_recorder();
    let report = tokio::time::timeout(
        Duration::from_secs(15),
        agent.run_with_approvals(
            "Read the archive, then report completion",
            CancellationToken::new(),
            None,
            None,
            Some(sender),
            None,
        ),
    )
    .await?;
    assert_eq!(report.status, RunStatus::Completed, "{}", report.detail);
    assert_eq!(report.final_answer.as_deref(), Some("native result"));
    assert!(serde_json::to_string(&report.messages)?.contains("native-optional-evidence-sentinel"));
    let prompts = executor.prompts.lock().await;
    assert_eq!(prompts.len(), 2);
    assert!(!serde_json::to_string(&prompts[1])?.contains("native-optional-evidence-sentinel"));
    let records = owner.await?;
    let states = records
        .iter()
        .filter_map(|record| match record {
            ExecutionRecord::CoreCheckpoint { batch, .. } => {
                Some(batch.decode(&crate::core::protocol::Limits {
                    input_bytes: 1024 * 1024,
                    checkpoint_bytes: 16 * 1024 * 1024,
                    unacknowledged_bytes: 32 * 1024 * 1024,
                    ..Default::default()
                }))
            }
            _ => None,
        })
        .collect::<Result<Vec<_>, _>>()?;
    let state: crate::core::session::SessionSnapshot = serde_json::from_value(
        states
            .last()
            .ok_or("Core checkpoint missing")?
            .checkpoint
            .state
            .clone(),
    )?;
    assert_eq!(state.context_store.decisions.len(), 1);
    assert!(
        state
            .context_store
            .views
            .values()
            .any(|view| !view.omitted.is_empty())
    );
    assert!(
        records
            .iter()
            .any(|record| matches!(record, ExecutionRecord::ModelResponse { .. }))
    );
    Ok(())
}

#[tokio::test]
async fn native_core_tool_effect_still_requires_native_approval()
-> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(decision)
        .mount(&server)
        .await;
    let workspace = TempDir::new()?;
    let (app, _) = app(
        &server,
        vec![
            turn(vec![call(
                "write-file",
                "write",
                serde_json::json!({"path":"denied.txt","content":"forbidden"}),
            )]),
            turn(vec![Content::Text {
                text: "approval denied".into(),
                provider_metadata: Default::default(),
            }]),
        ],
    )?;
    let agent = Agent::new(
        app,
        CallerContext::new("native-key", "native-user"),
        workspace.path(),
        AgentConfig::fixed("fixture-model", None),
    )?;
    let (approvals, mut receive) = mpsc::channel::<ApprovalRequest>(1);
    let owner = tokio::spawn(async move {
        let request = receive.recv().await.ok_or("missing native approval")?;
        assert_eq!(request.tool_name, "write");
        tokio::time::sleep(Duration::from_millis(600)).await;
        request
            .response
            .send(false)
            .map_err(|_| "approval receiver closed")
    });
    let elapsed = Instant::now();
    let report = tokio::time::timeout(
        Duration::from_secs(15),
        agent.run_with_approvals(
            "Write a file",
            CancellationToken::new(),
            None,
            Some(approvals),
            None,
            None,
        ),
    )
    .await?;
    owner.await??;
    assert_eq!(report.status, RunStatus::Completed, "{}", report.detail);
    assert!(!workspace.path().join("denied.txt").exists());
    assert!(
        elapsed
            .elapsed()
            .as_millis()
            .saturating_sub(u128::from(report.active_duration_ms))
            >= 500,
        "approval wait was charged as active execution"
    );
    Ok(())
}

#[tokio::test]
async fn native_checkpoint_capacity_failure_reports_the_resource_cause()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    std::fs::write(
        workspace.path().join("large.txt"),
        "retained evidence ".repeat(250),
    )?;
    let mut responses = (0..32)
        .map(|index| {
            turn(vec![call(
                &format!("read-{index}"),
                "read",
                serde_json::json!({"path":"large.txt"}),
            )])
        })
        .collect::<Vec<_>>();
    responses.push(turn(vec![text("done")]));
    let agent = super::agent(&workspace, responses, |_| {})?;
    let report = agent
        .run("Read the evidence", CancellationToken::new(), None)
        .await;
    assert_eq!(report.status, RunStatus::BoundExceeded, "{}", report.detail);
    assert!(
        report.steps < 32,
        "fixture reached the attempt limit instead of checkpoint capacity"
    );
    assert!(!report.unknown_effect);
    assert!(
        report.detail.contains("checkpoint capacity exhausted"),
        "{}",
        report.detail
    );
    Ok(())
}
