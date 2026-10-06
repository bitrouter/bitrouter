//! Real native workspace tools and SQLite authority, with scripted model turns.
use std::sync::Arc;
use std::time::Duration;

use bitrouter::managed_store::DatabaseNativeStore;
use bitrouter_orchestrator::agent::ToolMode;
use bitrouter_orchestrator::core::protocol::{CoreError, TaskInput, ToolExecute, ToolOutcome};
use bitrouter_orchestrator::core::session::RunStatus;
use bitrouter_orchestrator::harness::{
    HarnessConfig,
    managed::{
        session::{NativeApproval, NativeSession},
        store::NativeStore,
    },
};
use bitrouter_sdk::language_model::types::AuthScheme;
use bitrouter_sdk::language_model::{
    ApiProtocol, Content, FinishReason, GenerateResult, MockExecutor, MockResponse, RoutingTarget,
    StaticRoutingTable, Usage,
};
use bitrouter_sdk::{App, caller::CallerContext};
use serde_json::json;
use tokio_util::sync::CancellationToken;

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Approval(bool);
#[async_trait::async_trait]
impl NativeApproval for Approval {
    async fn approve(&self, _: &ToolExecute) -> Result<bool, CoreError> {
        Ok(self.0)
    }
}

fn output(content: Content) -> MockResponse {
    let calls = matches!(content, Content::ToolCall { .. });
    MockResponse::Generate(GenerateResult {
        content: vec![content],
        usage: Some(Usage {
            prompt_tokens: 12,
            completion_tokens: 5,
            ..Default::default()
        }),
        finish_reason: Some(if calls {
            FinishReason::ToolCalls
        } else {
            FinishReason::Stop
        }),
        response_id: None,
        stop_details: None,
        provider_metadata: Default::default(),
    })
}
fn text(value: &str) -> Content {
    Content::Text {
        text: value.into(),
        provider_metadata: Default::default(),
    }
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
fn app(content: Vec<Content>) -> Result<Arc<App>, Box<dyn std::error::Error>> {
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
            reasoning_effort: None,
            model_constraints: Default::default(),
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: AuthScheme::Bearer,
            headers: vec![],
        }],
    );
    let executor = MockExecutor::new(content.into_iter().map(output).collect());
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
fn input() -> TaskInput {
    TaskInput {
        text: "Use native tools to complete this task".into(),
        model: "fixture-model".into(),
        effort: None,
        max_output_tokens: Some(128),
        routing: Default::default(),
        max_concurrent_subagents: None,
        discardable_history: None,
        acceptance_criteria: vec![],
        required_materials: vec![],
        verification: None,
        limits: None,
    }
}

#[tokio::test]
async fn sqlite_native_core_writes_reads_releases_and_continues() -> TestResult {
    let dir = tempfile::tempdir()?;
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    std::fs::write(
        workspace.join("AGENTS.md"),
        "Keep native instructions in context.",
    )?;
    let url = format!("sqlite://{}/managed.db", dir.path().display());
    let db = bitrouter::db::connect(&url).await?;
    bitrouter::db::run_migrations(&db).await?;
    let store = Arc::new(DatabaseNativeStore::new(db.clone(), "integration".into())?);
    let model = app(vec![
        call(
            "write",
            "write",
            json!({"path":"result.txt","content":"native core result"}),
        ),
        call("read", "read", json!({"path":"result.txt"})),
        text("done"),
    ])?;
    let mut session = NativeSession::open(
        model,
        CallerContext::local(),
        store.clone(),
        &workspace,
        ToolMode::Coding,
        &HarnessConfig::default(),
    )
    .await?;
    let snapshot = tokio::time::timeout(
        Duration::from_secs(20),
        session.run(input(), Arc::new(Approval(true)), CancellationToken::new()),
    )
    .await??;
    assert_eq!(
        snapshot.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed)
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("result.txt"))?,
        "native core result"
    );
    let root = snapshot.root_turn().ok_or("missing root turn")?;
    assert_eq!(root.invocations.len(), 2);
    assert!(root.invocations.iter().all(|invocation| {
        invocation
            .result
            .as_ref()
            .is_some_and(|result| result.status == ToolOutcome::Succeeded)
    }));
    session.close().await?;
    let (_, bytes) = store.load().await?.ok_or("missing native journal")?;
    let journal: serde_json::Value = serde_json::from_slice(&bytes)?;
    assert_eq!(journal["released"], true);
    assert_eq!(
        journal["starts"].as_object().ok_or("missing starts")?.len(),
        2
    );
    let next_db = bitrouter::db::connect(&url).await?;
    let next_store = Arc::new(DatabaseNativeStore::new(next_db, "integration".into())?);
    let mut continued = NativeSession::open(
        app(vec![
            call("again", "read", json!({"path":"result.txt"})),
            text("continued"),
        ])?,
        CallerContext::local(),
        next_store,
        &workspace,
        ToolMode::Coding,
        &HarnessConfig::default(),
    )
    .await?;
    assert_eq!(
        continued.core().snapshot().await.session_id,
        snapshot.session_id
    );
    let next = tokio::time::timeout(
        Duration::from_secs(20),
        continued.run(input(), Arc::new(Approval(true)), CancellationToken::new()),
    )
    .await??;
    assert_eq!(
        next.run
            .as_ref()
            .and_then(|run| run.final_answer.as_deref()),
        Some("continued")
    );
    assert!(
        next.agents[&next.agent_id].history.len()
            > snapshot.agents[&snapshot.agent_id].history.len()
    );
    continued.close().await?;
    Ok(())
}

#[tokio::test]
async fn native_denied_write_has_no_effect_and_workspace_has_one_owner() -> TestResult {
    let dir = tempfile::tempdir()?;
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let db =
        bitrouter::db::connect(&format!("sqlite://{}/managed.db", dir.path().display())).await?;
    bitrouter::db::run_migrations(&db).await?;
    let store = Arc::new(DatabaseNativeStore::new(db.clone(), "denial".into())?);
    let mut session = NativeSession::open(
        app(vec![
            call(
                "write",
                "write",
                json!({"path":"denied.txt","content":"must not exist"}),
            ),
            text("denied"),
        ])?,
        CallerContext::local(),
        store.clone(),
        &workspace,
        ToolMode::Coding,
        &HarnessConfig::default(),
    )
    .await?;
    let competing = Arc::new(DatabaseNativeStore::new(db, "competing".into())?);
    assert!(
        NativeSession::open(
            app(vec![])?,
            CallerContext::local(),
            competing,
            &workspace,
            ToolMode::Coding,
            &HarnessConfig::default()
        )
        .await
        .is_err()
    );
    let snapshot = tokio::time::timeout(
        Duration::from_secs(20),
        session.run(input(), Arc::new(Approval(false)), CancellationToken::new()),
    )
    .await??;
    assert!(!workspace.join("denied.txt").exists());
    assert_eq!(
        snapshot
            .root_turn()
            .and_then(|turn| turn.invocations.first())
            .and_then(|call| call.result.as_ref())
            .map(|result| result.status),
        Some(ToolOutcome::Denied)
    );
    session.close().await?;
    let (_, bytes) = store.load().await?.ok_or("missing denial journal")?;
    let journal: serde_json::Value = serde_json::from_slice(&bytes)?;
    assert!(
        journal["starts"]
            .as_object()
            .ok_or("missing starts")?
            .values()
            .all(|start| start["started"] == false)
    );
    Ok(())
}

struct PendingApproval(tokio::sync::Notify);
#[async_trait::async_trait]
impl NativeApproval for PendingApproval {
    async fn approve(&self, _: &ToolExecute) -> Result<bool, CoreError> {
        self.0.notify_one();
        std::future::pending().await
    }
}

#[tokio::test]
async fn native_cancel_pending_approval_fences_the_write() -> TestResult {
    let dir = tempfile::tempdir()?;
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let db =
        bitrouter::db::connect(&format!("sqlite://{}/managed.db", dir.path().display())).await?;
    bitrouter::db::run_migrations(&db).await?;
    let store = Arc::new(DatabaseNativeStore::new(db, "cancel".into())?);
    let mut session = NativeSession::open(
        app(vec![call(
            "write",
            "write",
            json!({"path":"cancelled.txt","content":"must not exist"}),
        )])?,
        CallerContext::local(),
        store.clone(),
        &workspace,
        ToolMode::Coding,
        &HarnessConfig::default(),
    )
    .await?;
    let approval = Arc::new(PendingApproval(tokio::sync::Notify::new()));
    let cancel = CancellationToken::new();
    let run = session.run(input(), approval.clone(), cancel.clone());
    let trigger = async {
        approval.0.notified().await;
        cancel.cancel();
    };
    let (snapshot, ()) = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(run, trigger)
    })
    .await?;
    let snapshot = snapshot?;
    assert_eq!(
        snapshot.run.as_ref().map(|run| run.status),
        Some(RunStatus::Cancelled)
    );
    assert!(!workspace.join("cancelled.txt").exists());
    session.close().await?;
    let (_, bytes) = store.load().await?.ok_or("missing cancellation journal")?;
    let journal: serde_json::Value = serde_json::from_slice(&bytes)?;
    assert_eq!(journal["released"], true);
    assert!(
        !journal["fences"]
            .as_array()
            .ok_or("missing fences")?
            .is_empty()
    );
    assert!(
        journal["starts"]
            .as_object()
            .ok_or("missing starts")?
            .values()
            .all(|start| start["started"] == false)
    );
    Ok(())
}

#[tokio::test]
async fn sqlite_native_store_rejects_stale_ownership_revision() -> TestResult {
    let db = bitrouter::db::connect("sqlite::memory:").await?;
    bitrouter::db::run_migrations(&db).await?;
    let first = DatabaseNativeStore::new(db.clone(), "cas".into())?;
    let stale = DatabaseNativeStore::new(db, "cas".into())?;
    first.save(0, b"original".to_vec()).await?;
    assert!(stale.save(0, b"duplicate".to_vec()).await.is_err());
    first.save(1, b"winner".to_vec()).await?;
    assert!(stale.save(1, b"stale".to_vec()).await.is_err());
    assert_eq!(first.load().await?, Some((2, b"winner".to_vec())));
    Ok(())
}

struct DelayedCommitStore {
    inner: DatabaseNativeStore,
    entered: tokio::sync::Notify,
    delayed: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl NativeStore for DelayedCommitStore {
    async fn load(&self) -> Result<Option<(u64, Vec<u8>)>, CoreError> {
        self.inner.load().await
    }
    async fn save(&self, revision: u64, bytes: Vec<u8>) -> Result<(), CoreError> {
        use base64::Engine;
        // Pause a direct scheduler commit, while it holds the core commit lock.
        let journal: serde_json::Value = serde_json::from_slice(&bytes).map_err(|err| {
            CoreError::rejected(
                bitrouter_orchestrator::core::protocol::ErrorCode::CheckpointUnavailable,
                err.to_string(),
            )
        })?;
        let completed = journal["checkpoint"]["payload_bytes"]
            .as_str()
            .and_then(|encoded| {
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .ok()
            })
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|payload| {
                payload["events"].as_array().is_some_and(|events| {
                    events
                        .iter()
                        .any(|event| event["type"] == "agent.completed")
                })
            });
        if completed && !self.delayed.swap(true, std::sync::atomic::Ordering::SeqCst) {
            self.entered.notify_one();
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        self.inner.save(revision, bytes).await
    }
}

#[tokio::test]
async fn native_cancel_while_driver_commits_does_not_deadlock() -> TestResult {
    let dir = tempfile::tempdir()?;
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let db =
        bitrouter::db::connect(&format!("sqlite://{}/managed.db", dir.path().display())).await?;
    bitrouter::db::run_migrations(&db).await?;
    let store = Arc::new(DelayedCommitStore {
        inner: DatabaseNativeStore::new(db, "delayed".into())?,
        entered: tokio::sync::Notify::new(),
        delayed: std::sync::atomic::AtomicBool::new(false),
    });
    let mut session = NativeSession::open(
        app(vec![text("complete")])?,
        CallerContext::local(),
        store.clone(),
        &workspace,
        ToolMode::Coding,
        &HarnessConfig::default(),
    )
    .await?;
    let cancel = CancellationToken::new();
    let run = session.run(input(), Arc::new(Approval(true)), cancel.clone());
    let trigger = async {
        store.entered.notified().await;
        cancel.cancel();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(run, trigger)
    })
    .await?;
    let snapshot = result?;
    assert!(matches!(
        snapshot.run.as_ref().map(|run| run.status),
        Some(RunStatus::Completed | RunStatus::Cancelled)
    ));
    session.close().await?;
    Ok(())
}
