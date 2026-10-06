use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bitrouter_sdk::App;
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::types::{AuthScheme, GenerateResult, RoutingTarget};
use bitrouter_sdk::language_model::{
    ApiProtocol, Content, FinishReason, Message, MockExecutor, MockResponse, Role,
    StaticRoutingTable, StreamPart, ToolResultOutput, Usage,
};
use tempfile::TempDir;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::context;
use crate::control::{ModelBoundary, TurnControl};
use crate::store::{CommitRequest, EffectStatus, ExecutionRecord};

struct ReleaseReads(Arc<crate::tools::ReadGate>);
impl Drop for ReleaseReads {
    fn drop(&mut self) {
        let _ = self.0.allow(None);
    }
}

async fn wait_for_reads(
    gate: &crate::tools::ReadGate,
    count: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if gate.entered()?.0.len() >= count {
                return Ok::<_, String>(());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await??;
    Ok(())
}

fn commit_recorder() -> (
    mpsc::Sender<CommitRequest>,
    tokio::task::JoinHandle<Vec<ExecutionRecord>>,
) {
    let (sender, mut receiver) = mpsc::channel::<CommitRequest>(1);
    let owner = tokio::spawn(async move {
        let mut records = Vec::new();
        while let Some(request) = receiver.recv().await {
            records.extend(request.records);
            let _ = request.response.send(Ok(()));
        }
        records
    });
    (sender, owner)
}

fn target() -> RoutingTarget {
    RoutingTarget {
        provider_name: "fixture".into(),
        service_id: "fixture-model".into(),
        api_base: "https://example.invalid".into(),
        api_key: "fixture-key".into(),
        api_protocol: ApiProtocol::ChatCompletions,
        chat_token_limit_field: None,
        chat_supports_store: None,
        chat_supports_stream_options: None,
        reasoning_effort: None,
        model_constraints: Default::default(),
        account_label: None,
        api_key_override: None,
        api_base_override: None,
        auth_scheme: AuthScheme::Bearer,
        headers: Vec::new(),
    }
}

fn scripted_app(turns: Vec<GenerateResult>) -> std::io::Result<Arc<App>> {
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target()]);
    let responses = turns.into_iter().map(mock_stream).collect();
    App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(Arc::new(MockExecutor::new(responses)));
        })
        .build()
        .map(Arc::new)
        .map_err(std::io::Error::other)
}

fn turn(content: Vec<Content>) -> GenerateResult {
    let finish_reason = if content
        .iter()
        .any(|part| matches!(part, Content::ToolCall { .. }))
    {
        FinishReason::ToolCalls
    } else {
        FinishReason::Stop
    };
    GenerateResult {
        content,
        usage: Some(Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            ..Default::default()
        }),
        finish_reason: Some(finish_reason),
        response_id: None,
        stop_details: None,
        provider_metadata: Default::default(),
    }
}

fn mock_stream(turn: GenerateResult) -> MockResponse {
    let mut parts = Vec::new();
    for content in turn.content {
        match content {
            Content::Text { text, .. } => parts.push(StreamPart::TextDelta { text }),
            Content::ToolCall {
                id,
                name,
                arguments,
                provider_metadata,
                ..
            } => {
                parts.push(StreamPart::ToolCallDelta {
                    id,
                    name: Some(name),
                    arguments,
                    provider_metadata,
                });
            }
            _ => {}
        }
    }
    if let Some(usage) = turn.usage {
        parts.push(StreamPart::Usage { usage });
    }
    if let Some(reason) = turn.finish_reason {
        parts.push(StreamPart::Finish { reason });
    }
    MockResponse::Stream(parts)
}

fn call(id: &str, name: &str, arguments: serde_json::Value) -> Content {
    Content::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: arguments.to_string(),
        provider_executed: false,
        dynamic: false,
        provider_metadata: Default::default(),
    }
}

fn text(value: &str) -> Content {
    Content::Text {
        text: value.into(),
        provider_metadata: Default::default(),
    }
}

fn agent(
    workspace: &TempDir,
    turns: Vec<GenerateResult>,
    configure: impl FnOnce(&mut AgentConfig),
) -> std::io::Result<Agent> {
    let app = scripted_app(turns)?;
    let mut config = AgentConfig::fixed("fixture-model", None);
    configure(&mut config);
    Agent::new(app, CallerContext::local(), workspace.path(), config).map_err(std::io::Error::other)
}

#[tokio::test]
async fn gemini_optional_ids_execute_separately_and_replay_provider_ids()
-> Result<(), Box<dyn std::error::Error>> {
    use bitrouter_sdk::language_model::protocol::{
        OutboundAdapter, SseEvent, generate_content::GenerateContentAdapter,
    };

    let workspace = TempDir::new()?;
    for name in ["first.txt", "second.txt", "third.txt"] {
        std::fs::write(workspace.path().join(name), name)?;
    }
    let adapter = GenerateContentAdapter;
    let wire = serde_json::json!({
        "candidates": [{"content": {"role":"model", "parts":[
            {"functionCall":{"name":"read", "args":{"path":"first.txt"}}, "thoughtSignature":"signature"},
            {"functionCall":{"name":"read", "args":{"path":"second.txt"}}},
            {"functionCall":{"id":"provider-3", "name":"read", "args":{"path":"third.txt"}}}
        ]}, "finishReason":"STOP"}]
    });
    let mut decoder = adapter.stream_decoder();
    let mut parts = decoder.decode(&SseEvent {
        event: None,
        data: wire.to_string(),
    })?;
    parts.extend(decoder.finish()?);
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target()]);
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(Arc::new(MockExecutor::new(vec![
                    MockResponse::Stream(parts),
                    mock_stream(turn(vec![text("inspected")])),
                ])));
        })
        .build()?;
    let agent = Agent::new(
        Arc::new(app),
        CallerContext::local(),
        workspace.path(),
        AgentConfig::fixed("fixture-model", None).read_only(),
    )?;
    let report = agent.run("inspect", CancellationToken::new(), None).await;
    assert_eq!(report.status, RunStatus::Completed);
    let mut ids = HashSet::new();
    let outputs: Vec<_> = report
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            Content::ToolResult {
                call_id, output, ..
            } => {
                assert!(!call_id.is_empty());
                assert!(ids.insert(call_id.clone()));
                Some(output.to_provider_string())
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        outputs,
        [
            "File \"first.txt\"\nL1: first.txt\n",
            "File \"second.txt\"\nL1: second.txt\n",
            "File \"third.txt\"\nL1: third.txt\n"
        ]
    );
    assert!(ids.contains("provider-3"));
    let prompt = context::build(
        "fixture",
        None,
        "inspect",
        &report.messages,
        agent.tools.declarations(),
        512 * 1024,
    )?;
    let replay = adapter.render_request(&prompt)?;
    let calls = &replay["contents"][1]["parts"];
    assert!(calls[0]["functionCall"].get("id").is_none());
    assert!(calls[1]["functionCall"].get("id").is_none());
    assert_eq!(calls[0]["thoughtSignature"], "signature");
    assert_eq!(calls[2]["functionCall"]["id"], "provider-3");
    let results: Vec<_> = replay["contents"]
        .as_array()
        .ok_or("missing contents")?
        .iter()
        .flat_map(|content| content["parts"].as_array().into_iter().flatten())
        .filter_map(|part| part.get("functionResponse"))
        .collect();
    assert_eq!(results.len(), 3);
    assert!(results[0].get("id").is_none());
    assert!(results[1].get("id").is_none());
    assert_eq!(results[2]["id"], "provider-3");
    Ok(())
}

#[tokio::test]
async fn ordered_read_edit_shell_then_final_answer() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    std::fs::write(workspace.path().join("note.txt"), "old\n")?;
    let agent = agent(
        &workspace,
        vec![
            turn(vec![
                call("read-1", "read", serde_json::json!({"path":"note.txt"})),
                call(
                    "patch-1",
                    "edit",
                    serde_json::json!({
                        "path":"note.txt", "edits":[{"oldText":"old", "newText":"new"}]
                    }),
                ),
            ]),
            turn(vec![call(
                "check-1",
                "shell",
                serde_json::json!({"command":"echo checked"}),
            )]),
            turn(vec![text("Changed the note and ran a check.")]),
        ],
        |_| {},
    )?;
    let report = agent
        .run("update the note", CancellationToken::new(), None)
        .await;
    assert_eq!(report.status, RunStatus::Completed);
    assert_eq!(
        report.final_answer.as_deref(),
        Some("Changed the note and ran a check.")
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("note.txt"))?,
        "new\n"
    );
    let names: Vec<&str> = report
        .events
        .iter()
        .filter_map(|event| match event {
            RunEvent::ToolStarted { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(names, ["read", "edit", "shell"]);
    assert_eq!(report.messages.len(), 7);
    assert!(matches!(
        &report.messages[2].content[0],
        Content::ToolResult { call_id, output: ToolResultOutput::Text { value }, .. }
            if call_id == "read-1" && value == "File \"note.txt\"\nL1: old\n"
    ));
    let rebuilt = context::build(
        "fixture-model",
        None,
        "fixture instructions",
        &report.messages,
        agent.tools.declarations(),
        512 * 1024,
    )
    .map_err(std::io::Error::other)?;
    assert!(
        rebuilt
            .system
            .as_deref()
            .is_some_and(|system| system.starts_with("fixture instructions"))
    );
    assert_eq!(rebuilt.messages, report.messages);
    Ok(())
}

#[tokio::test]
async fn failed_batch_settles_and_provider_ids_can_repeat_in_later_steps()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    std::fs::write(workspace.path().join("note.txt"), "old")?;
    let mut malformed = call("bad-json", "edit", serde_json::json!({}));
    if let Content::ToolCall { arguments, .. } = &mut malformed {
        *arguments = "not json".into();
    }
    let agent = agent(
        &workspace,
        vec![
            turn(vec![
                call("unknown", "not_a_tool", serde_json::json!({})),
                malformed,
                call("dup", "read", serde_json::json!({"path":"note.txt"})),
            ]),
            turn(vec![call(
                "dup",
                "edit",
                serde_json::json!({
                    "path":"note.txt", "edits":[{"oldText":"old", "newText":"changed"}]
                }),
            )]),
            turn(vec![text("I received the tool errors.")]),
        ],
        |_| {},
    )?;
    let report = agent.run("try tools", CancellationToken::new(), None).await;
    assert_eq!(report.status, RunStatus::Completed);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("note.txt"))?,
        "changed"
    );
    let outputs: Vec<&ToolResultOutput> = report
        .events
        .iter()
        .filter_map(|event| match event {
            RunEvent::ToolFinished { output, .. } => Some(output),
            _ => None,
        })
        .collect();
    assert_eq!(outputs.len(), 4);
    assert!(outputs[0].is_error());
    assert!(outputs[1].is_error());
    assert!(outputs[2].is_error());
    assert!(!outputs[3].is_error());
    Ok(())
}

#[tokio::test]
async fn read_only_mode_denies_unadvertised_effects_without_approval()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    std::fs::write(workspace.path().join("note.txt"), "unchanged\n")?;
    let agent = agent(
        &workspace,
        vec![
            turn(vec![
                call(
                    "inspect",
                    "grep",
                    serde_json::json!({"pattern":"unchanged"}),
                ),
                call(
                    "write",
                    "write",
                    serde_json::json!({"path":"note.txt","content":"changed"}),
                ),
                call(
                    "shell",
                    "shell",
                    serde_json::json!({"command":"touch created.txt"}),
                ),
            ]),
            turn(vec![text("Inspection complete.")]),
        ],
        |config| *config = AgentConfig::fixed("fixture-model", None).read_only(),
    )?;
    let tools = agent.tools.declarations();
    let names = tools
        .iter()
        .filter_map(|tool| match tool {
            bitrouter_sdk::language_model::Tool::Function { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(names, ["read", "glob", "grep"]);
    let (approvals, mut approval_requests) = mpsc::channel(64);
    let report = agent
        .run_with_approvals(
            "inspect",
            CancellationToken::new(),
            None,
            Some(approvals),
            None,
            None,
        )
        .await;
    assert!(approval_requests.try_recv().is_err());
    assert_eq!(report.status, RunStatus::Completed);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("note.txt"))?,
        "unchanged\n"
    );
    assert!(!workspace.path().join("created.txt").exists());
    let outputs = report
        .events
        .iter()
        .filter_map(|event| match event {
            RunEvent::ToolFinished { output, .. } => Some(output),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(outputs.len(), 3);
    assert!(!outputs[0].is_error());
    assert!(outputs[1].is_error());
    assert!(outputs[2].is_error());
    Ok(())
}

#[tokio::test]
async fn cancellation_and_each_bound_stop_before_a_new_effect()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let effect = turn(vec![call(
        "create",
        "write",
        serde_json::json!({"path":"created.txt", "content":"created"}),
    )]);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let cancelled = agent(&workspace, vec![effect.clone()], |_| {})?
        .run("create", cancel, None)
        .await;
    assert_eq!(cancelled.status, RunStatus::Cancelled);

    let step_limited = agent(&workspace, vec![effect.clone()], |config| {
        config.max_steps = 1
    })?
    .run("create", CancellationToken::new(), None)
    .await;
    assert_eq!(step_limited.status, RunStatus::BoundExceeded);
    assert_eq!(step_limited.steps, 1);
    assert_eq!(step_limited.tool_calls, 1);
    context::validate_history(&step_limited.messages)?;
    std::fs::remove_file(workspace.path().join("created.txt"))?;

    let spend_limited = agent(&workspace, vec![effect.clone()], |config| {
        config.max_spend_microusd = Some(1);
        config.estimate_rates = Some(EstimateRates {
            prompt: 1_000_000,
            completion: 1_000_000,
        });
    })?
    .run("create", CancellationToken::new(), None)
    .await;
    assert_eq!(spend_limited.status, RunStatus::BoundExceeded);

    let context_limited = agent(&workspace, vec![effect], |config| {
        config.max_context_bytes = 1;
    })?
    .run("create", CancellationToken::new(), None)
    .await;
    assert_eq!(context_limited.status, RunStatus::BoundExceeded);

    let time_agent = agent(&workspace, vec![], |config| {
        config.max_duration = Duration::from_millis(1);
    })?;
    let empty_report = RunReport {
        cleanup_unconfirmed: false,
        resources: None,
        context_version: 0,
        status: RunStatus::Failed,
        final_answer: None,
        detail: String::new(),
        messages: Vec::new(),
        events: Vec::new(),
        steps: 0,
        estimated_spend_microusd: 0,
        tool_calls: 0,
        active_duration_ms: 0,
        unknown_effect: false,
    };
    assert!(matches!(
        time_agent.bound_status(
            &empty_report,
            Instant::now() - Duration::from_secs(1),
            &CancellationToken::new()
        ),
        Some((RunStatus::BoundExceeded, _))
    ));
    assert!(!workspace.path().join("created.txt").exists());
    Ok(())
}

#[tokio::test]
async fn streamed_text_is_visible_before_the_complete_message()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let agent = agent(&workspace, vec![turn(vec![text("hello")])], |_| {})?;
    let (sender, mut receiver) = mpsc::channel(64);
    let (commits, owner) = commit_recorder();
    let report = agent
        .run_with_approvals(
            "greet",
            CancellationToken::new(),
            Some(sender),
            None,
            Some(commits),
            Some("user-fixture".into()),
        )
        .await;
    let records = owner.await?;
    let request_item = records
        .iter()
        .find_map(|record| match record {
            ExecutionRecord::ModelRequest { item_id, .. } => Some(item_id.clone()),
            _ => None,
        })
        .ok_or("model request missing")?;
    assert!(!request_item.is_empty());
    assert_eq!(records.iter().filter(|record| matches!(record, ExecutionRecord::ModelResponse { item_id, .. } if item_id == &request_item)).count(), 1);
    assert!(
        matches!(&report.events[0], RunEvent::UserMessage { item_id, .. } if item_id == "user-fixture")
    );
    assert_eq!(report.status, RunStatus::Completed);
    let mut saw_delta = false;
    while let Ok(event) = receiver.try_recv() {
        match event {
            RunEvent::AssistantStarted { item_id, .. } => assert_eq!(item_id, request_item),
            RunEvent::AssistantDelta { text, item_id } => {
                assert_eq!(item_id, request_item);
                assert_eq!(text, "hello");
                saw_delta = true;
            }
            RunEvent::AssistantMessage { item_id, .. } => {
                assert_eq!(item_id, request_item);
                assert!(saw_delta);
            }
            _ => {}
        }
    }
    assert!(saw_delta);
    assert!(
        report
            .events
            .iter()
            .all(|event| !matches!(event, RunEvent::AssistantDelta { .. }))
    );
    Ok(())
}

#[tokio::test]
async fn length_truncated_tool_call_never_executes() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let table = StaticRoutingTable::new();
    table.insert("fixture-model", vec![target()]);
    let app = App::builder()
        .language_model(|builder| {
            builder
                .routing_table(Arc::new(table))
                .executor(Arc::new(MockExecutor::new(vec![MockResponse::Stream(
                    vec![
                        StreamPart::TextDelta {
                            text: "Creating the file".into(),
                        },
                        StreamPart::ToolCallDelta {
                            id: "write-1".into(),
                            name: Some("write".into()),
                            arguments: r#"{"path":"created.txt","content":"created"}"#.into(),
                            provider_metadata: Default::default(),
                        },
                        StreamPart::Finish {
                            reason: FinishReason::Length,
                        },
                    ],
                )])));
        })
        .build()?;
    let agent = Agent::new(
        Arc::new(app),
        CallerContext::local(),
        workspace.path(),
        AgentConfig::fixed("fixture-model", None),
    )
    .map_err(std::io::Error::other)?;
    let (commits, owner) = commit_recorder();
    let report = agent
        .run_with_approvals(
            "create",
            CancellationToken::new(),
            None,
            None,
            Some(commits),
            None,
        )
        .await;
    let records = owner.await?;
    let request_item = records
        .iter()
        .find_map(|record| match record {
            ExecutionRecord::ModelRequest { item_id, .. } => Some(item_id),
            _ => None,
        })
        .ok_or("request missing")?;
    let partial = records
        .iter()
        .find_map(|record| match record {
            ExecutionRecord::ModelInterrupted {
                item_id, partial, ..
            } if item_id == request_item => Some(partial),
            _ => None,
        })
        .ok_or("interrupted Item missing")?;
    assert!(
        matches!(&partial.content[0], Content::Text { text, .. } if text == "Creating the file")
    );
    assert_eq!(report.messages.len(), 1);
    assert!(
        !records
            .iter()
            .any(|record| matches!(record, ExecutionRecord::ModelResponse { .. }))
    );
    assert_eq!(report.status, RunStatus::Failed);
    assert!(!workspace.path().join("created.txt").exists());
    assert!(
        report
            .events
            .iter()
            .all(|event| !matches!(event, RunEvent::ToolStarted { .. }))
    );
    Ok(())
}

#[tokio::test]
async fn reads_overlap_with_bounds_and_edit_barriers_preserve_context_order()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    for path in ["a", "b", "c"] {
        std::fs::write(workspace.path().join(path), "old\n")?;
    }
    let mut runner = agent(
        &workspace,
        vec![
            turn(vec![
                call("a", "read", serde_json::json!({"path":"a"})),
                call("b", "read", serde_json::json!({"path":"b"})),
                call("c", "read", serde_json::json!({"path":"c"})),
                call(
                    "edit",
                    "edit",
                    serde_json::json!({"path":"a", "edits":[{"oldText":"old", "newText":"new"}]}),
                ),
                call("after", "read", serde_json::json!({"path":"a"})),
            ]),
            turn(vec![text("done")]),
        ],
        |_| {},
    )?;
    let gate = Arc::new(crate::tools::ReadGate::default());
    let _release = ReleaseReads(Arc::clone(&gate));
    runner.tools.set_read_gate(Arc::clone(&gate));
    let workers = Arc::new(Semaphore::new(16));
    let runner = runner.with_tool_workers(Arc::clone(&workers), 2);
    let (events, mut updates) = mpsc::channel(128);
    let (commits, owner) = commit_recorder();
    let run = tokio::spawn(async move {
        runner
            .run_with_approvals(
                "inspect then edit",
                CancellationToken::new(),
                Some(events),
                None,
                Some(commits),
                None,
            )
            .await
    });
    wait_for_reads(&gate, 2).await?;
    assert_eq!(gate.entered()?.0.len(), 2);
    assert_eq!(workers.available_permits(), 14);
    gate.allow(Some("b"))?;
    wait_for_reads(&gate, 3).await?;
    gate.allow(Some("c"))?;
    let mut completed_reads = 0;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = updates.recv().await {
            if matches!(event, RunEvent::ToolFinished { name, .. } if name == "read") {
                completed_reads += 1;
                if completed_reads == 2 {
                    break;
                }
            }
        }
    })
    .await?;
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("a"))?,
        "old\n"
    );
    assert!(!run.is_finished());
    gate.allow(Some("a"))?;
    let report = tokio::time::timeout(Duration::from_secs(5), run).await??;
    let records = owner.await?;
    assert_eq!(report.status, RunStatus::Completed);
    assert_eq!(gate.entered()?.1, 2);
    assert_eq!(workers.available_permits(), 16);
    let result_ids = |messages: &[Message]| {
        messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|part| match part {
                Content::ToolResult { call_id, .. } => Some(call_id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        result_ids(&report.messages),
        ["a", "b", "c", "edit", "after"]
    );
    let completion_messages = records
        .iter()
        .filter_map(|record| match record {
            ExecutionRecord::ToolResult { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        result_ids(&completion_messages),
        ["b", "c", "a", "edit", "after"]
    );
    assert!(report.messages.iter().flat_map(|message| &message.content).any(|part| matches!(part, Content::ToolResult { call_id, output: ToolResultOutput::Text { value }, .. } if call_id == "after" && value.contains("new"))));
    Ok(())
}

#[tokio::test]
async fn independent_tasks_share_the_global_worker_bound() -> Result<(), Box<dyn std::error::Error>>
{
    let gate = Arc::new(crate::tools::ReadGate::default());
    let _release = ReleaseReads(Arc::clone(&gate));
    let workers = Arc::new(Semaphore::new(2));
    let mut runs = Vec::new();
    let mut workspaces = Vec::new();
    for prefix in ["one", "two"] {
        let workspace = TempDir::new()?;
        let mut calls = Vec::new();
        for suffix in ["a", "b", "c"] {
            let path = format!("{prefix}-{suffix}");
            std::fs::write(workspace.path().join(&path), "text")?;
            calls.push(call(&path, "read", serde_json::json!({"path":path})));
        }
        let mut runner = agent(
            &workspace,
            vec![turn(calls), turn(vec![text("done")])],
            |_| {},
        )?;
        runner.tools.set_read_gate(Arc::clone(&gate));
        let runner = runner.with_tool_workers(Arc::clone(&workers), 3);
        runs.push(tokio::spawn(async move {
            runner.run("inspect", CancellationToken::new(), None).await
        }));
        workspaces.push(workspace);
    }
    wait_for_reads(&gate, 2).await?;
    assert_eq!(gate.entered()?.0.len(), 2);
    assert_eq!(workers.available_permits(), 0);
    gate.allow(None)?;
    for run in runs {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), run)
                .await??
                .status,
            RunStatus::Completed
        );
    }
    assert_eq!(gate.entered()?.0.len(), 6);
    assert_eq!(gate.entered()?.1, 2);
    assert_eq!(workers.available_permits(), 2);
    Ok(())
}

#[tokio::test]
async fn stopped_read_batches_join_workers_before_returning()
-> Result<(), Box<dyn std::error::Error>> {
    for fail_commit in [false, true] {
        let workspace = TempDir::new()?;
        for path in ["a", "b", "c"] {
            std::fs::write(workspace.path().join(path), "text")?;
        }
        let mut runner = agent(
            &workspace,
            vec![
                turn(
                    ["a", "b", "c"]
                        .into_iter()
                        .map(|path| call(path, "read", serde_json::json!({"path":path})))
                        .collect(),
                ),
                turn(vec![text("done")]),
            ],
            |_| {},
        )?;
        let gate = Arc::new(crate::tools::ReadGate::default());
        let _release = ReleaseReads(Arc::clone(&gate));
        runner.tools.set_read_gate(Arc::clone(&gate));
        let workers = Arc::new(Semaphore::new(16));
        let runner = runner.with_tool_workers(Arc::clone(&workers), 2);
        let cancellation = CancellationToken::new();
        let control = cancellation.clone();
        let (commits, mut requests) = mpsc::channel::<CommitRequest>(1);
        let (failure, notification) = oneshot::channel();
        let owner = tokio::spawn(async move {
            let mut failed = false;
            let mut failure = Some(failure);
            while let Some(request) = requests.recv().await {
                if fail_commit
                    && request
                        .records
                        .iter()
                        .any(|record| matches!(record, ExecutionRecord::ToolResult { .. }))
                {
                    failed = true;
                    if let Some(failure) = failure.take() {
                        let _ = failure.send(());
                    }
                }
                let _ = request.response.send(if failed {
                    Err("lost result commit".into())
                } else {
                    Ok(())
                });
            }
        });
        let run = tokio::spawn(async move {
            runner
                .run_with_approvals("inspect", cancellation, None, None, Some(commits), None)
                .await
        });
        wait_for_reads(&gate, 2).await?;
        if fail_commit {
            gate.allow(Some("a"))?;
            tokio::time::timeout(Duration::from_secs(5), notification).await??;
        } else {
            control.cancel();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!run.is_finished());
        assert_eq!(gate.entered()?.0.len(), 2);
        gate.allow(None)?;
        let report = tokio::time::timeout(Duration::from_secs(5), run).await??;
        owner.await?;
        assert_eq!(
            report.status,
            if fail_commit {
                RunStatus::Failed
            } else {
                RunStatus::Cancelled
            }
        );
        assert_eq!(gate.entered()?.0.len(), 2);
        assert_eq!(workers.available_permits(), 16);
        if !fail_commit {
            assert_eq!(report.tool_calls, 3);
            assert_eq!(report.messages.len(), 5);
        }
    }
    Ok(())
}

#[tokio::test]
async fn a_read_error_stops_new_calls_and_settles_after_existing_workers()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    std::fs::write(workspace.path().join("a"), "text")?;
    std::fs::write(workspace.path().join("later"), "text")?;
    let mut runner = agent(
        &workspace,
        vec![
            turn(
                ["a", "missing", "later"]
                    .into_iter()
                    .map(|path| call(path, "read", serde_json::json!({"path":path})))
                    .collect(),
            ),
            turn(vec![text("handled the read error")]),
        ],
        |_| {},
    )?;
    let gate = Arc::new(crate::tools::ReadGate::default());
    let _release = ReleaseReads(Arc::clone(&gate));
    runner.tools.set_read_gate(Arc::clone(&gate));
    let runner = runner.with_tool_workers(Arc::new(Semaphore::new(16)), 2);
    let (events, mut updates) = mpsc::channel(128);
    let run = tokio::spawn(async move {
        runner
            .run("inspect", CancellationToken::new(), Some(events))
            .await
    });
    wait_for_reads(&gate, 2).await?;
    gate.allow(Some("missing"))?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = updates.recv().await {
            if matches!(event, RunEvent::ToolFinished { output, .. } if output.is_error()) {
                break;
            }
        }
    })
    .await?;
    assert!(!run.is_finished());
    assert_eq!(gate.entered()?.0.len(), 2);
    gate.allow(Some("a"))?;
    let report = tokio::time::timeout(Duration::from_secs(5), run).await??;
    assert_eq!(report.status, RunStatus::Completed);
    assert_eq!(report.steps, 2);
    assert_eq!(gate.entered()?.0.len(), 2);
    assert!(report.messages.iter().flat_map(|message| &message.content).any(|part| matches!(part, Content::ToolResult { call_id, output: ToolResultOutput::ErrorJson { value }, .. } if call_id == "later" && value.get("execution_status").and_then(serde_json::Value::as_str) == Some("not_executed"))));
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn active_duration_stops_and_joins_an_exclusive_command()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let runner = agent(
        &workspace,
        vec![turn(vec![
            call(
                "slow",
                "shell",
                serde_json::json!({"command":"touch started; sleep 30; touch leaked"}),
            ),
            call(
                "later",
                "write",
                serde_json::json!({"path":"later", "content":"text"}),
            ),
        ])],
        // Resource discovery and process startup count toward active time too.
        // Leave enough room to reach the running-command boundary on CI.
        |config| config.max_duration = Duration::from_secs(5),
    )?;
    let report = tokio::time::timeout(
        Duration::from_secs(15),
        runner.run("check", CancellationToken::new(), None),
    )
    .await?;
    assert!(workspace.path().join("started").exists());
    assert!(report.unknown_effect);
    assert_eq!(report.status, RunStatus::Failed);
    assert!(!workspace.path().join("leaked").exists());
    assert!(!workspace.path().join("later").exists());
    assert_eq!(report.tool_calls, 2);
    assert_eq!(report.messages.len(), 4);
    Ok(())
}

#[tokio::test]
async fn cancelled_stream_commits_partial_item_without_context_admission()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let agent = agent(
        &workspace,
        vec![turn((0..20).map(|_| text("partial ")).collect())],
        |_| {},
    )?;
    let cancellation = CancellationToken::new();
    let control = cancellation.clone();
    let (sender, mut receiver) = mpsc::channel(1);
    let (commits, owner) = commit_recorder();
    let run = tokio::spawn(async move {
        agent
            .run_with_approvals(
                "cancel",
                cancellation,
                Some(sender),
                None,
                Some(commits),
                None,
            )
            .await
    });
    let mut streamed_item = None;
    while let Some(event) = receiver.recv().await {
        if let RunEvent::AssistantDelta { item_id, .. } = event {
            streamed_item = Some(item_id);
            control.cancel();
        }
    }
    let report = run.await?;
    let records = owner.await?;
    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(report.messages.len(), 1);
    assert!(records.iter().any(|record| matches!(record, ExecutionRecord::ModelInterrupted { item_id, partial, .. } if Some(item_id) == streamed_item.as_ref() && !partial.content.is_empty())));
    assert!(!records.iter().any(|record| matches!(
        record,
        ExecutionRecord::ModelResponse { .. } | ExecutionRecord::ToolIntent { .. }
    )));
    assert_eq!(
        report
            .events
            .iter()
            .filter(|event| matches!(event, RunEvent::AssistantInterrupted { .. }))
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn steering_drains_inflight_read_workers_without_cancelling_them_before_the_next_model_step()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    std::fs::write(workspace.path().join("one.txt"), "one")?;
    std::fs::write(workspace.path().join("two.txt"), "two")?;
    let mut runner = agent(
        &workspace,
        vec![
            turn(vec![
                call("one", "read", serde_json::json!({"path":"one.txt"})),
                call("two", "read", serde_json::json!({"path":"two.txt"})),
                call(
                    "stale",
                    "write",
                    serde_json::json!({"path":"stale.txt","content":"stale"}),
                ),
            ]),
            turn(vec![text("done")]),
        ],
        |_| {},
    )?;
    let gate = Arc::new(crate::tools::ReadGate::default());
    let _release = ReleaseReads(Arc::clone(&gate));
    runner.tools.set_read_gate(Arc::clone(&gate));
    let fence = Arc::new(crate::control::LaunchFence::default());
    let (commits, recorder) = commit_recorder();
    let owner_commits = commits.clone();
    let (models, mut requests) = mpsc::channel::<ModelBoundary>(1);
    let owner_fence = Arc::clone(&fence);
    let owner = tokio::spawn(async move {
        let mut prompts = Vec::new();
        while let Some(mut request) = requests.recv().await {
            if owner_fence.pending() {
                request
                    .prompt
                    .messages
                    .push(Message::text(Role::User, "shared correction"));
                request.context_version += 1;
            }
            let result = commit_execution(
                &Some(owner_commits.clone()),
                vec![ExecutionRecord::ModelRequest {
                    step_id: request.step_id,
                    item_id: request.item_id,
                    context_version: request.context_version,
                    prompt: Box::new(request.prompt.clone()),
                }],
            )
            .await;
            owner_fence.set(false);
            prompts.push(request.prompt.clone());
            let _ = request
                .response
                .send(result.map(|()| (request.prompt, request.context_version)));
        }
        prompts
    });
    let control = TurnControl {
        fence: Arc::clone(&fence),
        models,
    };
    let run = tokio::spawn(async move {
        runner
            .run_context(
                RunInput {
                    prompt: "read both".into(),
                    messages: Vec::new(),
                    user_item_id: "user".into(),
                    context_version: 0,
                    checkpoint: None,
                    complete_checkpoint: false,
                    restored_verification: None,
                },
                CancellationToken::new(),
                RunChannels {
                    events: None,
                    approvals: None,
                    commits: Some(commits),
                    control: Some(control),
                },
            )
            .await
    });
    wait_for_reads(&gate, 2).await?;
    fence.set(true);
    assert_eq!(gate.entered()?.0.len(), 2);
    gate.allow(None)?;
    let report = run.await?;
    let requests = owner.await?;
    let facts = recorder.await?;
    assert_eq!(report.status, RunStatus::Completed);
    assert_eq!(requests.len(), 2);
    crate::context::validate_history(&requests[1].messages)?;
    assert!(serde_json::to_string(&requests[1].messages)?.contains("shared correction"));
    assert!(!workspace.path().join("stale.txt").exists());
    assert!(
        facts
            .iter()
            .filter_map(|fact| match fact {
                ExecutionRecord::ToolResult {
                    message, effect, ..
                } if *effect == EffectStatus::Completed => Some(message),
                _ => None,
            })
            .all(|message| !message.content.iter().any(
                |content| matches!(content, Content::ToolResult { output, .. } if output.is_error())
            ))
    );
    assert_eq!(
        facts
            .iter()
            .filter(|fact| matches!(
                fact,
                ExecutionRecord::ToolResult {
                    effect: EffectStatus::Completed,
                    ..
                }
            ))
            .count(),
        2
    );
    Ok(())
}

#[tokio::test]
async fn cancellation_after_response_commit_settles_all_unstarted_calls()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let runner = agent(
        &workspace,
        vec![turn(vec![
            call(
                "one",
                "write",
                serde_json::json!({"path":"one.txt","content":"one"}),
            ),
            call(
                "two",
                "write",
                serde_json::json!({"path":"two.txt","content":"two"}),
            ),
        ])],
        |_| {},
    )?;
    let cancellation = CancellationToken::new();
    let control = cancellation.clone();
    let (commits, mut requests) = mpsc::channel::<CommitRequest>(1);
    let owner = tokio::spawn(async move {
        let mut results = 0;
        while let Some(request) = requests.recv().await {
            if request
                .records
                .iter()
                .any(|record| matches!(record, ExecutionRecord::ModelResponse { .. }))
            {
                control.cancel();
            }
            for record in &request.records {
                if let ExecutionRecord::ToolResult { effect, .. } = record {
                    assert_eq!(*effect, EffectStatus::NotExecuted);
                    results += 1;
                }
            }
            let _ = request.response.send(Ok(()));
        }
        results
    });
    let report = runner
        .run_with_approvals("write both", cancellation, None, None, Some(commits), None)
        .await;
    assert_eq!(report.status, RunStatus::Cancelled);
    assert_eq!(owner.await?, 2);
    context::validate_history(&report.messages)?;
    assert!(!workspace.path().join("one.txt").exists());
    assert!(!workspace.path().join("two.txt").exists());
    Ok(())
}

#[tokio::test]
async fn duplicate_complete_call_ids_reject_the_response_before_any_effect()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let runner = agent(
        &workspace,
        vec![turn(vec![
            call(
                "earlier",
                "write",
                serde_json::json!({"path":"earlier.txt","content":"one"}),
            ),
            call(
                "duplicate",
                "write",
                serde_json::json!({"path":"one.txt","content":"one"}),
            ),
            call(
                "duplicate",
                "write",
                serde_json::json!({"path":"two.txt","content":"two"}),
            ),
        ])],
        |_| {},
    )?;
    let report = runner.run("write", CancellationToken::new(), None).await;
    assert_eq!(report.status, RunStatus::Failed);
    assert!(report.detail.contains("duplicate"));
    assert!(!workspace.path().join("earlier.txt").exists());
    assert!(!workspace.path().join("one.txt").exists());
    assert!(!workspace.path().join("two.txt").exists());
    Ok(())
}
