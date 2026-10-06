use std::sync::Arc;
use std::time::Duration;

use bitrouter_sdk::caller::CallerContext;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

use super::support::{HeldModel, app, app_with_executor, final_turn, input, wait_for};
use crate::acp::native::{NativeAcpServer, NativeSessionConfig};
use crate::agent::AgentConfig;
use crate::service::ThreadService;
use crate::store::{ExecutionRecord, ExecutionStore, MemoryExecutionStore};
use crate::thread::{PermissionProfile, ThreadStatus};
use crate::turn::TurnStatus;

struct Peer {
    write: WriteHalf<DuplexStream>,
    read: BufReader<ReadHalf<DuplexStream>>,
    server: tokio::task::JoinHandle<Result<(), agent_client_protocol::Error>>,
    seen: Vec<Value>,
}

impl Peer {
    async fn connect(
        server: NativeAcpServer,
        version: u32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let (client, daemon) = tokio::io::duplex(64 * 1024);
        let (read, write) = tokio::io::split(daemon);
        let task = tokio::spawn(async move { server.connect(read, write).await });
        let (read, write) = tokio::io::split(client);
        let mut peer = Self {
            write,
            read: BufReader::new(read),
            server: task,
            seen: Vec::new(),
        };
        let initialize = peer.request(1, "initialize", json!({"protocolVersion":version,"info":{"name":"test","version":"1"},"clientInfo":{"name":"test","version":"1"}})).await?;
        assert_eq!(initialize["protocolVersion"], version);
        if version == 1 {
            assert_eq!(initialize["agentCapabilities"]["loadSession"], true);
        } else {
            assert!(initialize["capabilities"]["session"].is_object());
        }
        Ok(peer)
    }

    async fn send(&mut self, value: Value) -> Result<(), Box<dyn std::error::Error>> {
        let mut bytes = serde_json::to_vec(&value)?;
        bytes.push(b'\n');
        self.write.write_all(&bytes).await?;
        Ok(())
    }

    async fn receive(
        &mut self,
        matches: impl Fn(&Value) -> bool,
    ) -> Result<Value, Box<dyn std::error::Error>> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let mut line = String::new();
                if self.read.read_line(&mut line).await? == 0 {
                    return Err("ACP stream ended".into());
                }
                let value: Value = serde_json::from_str(&line)?;
                self.seen.push(value.clone());
                if matches(&value) {
                    return Ok(value);
                }
            }
        })
        .await?
    }

    async fn request(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
    ) -> Result<Value, Box<dyn std::error::Error>> {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        let response = self
            .receive(|value| {
                value["id"] == id && (value.get("result").is_some() || value.get("error").is_some())
            })
            .await?;
        if let Some(error) = response.get("error") {
            return Err(format!("{method}: {error}").into());
        }
        Ok(response["result"].clone())
    }

    async fn disconnect(self) -> Result<(), Box<dyn std::error::Error>> {
        let Self {
            mut write,
            read,
            server,
            ..
        } = self;
        write.shutdown().await?;
        drop(write);
        drop(read);
        let _ = tokio::time::timeout(Duration::from_secs(5), server).await??;
        Ok(())
    }
}

fn server(service: &ThreadService, read_only: bool) -> NativeAcpServer {
    let agent = AgentConfig::fixed("fixture-model", None);
    NativeAcpServer::new(
        service.clone(),
        CallerContext::local(),
        NativeSessionConfig {
            agent: if read_only { agent.read_only() } else { agent },
            permission_profile: if read_only {
                PermissionProfile::ReadOnly
            } else {
                PermissionProfile::Ask
            },
            servers: Vec::new(),
            register_local_workspaces: false,
        },
    )
}

fn native_target(service: &ThreadService, session: &str) -> crate::thread::ThreadTarget {
    crate::thread::ThreadTarget {
        thread_id: session.into(),
        server_instance_id: service.capabilities().server_instance_id,
    }
}

fn setup(workspace: &TempDir) -> Value {
    json!({"cwd":workspace.path(),"mcpServers":[]})
}

#[tokio::test]
async fn both_wire_versions_share_stable_history_and_prompt_completion()
-> Result<(), Box<dyn std::error::Error>> {
    for first_version in [1, 2] {
        let workspace = TempDir::new()?;
        let store = Arc::new(MemoryExecutionStore::default());
        let service = ThreadService::with_store(
            app(vec![final_turn(), final_turn()])?,
            &[workspace.path().to_path_buf()],
            store.clone(),
        )?;
        let server = server(&service, true);
        let mut peer = Peer::connect(server.clone(), first_version).await?;
        let created = peer.request(2, "session/new", setup(&workspace)).await?;
        let session = created["sessionId"]
            .as_str()
            .ok_or("session ID missing")?
            .to_string();
        let response = peer
            .request(
                3,
                "session/prompt",
                json!({"sessionId":session,"prompt":[{"type":"text","text":"first"}]}),
            )
            .await?;
        let native_target = native_target(&service, &session);
        let turn_id = service
            .read_thread(&native_target, &CallerContext::local())?
            .active_turn_id
            .or_else(|| {
                service
                    .read_thread_view(&native_target, &CallerContext::local())
                    .ok()
                    .and_then(|view| view.latest_turn.map(|turn| turn.turn_id))
            })
            .ok_or("Turn missing")?;
        service
            .wait_turn_settled(&native_target, &CallerContext::local(), &turn_id)
            .await?;
        if first_version == 1 {
            assert_eq!(response["stopReason"], "end_turn");
        } else {
            let stored = store.load(&session).await?.ok_or("history missing")?;
            let id = stored
                .records
                .iter()
                .find_map(|record| {
                    if let ExecutionRecord::TurnQueued { user_item_id, .. } = record {
                        Some(user_item_id)
                    } else {
                        None
                    }
                })
                .ok_or("user Item missing")?;
            assert_eq!(response["messageId"], *id);
        }
        peer.disconnect().await?;
        let second_version = 3 - first_version;
        let mut peer = Peer::connect(server.clone(), second_version).await?;
        let mut reopen = setup(&workspace);
        reopen["sessionId"] = json!(session);
        if second_version == 2 {
            reopen["replayFrom"] = json!({"type":"start"});
        }
        peer.request(
            2,
            if second_version == 1 {
                "session/load"
            } else {
                "session/resume"
            },
            reopen,
        )
        .await?;
        assert!(
            peer.seen
                .iter()
                .any(|value| value.pointer("/params/update/content/0/text")
                    == Some(&json!("done"))
                    || value.pointer("/params/update/content/text") == Some(&json!("done")))
        );
        let listed = peer.request(3, "session/list", json!({})).await?;
        assert_eq!(listed["sessions"][0]["sessionId"], session);
        let response = peer
            .request(
                4,
                "session/prompt",
                json!({"sessionId":session,"prompt":[{"type":"text","text":"second"}]}),
            )
            .await?;
        if second_version == 1 {
            assert_eq!(response["stopReason"], "end_turn");
        } else {
            assert!(response["messageId"].is_string());
        }
        peer.request(5, "session/close", json!({"sessionId":session}))
            .await?;
        assert!(store.load(&session).await?.is_some());
        peer.disconnect().await?;
        service.shutdown().await;
        server.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn disconnect_during_model_work_preserves_execution_and_same_approval_on_cross_version_reconnect()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let held = Arc::new(HeldModel::new());
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(
        app_with_executor(held.clone())?,
        &[workspace.path().to_path_buf()],
        store.clone(),
    )?;
    let server = server(&service, false);
    let mut peer = Peer::connect(server.clone(), 2).await?;
    let session = peer.request(2, "session/new", setup(&workspace)).await?["sessionId"]
        .as_str()
        .ok_or("session missing")?
        .to_string();
    let accepted = peer
        .request(
            3,
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"text","text":"write"}]}),
        )
        .await?;
    let turn_id = accepted
        .pointer("/_meta/bitrouter/turnId")
        .and_then(Value::as_str)
        .ok_or("Turn missing")?
        .to_string();
    held.wait().await?;
    peer.disconnect().await?;
    assert_eq!(service.read(&turn_id)?.status, TurnStatus::Running);
    held.release.add_permits(1);
    let waiting = wait_for(&service, &turn_id, TurnStatus::WaitingForInput).await?;
    let input = waiting.pending_input.ok_or("input missing")?;
    let mut peer = Peer::connect(server.clone(), 1).await?;
    let mut reopen = setup(&workspace);
    reopen["sessionId"] = json!(session);
    peer.request(2, "session/load", reopen).await?;
    let permission = peer
        .receive(|value| value["method"] == "session/request_permission")
        .await?;
    assert_eq!(
        permission.pointer("/params/_meta/bitrouter/requestId"),
        Some(&json!(input.request_id))
    );
    assert_eq!(
        permission.pointer("/params/toolCall/toolCallId"),
        Some(&json!(input.tool_id))
    );
    peer.send(json!({"jsonrpc":"2.0","id":permission["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}})).await?;
    let target = native_target(&service, &session);
    assert_eq!(
        service
            .wait_turn_settled(&target, &CallerContext::local(), &turn_id)
            .await?
            .status,
        TurnStatus::Completed
    );
    assert!(workspace.path().join("stale.txt").exists());
    let saved = store.load(&session).await?.ok_or("history missing")?;
    assert!(!saved.records.iter().any(|record| matches!(record, ExecutionRecord::TurnRecord { fact, .. } if matches!(fact.as_ref(), ExecutionRecord::TurnLifecycle { lifecycle: crate::turn::TurnLifecycle::CancelRequested, .. }))));
    peer.disconnect().await?;
    service.shutdown().await;
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn wire_cancel_pauses_queue_and_close_withdraws_it_durably()
-> Result<(), Box<dyn std::error::Error>> {
    for version in [1, 2] {
        let workspace = TempDir::new()?;
        let held = Arc::new(HeldModel::new());
        let store = Arc::new(MemoryExecutionStore::default());
        let service = ThreadService::with_store(
            app_with_executor(held.clone())?,
            &[workspace.path().to_path_buf()],
            store.clone(),
        )?;
        let server = server(&service, false);
        let mut peer = Peer::connect(server.clone(), version).await?;
        let session = peer.request(2, "session/new", setup(&workspace)).await?["sessionId"]
            .as_str()
            .ok_or("session missing")?
            .to_string();
        peer.send(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"held"}]}})).await?;
        held.wait().await?;
        let target = native_target(&service, &session);
        let active = service
            .read_thread(&target, &CallerContext::local())?
            .active_turn_id
            .ok_or("active missing")?;
        let queued = service
            .enqueue_turn(&target, &CallerContext::local(), input("queued", "queue"))
            .await?;
        peer.send(
            json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}),
        )
        .await?;
        service
            .wait_turn_settled(&target, &CallerContext::local(), &active)
            .await?;
        let paused = service.read_thread(&target, &CallerContext::local())?;
        assert_eq!(paused.status, ThreadStatus::Paused);
        assert_eq!(paused.queued[0].turn_id, queued.turn_id);
        peer.request(
            4,
            "session/close",
            json!({"sessionId":session,"_meta":{"bitrouter":{"idempotencyKey":"close"}}}),
        )
        .await?;
        assert_eq!(
            service
                .read_stored_turn(&target, &CallerContext::local(), &queued.turn_id)
                .await?
                .status,
            TurnStatus::Cancelled
        );
        assert!(
            service
                .read_thread(&target, &CallerContext::local())?
                .queued
                .is_empty()
        );
        assert!(!workspace.path().join("stale.txt").exists());
        peer.disconnect().await?;
        service.shutdown().await;
        server.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn replacement_approval_owner_rejects_old_wire_answer_in_both_versions()
-> Result<(), Box<dyn std::error::Error>> {
    for version in [1, 2] {
        let workspace = TempDir::new()?;
        let store = Arc::new(MemoryExecutionStore::default());
        let service = ThreadService::with_store(
            app(vec![
                super::support::turn(vec![super::support::tool_call(
                    "write",
                    "write",
                    json!({"path":"forbidden.txt","content":"late approval"}),
                )]),
                final_turn(),
            ])?,
            &[workspace.path().to_path_buf()],
            store.clone(),
        )?;
        let server = server(&service, false);
        let mut old = Peer::connect(server.clone(), version).await?;
        let session = old.request(2, "session/new", setup(&workspace)).await?["sessionId"]
            .as_str()
            .ok_or("session missing")?
            .to_string();
        old.send(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"write"}]}})).await?;
        let first = old
            .receive(|value| value["method"] == "session/request_permission")
            .await?;
        let target = native_target(&service, &session);
        let active = service
            .read_thread(&target, &CallerContext::local())?
            .active_turn_id
            .ok_or("active missing")?;
        let mut new = Peer::connect(server.clone(), 3 - version).await?;
        let mut reopen = setup(&workspace);
        reopen["sessionId"] = json!(session);
        new.request(2, "session/resume", reopen).await?;
        let current = new
            .receive(|value| value["method"] == "session/request_permission")
            .await?;
        assert_eq!(
            first.pointer("/params/_meta/bitrouter/requestId"),
            current.pointer("/params/_meta/bitrouter/requestId")
        );
        if version == 2 {
            assert!(
                first
                    .pointer("/params/subject/toolCall/toolCallId")
                    .is_some()
            );
        } else {
            assert!(
                current
                    .pointer("/params/subject/toolCall/toolCallId")
                    .is_some()
            );
        }
        old.send(json!({"jsonrpc":"2.0","id":first["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}})).await?;
        new.send(json!({"jsonrpc":"2.0","id":current["id"],"result":{"outcome":{"outcome":"selected","optionId":"reject_once"}}})).await?;
        service
            .wait_turn_settled(&target, &CallerContext::local(), &active)
            .await?;
        assert!(!workspace.path().join("forbidden.txt").exists());
        let saved = store.load(&session).await?.ok_or("history missing")?;
        let answers: Vec<_> = saved
            .records
            .iter()
            .filter_map(|record| {
                if let ExecutionRecord::TurnRecord { fact, .. } = record {
                    if let ExecutionRecord::TurnLifecycle {
                        lifecycle: crate::turn::TurnLifecycle::InputResolved { approved, .. },
                        ..
                    } = fact.as_ref()
                    {
                        Some(*approved)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(answers, vec![false]);
        new.request(
            3,
            "session/close",
            json!({"sessionId":session,"_meta":{"bitrouter":{"idempotencyKey":"close"}}}),
        )
        .await?;
        new.request(
            4,
            "session/close",
            json!({"sessionId":session,"_meta":{"bitrouter":{"idempotencyKey":"close"}}}),
        )
        .await?;
        old.disconnect().await?;
        new.disconnect().await?;
        service.shutdown().await;
        server.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn reconnect_keeps_queue_paused_until_explicit_extension()
-> Result<(), Box<dyn std::error::Error>> {
    for version in [1, 2] {
        let workspace = TempDir::new()?;
        let held = Arc::new(HeldModel::new());
        let service = ThreadService::new(
            app_with_executor(held.clone())?,
            &[workspace.path().to_path_buf()],
        )?;
        let server = server(&service, false);
        let mut peer = Peer::connect(server.clone(), version).await?;
        let session = peer.request(2, "session/new", setup(&workspace)).await?["sessionId"]
            .as_str()
            .ok_or("session missing")?
            .to_string();
        peer.send(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"held"}]}})).await?;
        held.wait().await?;
        let target = native_target(&service, &session);
        let active = service
            .read_thread(&target, &CallerContext::local())?
            .active_turn_id
            .ok_or("active missing")?;
        let queued = service
            .enqueue_turn(&target, &CallerContext::local(), input("retained", "queue"))
            .await?;
        peer.send(
            json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}),
        )
        .await?;
        service
            .wait_turn_settled(&target, &CallerContext::local(), &active)
            .await?;
        if version == 2 {
            peer.receive(|v| {
                v.pointer("/params/update/state") == Some(&json!("idle"))
                    && v.pointer("/params/update/stopReason") == Some(&json!("cancelled"))
            })
            .await?;
            peer.receive(|v| v.pointer("/params/update/state") == Some(&json!("requires_action")))
                .await?;
        } else {
            peer.receive(|v| {
                v["id"] == 3 && v.pointer("/result/stopReason") == Some(&json!("cancelled"))
            })
            .await?;
        }
        peer.disconnect().await?;
        let mut peer = Peer::connect(server.clone(), 3 - version).await?;
        let mut reopen = setup(&workspace);
        reopen["sessionId"] = json!(session);
        peer.request(2, "session/resume", reopen).await?;
        assert_eq!(
            service
                .read_thread(&target, &CallerContext::local())?
                .status,
            ThreadStatus::Paused
        );
        assert!(
            peer.request(
                3,
                "session/prompt",
                json!({"sessionId":session,"prompt":[{"type":"text","text":"cannot overtake"}]})
            )
            .await
            .is_err()
        );
        peer.request(
            4,
            "_bitrouter/session/resume_queue",
            json!({"sessionId":session}),
        )
        .await?;
        let waiting = wait_for(&service, &queued.turn_id, TurnStatus::WaitingForInput).await?;
        assert_eq!(waiting.status, TurnStatus::WaitingForInput);
        peer.request(5, "session/close", json!({"sessionId":session}))
            .await?;
        assert!(!workspace.path().join("stale.txt").exists());
        peer.disconnect().await?;
        service.shutdown().await;
        server.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn invalid_resources_media_and_oversized_prompts_admit_no_execution()
-> Result<(), Box<dyn std::error::Error>> {
    for version in [1, 2] {
        let workspace = TempDir::new()?;
        let held = Arc::new(HeldModel::new());
        let service = ThreadService::new(
            app_with_executor(held.clone())?,
            &[workspace.path().to_path_buf()],
        )?;
        let server = server(&service, false);
        let mut peer = Peer::connect(server.clone(), version).await?;
        let mut bad = setup(&workspace);
        bad["mcpServers"] =
            json!([{"name":"injected","command":"sh","args":["-c","touch forbidden"],"env":[]}]);
        assert!(peer.request(2, "session/new", bad).await.is_err());
        let ungranted = TempDir::new()?;
        assert!(
            peer.request(3, "session/new", setup(&ungranted))
                .await
                .is_err()
        );
        let session = peer.request(4, "session/new", setup(&workspace)).await?["sessionId"]
            .as_str()
            .ok_or("session missing")?
            .to_string();
        assert!(peer.request(5,"session/prompt",json!({"sessionId":session,"prompt":[{"type":"image","data":"YWJj","mimeType":"image/png"}]})).await.is_err());
        assert!(peer.request(6,"session/prompt",json!({"sessionId":session,"prompt":[{"type":"text","text":"x".repeat(service.capabilities().limits.request_bytes+1)}]})).await.is_err());
        assert_eq!(held.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(
            service
                .read_thread(&native_target(&service, &session), &CallerContext::local())?
                .active_turn_id
                .is_none()
        );
        peer.disconnect().await?;
        service.shutdown().await;
        server.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn accepted_close_survives_eof_without_blocking_other_threads_and_cold_history()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let other = TempDir::new()?;
    let store = Arc::new(super::close::CloseStore {
        memory: MemoryExecutionStore::default(),
        entered: tokio::sync::Semaphore::new(0),
        release: tokio::sync::Semaphore::new(0),
        reject: false,
    });
    let held = Arc::new(HeldModel::with_turns(vec![final_turn()]));
    let service = ThreadService::with_store(
        app_with_executor(held.clone())?,
        &[workspace.path().to_path_buf(), other.path().to_path_buf()],
        store.clone(),
    )?;
    let server = server(&service, true);
    let mut peer = Peer::connect(server.clone(), 2).await?;
    let session = peer.request(2, "session/new", setup(&workspace)).await?["sessionId"]
        .as_str()
        .ok_or("session missing")?
        .to_string();
    let target = native_target(&service, &session);
    peer.request(
        7,
        "session/prompt",
        json!({"sessionId":session,"prompt":[{"type":"text","text":"hold"}]}),
    )
    .await?;
    held.wait().await?;
    let active = service
        .read_thread(&target, &CallerContext::local())?
        .active_turn_id
        .ok_or("active missing")?;
    let queued = service
        .enqueue_turn(
            &target,
            &CallerContext::local(),
            input("withdraw", "withdraw"),
        )
        .await?;
    service
        .cancel_turn(
            &target,
            &CallerContext::local(),
            crate::turn::CancelTurnRequest {
                turn_id: active.clone(),
                idempotency_key: "cancel".into(),
            },
        )
        .await?;
    service
        .wait_turn_settled(&target, &CallerContext::local(), &active)
        .await?;
    peer.send(json!({"jsonrpc":"2.0","id":3,"method":"session/close","params":{"sessionId":session,"_meta":{"bitrouter":{"idempotencyKey":"close"}}}})).await?;
    tokio::time::timeout(Duration::from_secs(3), store.entered.acquire())
        .await??
        .forget();
    assert_eq!(
        service
            .read_thread(&target, &CallerContext::local())?
            .status,
        ThreadStatus::Closing
    );
    peer.disconnect().await?;
    let mut second = Peer::connect(server.clone(), 1).await?;
    let second_session = second.request(2, "session/new", setup(&other)).await?["sessionId"]
        .as_str()
        .ok_or("session missing")?
        .to_string();
    assert_eq!(
        second
            .request(
                3,
                "session/prompt",
                json!({"sessionId":second_session,"prompt":[{"type":"text","text":"independent"}]})
            )
            .await?["stopReason"],
        "end_turn"
    );
    store.release.add_permits(1);
    // Same operation key joins the accepted close and proves durable completion.
    service
        .close_thread(&target, &CallerContext::local(), "close".into())
        .await?;
    assert_eq!(
        service
            .read_stored_turn(&target, &CallerContext::local(), &queued.turn_id)
            .await?
            .status,
        TurnStatus::Cancelled
    );
    second.disconnect().await?;
    service.shutdown().await;
    server.shutdown().await;
    let restarted = ThreadService::with_store(
        app(vec![])?,
        &[workspace.path().to_path_buf(), other.path().to_path_buf()],
        store.clone(),
    )?;
    let mut peer = Peer::connect(self::server(&restarted, true), 1).await?;
    let mut reopen = setup(&workspace);
    reopen["sessionId"] = json!(session);
    peer.request(2, "session/load", reopen).await?;
    let saved = store
        .memory
        .load(&session)
        .await?
        .ok_or("history missing")?;
    assert_eq!(saved.format_version, 5);
    assert_eq!(
        saved
            .records
            .iter()
            .filter(|record| matches!(record, ExecutionRecord::ThreadCloseCompleted { .. }))
            .count(),
        1
    );
    assert_eq!(
        restarted
            .read_thread(
                &native_target(&restarted, &session),
                &CallerContext::local()
            )?
            .queued
            .len(),
        0
    );
    peer.disconnect().await?;
    restarted.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn unicode_stream_reconciles_once_and_replays_beyond_hot_history()
-> Result<(), Box<dyn std::error::Error>> {
    for version in [1, 2] {
        let workspace = TempDir::new()?;
        let text = "路由🦀\n".repeat(4000);
        let response = super::support::turn(vec![bitrouter_sdk::language_model::Content::Text {
            text: text.clone(),
            provider_metadata: Default::default(),
        }]);
        let service = ThreadService::with_limits(
            app(vec![response.clone(); 12])?,
            &[workspace.path().to_path_buf()],
            crate::service::RuntimeLimits {
                events_per_thread: 2,
                ..Default::default()
            },
        )?;
        let mut config = AgentConfig::fixed("fixture-model", None).read_only();
        config.max_context_bytes = service.capabilities().limits.context_bytes_per_thread;
        let server = server(&service, true).with_agent_config(config);
        let mut peer = Peer::connect(server.clone(), version).await?;
        let session = peer.request(2, "session/new", setup(&workspace)).await?["sessionId"]
            .as_str()
            .ok_or("session missing")?
            .to_string();
        for number in 0..12 {
            let accepted = peer.request(10+number,"session/prompt",json!({"sessionId":session,"prompt":[{"type":"text","text":format!("prompt {number}")}]})).await?;
            let target = native_target(&service, &session);
            let turn_id = if version == 2 {
                accepted
                    .pointer("/_meta/bitrouter/turnId")
                    .and_then(Value::as_str)
                    .ok_or("Turn missing")?
                    .to_string()
            } else {
                service
                    .read_thread_view(&target, &CallerContext::local())?
                    .latest_turn
                    .ok_or("Turn missing")?
                    .turn_id
            };
            service
                .wait_turn_settled(&target, &CallerContext::local(), &turn_id)
                .await?;
        }
        if version == 1 {
            let joined = peer
                .seen
                .iter()
                .filter(|v| {
                    v.pointer("/params/update/sessionUpdate") == Some(&json!("agent_message_chunk"))
                })
                .filter_map(|v| {
                    v.pointer("/params/update/content/text")
                        .and_then(Value::as_str)
                })
                .collect::<String>();
            assert_eq!(joined, text.repeat(12));
        }
        peer.disconnect().await?;
        let mut peer = Peer::connect(server.clone(), 3 - version).await?;
        let mut reopen = setup(&workspace);
        reopen["sessionId"] = json!(session);
        if version == 1 {
            reopen["replayFrom"] = json!({"type":"start"});
        }
        peer.request(
            2,
            if version == 1 {
                "session/resume"
            } else {
                "session/load"
            },
            reopen,
        )
        .await?;
        let history: Vec<_> = peer
            .seen
            .iter()
            .filter(|v| v["method"] == "session/update")
            .collect();
        if version == 1 {
            let full: Vec<_> = history
                .iter()
                .filter(|v| {
                    v.pointer("/params/update/sessionUpdate") == Some(&json!("agent_message"))
                })
                .collect();
            assert_eq!(full.len(), 12);
            assert!(
                full.iter()
                    .all(|v| v.pointer("/params/update/content/0/text") == Some(&json!(text)))
            );
            let ids: std::collections::HashSet<_> = full
                .iter()
                .filter_map(|v| {
                    v.pointer("/params/update/messageId")
                        .and_then(Value::as_str)
                })
                .collect();
            assert_eq!(ids.len(), 12);
        } else {
            let joined = history
                .iter()
                .filter(|v| {
                    v.pointer("/params/update/sessionUpdate") == Some(&json!("agent_message_chunk"))
                })
                .filter_map(|v| {
                    v.pointer("/params/update/content/text")
                        .and_then(Value::as_str)
                })
                .collect::<String>();
            assert_eq!(joined, text.repeat(12));
        }
        peer.disconnect().await?;
        service.shutdown().await;
        server.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn stalled_replay_disconnects_observer_without_cancelling_native_history()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let response = super::support::turn(vec![bitrouter_sdk::language_model::Content::Text {
        text: "路由🦀\n".repeat(4000),
        provider_metadata: Default::default(),
    }]);
    let service = ThreadService::with_limits_and_store(
        app(vec![response; 12])?,
        &[workspace.path().to_path_buf()],
        crate::service::RuntimeLimits {
            subscriber_bytes_per_thread: 2 * 1024 * 1024,
            ..Default::default()
        },
        store.clone(),
    )?;
    let mut request = super::support::thread_request(&workspace, "create");
    request.config = request.config.read_only();
    request.config.max_context_bytes = service.capabilities().limits.context_bytes_per_thread;
    request.permission_profile = PermissionProfile::ReadOnly;
    let thread = service
        .create_thread(&service.capabilities().server_instance_id, request)
        .await?;
    let target = native_target(&service, &thread.thread_id);
    for number in 0..12 {
        let receipt = service
            .start_turn(
                &target,
                &CallerContext::local(),
                input("prime history", &format!("prime-{number}")),
            )
            .await?;
        service
            .wait_turn_settled(&target, &CallerContext::local(), &receipt.turn_id)
            .await?;
    }
    let server = server(&service, true);
    let mut peer = Peer::connect(server.clone(), 2).await?;
    for id in 2..10 {
        peer.send(json!({"jsonrpc":"2.0","id":id,"method":"session/resume","params":{"sessionId":thread.thread_id,"cwd":workspace.path(),"mcpServers":[],"replayFrom":{"type":"start"}}})).await?;
    }
    // Deliberately do not consume stdout. Native byte admission closes this
    // transport after its bounded stall interval, without business cancellation.
    let ended = tokio::time::timeout(Duration::from_secs(8), &mut peer.server).await??;
    assert!(ended.is_err());
    assert_eq!(
        service
            .read_thread(&target, &CallerContext::local())?
            .status,
        ThreadStatus::Idle
    );
    let saved = store
        .load(&thread.thread_id)
        .await?
        .ok_or("history missing")?;
    assert!(!saved.records.iter().any(|record|matches!(record,ExecutionRecord::TurnRecord{fact,..} if matches!(fact.as_ref(),ExecutionRecord::TurnLifecycle{lifecycle:crate::turn::TurnLifecycle::CancelRequested,..}))));
    drop(peer);
    service.shutdown().await;
    server.shutdown().await;
    Ok(())
}
