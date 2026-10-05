//! Resource loading uses the actual native Turn, commit, approval and recovery path.
use super::support::*;
use crate::harness::HarnessConfig;
use crate::service::ThreadService;
use crate::store::{EffectStatus, ExecutionRecord, ExecutionStore, MemoryExecutionStore};
use crate::turn::{CancelTurnRequest, TurnStatus};
use bitrouter_sdk::caller::CallerContext;
use rmcp::model::ProtocolVersion;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, Request, ResponseTemplate};

#[path = "../../../tests/harness_resources/mcp_fixture.rs"]
mod fixture;

fn config(server: &wiremock::MockServer) -> HarnessConfig {
    HarnessConfig {
        servers: vec![fixture::configuration(server)],
        protocol: ProtocolVersion::LATEST,
        skill_roots: Vec::new(),
    }
}

async fn calls(server: &wiremock::MockServer) -> Result<usize, String> {
    Ok(server
        .received_requests()
        .await
        .ok_or("requests unavailable")?
        .iter()
        .filter(|request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .is_ok_and(|value| value["method"] == "tools/call")
        })
        .count())
}

#[tokio::test]
async fn mcp_approval_and_discovery_cross_native_durable_barriers()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let skill = workspace.path().join(".agents/skills/demo");
    std::fs::create_dir_all(&skill)?;
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: demo\ndescription: Test discovery\n---\nSECRET_SKILL_BODY",
    )?;
    let upstream = fixture::server(false).await;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![
            turn(vec![tool_call(
                "echo",
                "fixture__echo",
                json!({"text":"hello"}),
            )]),
            final_turn(),
        ])?,
        &[workspace.path().into()],
        store.clone(),
    )?
    .with_resources(config(&upstream))?;
    let created = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "create"),
        )
        .await?;
    let caller = CallerContext::local();
    let target = target(&created);
    // Metadata creation and cold reads are not MCP discovery/child launch authority.
    service.read_thread_view(&target, &caller)?;
    assert!(
        upstream
            .received_requests()
            .await
            .ok_or("requests")?
            .is_empty()
    );
    let accepted = service
        .start_turn(&target, &caller, input("echo", "turn"))
        .await?;
    let waiting = wait_for(&service, &accepted.turn_id, TurnStatus::WaitingForInput).await?;
    assert_eq!(
        waiting.pending_input.as_ref().ok_or("approval")?.tool_name,
        "fixture__echo"
    );
    assert_eq!(calls(&upstream).await?, 0);
    let inventory = waiting.resources.as_ref().ok_or("inventory")?;
    assert_eq!(inventory.skills.len(), 1);
    assert!(!serde_json::to_string(inventory)?.contains("SECRET_SKILL_BODY"));
    let before = store.load(&created.thread_id).await?.ok_or("journal")?;
    let facts: Vec<_> = before.records.iter().map(turn_fact).collect();
    let discovery = facts
        .iter()
        .position(|fact| matches!(fact, ExecutionRecord::HarnessInventory { .. }))
        .ok_or("durable inventory")?;
    let model = facts
        .iter()
        .position(|fact| matches!(fact, ExecutionRecord::ModelRequest { .. }))
        .ok_or("model request")?;
    assert!(discovery < model);
    assert!(
        !facts
            .iter()
            .any(|fact| matches!(fact, ExecutionRecord::ToolIntent { .. }))
    );
    service
        .answer_input(
            &accepted.turn_id,
            waiting.pending_input_id.as_deref().ok_or("approval id")?,
            true,
        )
        .await?;
    let completed = wait_for(&service, &accepted.turn_id, TurnStatus::Completed).await?;
    assert_eq!(completed.status, TurnStatus::Completed);
    assert_eq!(calls(&upstream).await?, 1);
    let requests = prompts(store.as_ref(), &created.thread_id, &accepted.turn_id).await?;
    assert_eq!(requests.len(), 2);
    let first = serde_json::to_string(&requests[0])?;
    assert!(first.contains("fixture__echo"));
    assert!(!first.contains("SECRET_SKILL_BODY"));
    assert!(!first.contains("Use echo with text."));
    let after = store.load(&created.thread_id).await?.ok_or("journal")?;
    assert!(after.records.iter().map(turn_fact).any(|fact| matches!(
        fact,
        ExecutionRecord::ToolResult {
            effect: EffectStatus::Completed,
            ..
        }
    )));
    // Acceptance replay observes the retained Turn rather than executing again.
    assert_eq!(
        service
            .start_turn(&target, &caller, input("echo", "turn"))
            .await?
            .turn_id,
        accepted.turn_id
    );
    assert_eq!(calls(&upstream).await?, 1);
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn denied_mcp_call_does_not_reach_upstream() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let upstream = fixture::server(false).await;
    let service = ThreadService::new(
        app(vec![
            turn(vec![tool_call(
                "echo",
                "fixture__echo",
                json!({"text":"denied"}),
            )]),
            final_turn(),
        ])?,
        &[workspace.path().into()],
    )?
    .with_resources(config(&upstream))?;
    let accepted = service.submit_fixture(request(&workspace)).await?;
    let waiting = wait_for(&service, &accepted.turn_id, TurnStatus::WaitingForInput).await?;
    service
        .answer_input(
            &accepted.turn_id,
            waiting.pending_input_id.as_deref().ok_or("approval")?,
            false,
        )
        .await?;
    let result = wait_for(&service, &accepted.turn_id, TurnStatus::Completed).await?;
    assert_eq!(result.status, TurnStatus::Completed);
    assert_eq!(calls(&upstream).await?, 0);
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn read_only_threads_do_not_connect_to_effectful_mcp_servers()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let upstream = fixture::server(false).await;
    let service = ThreadService::new(app(vec![final_turn()])?, &[workspace.path().into()])?
        .with_resources(config(&upstream))?;
    let mut request = request(&workspace);
    request.config = request.config.read_only();
    let accepted = service.submit_fixture(request).await?;
    let result = wait_for(&service, &accepted.turn_id, TurnStatus::Completed).await?;
    assert_eq!(result.status, TurnStatus::Completed);
    assert!(result.resources.ok_or("inventory")?.tools.is_empty());
    assert!(
        upstream
            .received_requests()
            .await
            .ok_or("requests")?
            .is_empty()
    );
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn invalid_catalog_never_reaches_a_model_request() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let upstream = fixture::server(true).await;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![final_turn()])?,
        &[workspace.path().into()],
        store.clone(),
    )?
    .with_resources(config(&upstream))?;
    let accepted = service.submit_fixture(request(&workspace)).await?;
    let result = wait_for(&service, &accepted.turn_id, TurnStatus::Failed).await?;
    assert_eq!(result.status, TurnStatus::Failed);
    assert!(
        prompts(store.as_ref(), &accepted.thread_id, &accepted.turn_id)
            .await?
            .is_empty()
    );
    assert_eq!(calls(&upstream).await?, 0);
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn cancellation_after_observed_mcp_dispatch_blocks_replay()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let upstream = fixture::server(false).await;
    let sent = Arc::new(tokio::sync::Semaphore::new(0));
    let observed = sent.clone();
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(|request: &Request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .is_ok_and(|v| v["method"] == "tools/call")
        })
        .respond_with(move |request: &Request| {
            observed.add_permits(1);
            let body =
                serde_json::from_slice::<serde_json::Value>(&request.body).unwrap_or_default();
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(20))
                .set_body_json(json!({"jsonrpc":"2.0","id":body["id"],"result":{"content":[]}}))
        })
        .with_priority(1)
        .mount(&upstream)
        .await;
    let service = ThreadService::new(
        app(vec![
            turn(vec![tool_call(
                "echo",
                "fixture__echo",
                json!({"text":"cancel"}),
            )]),
            final_turn(),
        ])?,
        &[workspace.path().into()],
    )?
    .with_resources(config(&upstream))?;
    let caller = CallerContext::local();
    let created = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "create"),
        )
        .await?;
    let target = target(&created);
    let accepted = service
        .start_turn(&target, &caller, input("cancel", "turn"))
        .await?;
    let waiting = wait_for(&service, &accepted.turn_id, TurnStatus::WaitingForInput).await?;
    service
        .answer_input(
            &accepted.turn_id,
            waiting.pending_input_id.as_deref().ok_or("approval")?,
            true,
        )
        .await?;
    let permit = tokio::time::timeout(Duration::from_secs(3), sent.acquire()).await??;
    permit.forget();
    service
        .cancel_turn(
            &target,
            &caller,
            CancelTurnRequest {
                turn_id: accepted.turn_id.clone(),
                idempotency_key: "cancel".into(),
            },
        )
        .await?;
    let result = wait_for(&service, &accepted.turn_id, TurnStatus::RecoveryRequired).await?;
    assert_eq!(result.status, TurnStatus::RecoveryRequired);
    assert!(result.unknown_effect);
    assert_eq!(calls(&upstream).await?, 1);
    service.shutdown().await;
    Ok(())
}

#[cfg(unix)]
fn stdio_config() -> HarnessConfig {
    use bitrouter_sdk::mcp::transport::{McpServerConfig, McpTransport};
    HarnessConfig {
        servers: vec![McpServerConfig::with_defaults(
            "stdio",
            McpTransport::Stdio {
                command: "python3".into(),
                args: vec![format!(
                    "{}/tests/fixtures/harness_mcp.py",
                    env!("CARGO_MANIFEST_DIR")
                )],
                env: Default::default(),
            },
        )],
        protocol: ProtocolVersion::LATEST,
        skill_roots: Vec::new(),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn stdio_turn_binds_workspace_and_joins_process_before_settlement()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let mut config = stdio_config();
    if let bitrouter_sdk::mcp::transport::McpTransport::Stdio { args, .. } =
        &mut config.servers[0].transport
    {
        args.push("descendant".into());
    }
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![
            turn(vec![tool_call("echo", "stdio__echo", json!({}))]),
            final_turn(),
        ])?,
        &[workspace.path().into()],
        store.clone(),
    )?
    .with_resources(config)?;
    let accepted = service.submit_fixture(request(&workspace)).await?;
    let waiting = wait_for(&service, &accepted.turn_id, TurnStatus::WaitingForInput).await?;
    service
        .answer_input(
            &accepted.turn_id,
            waiting.pending_input_id.as_deref().ok_or("approval")?,
            true,
        )
        .await?;
    let result = wait_for(&service, &accepted.turn_id, TurnStatus::Completed).await?;
    assert_eq!(result.status, TurnStatus::Completed);
    let journal = store.load(&accepted.thread_id).await?.ok_or("journal")?;
    let output = journal
        .records
        .iter()
        .map(turn_fact)
        .find_map(|fact| match fact {
            ExecutionRecord::ToolResult {
                message,
                effect: EffectStatus::Completed,
                ..
            } => Some(message),
            _ => None,
        })
        .ok_or("tool result")?;
    assert!(
        serde_json::to_string(output)?
            .contains(&workspace.path().canonicalize()?.display().to_string())
    );
    for file in ["mcp.pid", "mcp-child.pid"] {
        let pid = std::fs::read_to_string(workspace.path().join(file))?.parse::<u32>()?;
        let status = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        assert!(
            !status.success(),
            "MCP process {file} is still alive after a completed Turn"
        );
    }
    service.shutdown().await;
    assert!(
        store
            .read_owner(&service.inner.instance_id)
            .await?
            .ok_or("owner")?
            .stopped_at_ms
            .is_some()
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn cancelled_handshake_joins_process_before_stopped_owner()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let mut config = stdio_config();
    if let bitrouter_sdk::mcp::transport::McpTransport::Stdio { args, .. } =
        &mut config.servers[0].transport
    {
        args.push("hang".into());
    }
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![final_turn()])?,
        &[workspace.path().into()],
        store.clone(),
    )?
    .with_resources(config)?;
    let created = service
        .create_thread(
            &service.inner.instance_id,
            thread_request(&workspace, "create"),
        )
        .await?;
    let caller = CallerContext::local();
    let target = target(&created);
    let accepted = service
        .start_turn(&target, &caller, input("cancel before model", "turn"))
        .await?;
    tokio::time::timeout(Duration::from_secs(3), async {
        while !workspace.path().join("mcp.pid").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    service
        .cancel_turn(
            &target,
            &caller,
            CancelTurnRequest {
                turn_id: accepted.turn_id.clone(),
                idempotency_key: "cancel-handshake".into(),
            },
        )
        .await?;
    let result = wait_for(&service, &accepted.turn_id, TurnStatus::Cancelled)
        .await
        .map_err(|error| format!("{error}; snapshot={:?}", service.read(&accepted.turn_id)))?;
    assert_eq!(result.status, TurnStatus::Cancelled);
    assert!(
        prompts(store.as_ref(), &created.thread_id, &accepted.turn_id)
            .await?
            .is_empty()
    );
    service.shutdown().await;
    assert!(
        store
            .read_owner(&service.inner.instance_id)
            .await?
            .ok_or("owner")?
            .stopped_at_ms
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn checkpoint_continuation_reuses_results_and_refuses_changed_resource_binding()
-> Result<(), Box<dyn std::error::Error>> {
    for changed in [false, true] {
        let workspace = TempDir::new()?;
        let upstream = fixture::server(false).await;
        let source_store = Arc::new(MemoryExecutionStore::default());
        let source = ThreadService::with_store(
            app(vec![
                turn(vec![tool_call(
                    "echo",
                    "fixture__echo",
                    json!({"text":"retained"}),
                )]),
                final_turn(),
            ])?,
            &[workspace.path().into()],
            source_store.clone(),
        )?
        .with_resources(config(&upstream))?;
        let accepted = source.submit_fixture(request(&workspace)).await?;
        let waiting = wait_for(&source, &accepted.turn_id, TurnStatus::WaitingForInput).await?;
        source
            .answer_input(
                &accepted.turn_id,
                waiting.pending_input_id.as_deref().ok_or("approval")?,
                true,
            )
            .await?;
        assert_eq!(
            wait_for(&source, &accepted.turn_id, TurnStatus::Completed)
                .await?
                .status,
            TurnStatus::Completed
        );
        source.shutdown().await;
        let saved = source_store
            .load(&accepted.thread_id)
            .await?
            .ok_or("journal")?;
        let cutoff = saved
            .records
            .iter()
            .position(|record| {
                matches!(
                    turn_fact(record),
                    ExecutionRecord::RunCheckpoint {
                        model_steps: 1,
                        tool_calls: 1,
                        ..
                    }
                )
            })
            .ok_or("between-step checkpoint")?
            + 1;
        // A stopped, joined source and a precise prefix isolate continuation;
        // this is not evidence that an abruptly lost owner can be retired.
        let memory = Arc::new(MemoryExecutionStore::default());
        let crate::store::OwnerClaim::Acquired { owner } =
            memory.claim_owner(&source.inner.instance_id).await?
        else {
            return Err("owner".into());
        };
        memory
            .commit_owned(&owner, &accepted.thread_id, 0, &saved.records[..cutoff])
            .await?;
        memory.stop_owner(&owner).await?;
        let replacement = fixture::server(false).await;
        let destination = ThreadService::with_store(
            app(vec![final_turn()])?,
            &[workspace.path().into()],
            memory.clone(),
        )?
        .with_resources(config(if changed { &replacement } else { &upstream }))?;
        let target = crate::thread::ThreadTarget {
            thread_id: accepted.thread_id.clone(),
            server_instance_id: destination.inner.instance_id.clone(),
        };
        let caller = CallerContext::local();
        let loaded = destination.load_thread(&target, &caller).await?;
        let recovery = loaded.recovery.as_ref().ok_or("recovery")?;
        assert!(recovery.turn.as_ref().ok_or("turn")?.resources.is_some());
        destination
            .recover_thread(
                &target,
                &caller,
                crate::thread::ThreadRecoveryRequest {
                    source_server_instance_id: recovery.source_server_instance_id.clone(),
                    source_cursor: recovery.source_cursor,
                    idempotency_key: "continue".into(),
                },
            )
            .await?;
        let expected = if changed {
            TurnStatus::Failed
        } else {
            TurnStatus::Completed
        };
        let done = wait_for(&destination, &accepted.turn_id, expected).await?;
        assert_eq!(done.status, expected);
        if changed {
            assert!(
                done.detail
                    .as_deref()
                    .is_some_and(|detail| detail.contains("resources changed"))
            );
        }
        assert_eq!(calls(&upstream).await?, 1);
        assert_eq!(calls(&replacement).await?, 0);
        let prompts = prompts(memory.as_ref(), &accepted.thread_id, &accepted.turn_id).await?;
        assert_eq!(prompts.len(), if changed { 1 } else { 2 });
        if !changed {
            assert!(serde_json::to_string(&prompts[1].messages)?.contains("retained"));
        }
        destination.shutdown().await;
    }
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn changed_mcp_catalog_rejects_old_dispatch_without_retry()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let mut configuration = stdio_config();
    if let bitrouter_sdk::mcp::transport::McpTransport::Stdio { args, .. } =
        &mut configuration.servers[0].transport
    {
        args.push("notify".into());
    }
    let mut client = crate::harness::mcp::McpConnections::connect(
        workspace.path(),
        &configuration.servers,
        configuration.protocol,
    )
    .await?;
    let cancel = tokio_util::sync::CancellationToken::new();
    assert_eq!(
        client
            .call("stdio__echo", &json!({}), 1024, &cancel)
            .await?
            .0,
        EffectStatus::Completed
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while client.validate_catalog().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    assert!(
        client
            .call("stdio__echo", &json!({}), 1024, &cancel)
            .await
            .is_err()
    );
    client.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn native_name_collision_is_rejected_before_model_execution()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let upstream = fixture::server(false).await;
    Mock::given(method("POST")).and(path("/mcp"))
        .and(|request:&Request| serde_json::from_slice::<serde_json::Value>(&request.body).is_ok_and(|v| v["method"]=="tools/list"))
        .respond_with(|request:&Request| {
            let body=serde_json::from_slice::<serde_json::Value>(&request.body).unwrap_or_default();
            ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":body["id"],"result":{"tools":[{"name":"read","inputSchema":{"type":"object"}}]}}))
        }).with_priority(1).mount(&upstream).await;
    let mut configuration = config(&upstream);
    configuration.servers[0].tool_prefix = Some(String::new());
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app(vec![final_turn()])?,
        &[workspace.path().into()],
        store.clone(),
    )?
    .with_resources(configuration)?;
    let accepted = service.submit_fixture(request(&workspace)).await?;
    let result = wait_for(&service, &accepted.turn_id, TurnStatus::Failed).await?;
    assert_eq!(result.status, TurnStatus::Failed);
    assert!(
        result
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("collides"))
    );
    assert!(
        prompts(store.as_ref(), &accepted.thread_id, &accepted.turn_id)
            .await?
            .is_empty()
    );
    service.shutdown().await;
    Ok(())
}
