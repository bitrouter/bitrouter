//! Production SQLite persistence with real MCP HTTP, using scripted model output.
use bitrouter_ai::types::AuthScheme;
use bitrouter_ai::types::{ApiProtocol, FinishReason, StreamPart, Usage};
use bitrouter_orchestrator::agent::AgentConfig;
use bitrouter_orchestrator::harness::HarnessConfig;
use bitrouter_orchestrator::service::ThreadService;
use bitrouter_orchestrator::store::{ExecutionRecord, ExecutionStore};
use bitrouter_orchestrator::thread::{PermissionProfile, ThreadRequest, ThreadTarget};
use bitrouter_orchestrator::turn::{ApprovalAnswer, TurnRequest, TurnSnapshot, TurnStatus};
use bitrouter_sdk::language_model::{
    MockExecutor, MockResponse, RoutingTarget, StaticRoutingTable,
};
use bitrouter_sdk::{App, caller::CallerContext};
use rmcp::model::ProtocolVersion;
use sea_orm::{ConnectionTrait, Statement};
use std::sync::Arc;
use std::time::Duration;

#[path = "../../../crates/bitrouter-orchestrator/tests/harness_resources/mcp_fixture.rs"]
mod fixture;

fn app(executor: MockExecutor) -> Result<Arc<App>, Box<dyn std::error::Error>> {
    let routing = StaticRoutingTable::new();
    routing.insert(
        "fixture-model",
        vec![RoutingTarget {
            provider_name: "fixture".into(),
            service_id: "fixture-model".into(),
            api_base: "https://example.invalid".into(),
            api_key: "fixture-key".into(),
            api_protocol: ApiProtocol::ChatCompletions,
            chat_token_limit_field: None,
            chat_supports_store: None,
            chat_supports_stream_options: None,
            chat_google_extensions: false,
            reasoning_effort: None,
            model_constraints: Default::default(),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: AuthScheme::Bearer,
            headers: Vec::new(),
        }],
    );
    Ok(Arc::new(
        App::builder()
            .language_model(|builder| {
                builder
                    .routing_table(Arc::new(routing))
                    .executor(Arc::new(executor));
            })
            .build()?,
    ))
}

async fn wait(
    service: &ThreadService,
    target: &ThreadTarget,
    turn_id: &str,
    wanted: TurnStatus,
) -> Result<TurnSnapshot, Box<dyn std::error::Error>> {
    Ok(tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let view = service.read_turn(target, &CallerContext::local(), turn_id)?;
            if view.status == wanted || view.status.terminal() {
                return Ok::<_, bitrouter_orchestrator::service::ServiceError>(view);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await??)
}

#[tokio::test]
async fn sqlite_reopen_retains_resource_inventory_and_known_result_without_connections()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    std::fs::write(workspace.join("AGENTS.md"), "SQLITE_STARTUP_INSTRUCTIONS")?;
    let skill = workspace.join(".codex/skills/demo");
    std::fs::create_dir_all(&skill)?;
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: demo\ndescription: Discover this skill\n---\nORIGINAL_BODY",
    )?;
    let upstream = fixture::server(false).await;
    let configuration = HarnessConfig {
        servers: vec![fixture::configuration(&upstream)],
        protocol: ProtocolVersion::LATEST,
        skill_roots: Vec::new(),
        instructions: Default::default(),
    };
    let url = format!("sqlite://{}/runtime.db", directory.path().display());
    let db = bitrouter::db::connect(&url).await?;
    bitrouter::db::run_migrations(&db).await?;
    let store = Arc::new(bitrouter::agent_store::DatabaseExecutionStore::new(
        db.clone(),
    ));
    let model = MockExecutor::new(vec![
        MockResponse::Stream(vec![
            StreamPart::ToolCallDelta {
                id: "echo".into(),
                name: Some("fixture__echo".into()),
                arguments: "{\"text\":\"sqlite-evidence\"}".into(),
                provider_metadata: Default::default(),
            },
            StreamPart::Usage {
                usage: Usage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    ..Default::default()
                },
            },
            StreamPart::Finish {
                reason: FinishReason::ToolCalls,
            },
        ]),
        MockResponse::Stream(vec![
            StreamPart::TextDelta {
                text: "done".into(),
            },
            StreamPart::Usage {
                usage: Usage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    ..Default::default()
                },
            },
            StreamPart::Finish {
                reason: FinishReason::Stop,
            },
        ]),
    ]);
    let service =
        ThreadService::with_store(app(model)?, std::slice::from_ref(&workspace), store.clone())?
            .with_resources(configuration.clone())?;
    let caller = CallerContext::local();
    let created = service
        .create_thread(
            &service.capabilities().server_instance_id,
            ThreadRequest {
                caller: caller.clone(),
                workspace: workspace.clone(),
                config: AgentConfig::fixed("fixture-model", None),
                permission_profile: PermissionProfile::Ask,
                verification_command: None,
                idempotency_key: "create".into(),
            },
        )
        .await?;
    let target = ThreadTarget {
        thread_id: created.thread_id.clone(),
        server_instance_id: created.server_instance_id.clone(),
    };
    // A version-2 root with only baseline Thread-created facts remains readable;
    // the next append upgrades its envelope in the same transaction.
    db.execute(Statement::from_sql_and_values(
        db.get_database_backend(),
        "UPDATE bro_executions SET format_version=2 WHERE id=?",
        [created.thread_id.clone().into()],
    ))
    .await?;
    assert_eq!(
        store
            .load(&created.thread_id)
            .await?
            .ok_or("legacy root")?
            .format_version,
        2
    );
    assert!(
        upstream
            .received_requests()
            .await
            .ok_or("requests")?
            .is_empty()
    );
    let turn = service
        .start_turn(
            &target,
            &caller,
            TurnRequest {
                prompt: "echo".into(),
                idempotency_key: "first".into(),
            },
        )
        .await?;
    let waiting = wait(
        &service,
        &target,
        &turn.turn_id,
        TurnStatus::WaitingForInput,
    )
    .await?;
    service
        .answer_thread_input(
            &target,
            &caller,
            ApprovalAnswer {
                turn_id: turn.turn_id.clone(),
                request_id: waiting.pending_input_id.ok_or("approval")?,
                approved: true,
                idempotency_key: "approve".into(),
            },
        )
        .await?;
    let completed = wait(&service, &target, &turn.turn_id, TurnStatus::Completed).await?;
    assert_eq!(completed.status, TurnStatus::Completed);
    let inventory = completed.resources.ok_or("inventory")?;
    service.shutdown().await;
    let journal = store.load(&created.thread_id).await?.ok_or("journal")?;
    assert_eq!(journal.format_version, 6);
    assert!(serde_json::to_string(&journal.records)?.contains("sqlite-evidence"));
    assert!(journal.records.iter().any(|record| matches!(
        record, ExecutionRecord::TurnRecord { fact, .. }
            if matches!(fact.as_ref(), ExecutionRecord::InstructionContext { snapshot, .. }
                if snapshot.body == "SQLITE_STARTUP_INSTRUCTIONS")
    )));
    assert!(journal.records.iter().any(|record| matches!(record,ExecutionRecord::TurnRecord { fact,.. } if matches!(fact.as_ref(),ExecutionRecord::HarnessInventory {..}))));
    drop(service);
    drop(store);
    db.close().await?;
    let before = upstream.received_requests().await.ok_or("requests")?.len();
    // Opening committed history uses its original inventory even after disk changes.
    std::fs::remove_file(workspace.join("AGENTS.md"))?;
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: demo\ndescription: Changed metadata\n---\nCHANGED_BODY",
    )?;
    let reopened = bitrouter::db::connect(&url).await?;
    let destination = ThreadService::with_store(
        app(MockExecutor::new(vec![]))?,
        &[workspace],
        Arc::new(bitrouter::agent_store::DatabaseExecutionStore::new(
            reopened.clone(),
        )),
    )?
    .with_resources(configuration)?;
    let target = ThreadTarget {
        server_instance_id: destination.capabilities().server_instance_id,
        ..target
    };
    let loaded = destination.load_thread(&target, &caller).await?;
    assert_eq!(
        loaded
            .latest_turn
            .as_ref()
            .ok_or("turn")?
            .resources
            .as_ref(),
        Some(&inventory)
    );
    assert_eq!(
        upstream.received_requests().await.ok_or("requests")?.len(),
        before
    );
    destination.shutdown().await;
    drop(destination);
    reopened.close().await?;
    Ok(())
}
