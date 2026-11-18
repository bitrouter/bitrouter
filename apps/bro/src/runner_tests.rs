//! Real HTTP/SSE requests exercise the public SDK and the stored restart boundary.
use std::fs;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::harness::agent::ToolMode;
use bitrouter_ai::harness::turn::TurnStatus;
use bitrouter_sdk::gateway::GatewayClient;
use serde_json::json;

use crate::{
    checkpoint::{Home, ModelState, Next, Thread, ToolState},
    runner,
};

#[path = "../tests/support/gateway.rs"]
mod gateway_fixture;
use gateway_fixture::{Mock, chunk, stream};

fn call_stream(done: bool) -> String {
    let mut body = chunk(
        json!({"role":"assistant","reasoning_content":"retained","tool_calls":[
            {"index":0,"id":"read1","type":"function","function":{"name":"read","arguments":"{\"path\":\"seed\"}"}},
            {"index":1,"id":"write1","type":"function","extra_content":{"google":{"thought_signature":"signed"}},"function":{"name":"write","arguments":"{\"path\":"}}
        ]}),
        None,
    );
    body.push_str(&chunk(json!({"tool_calls":[{"index":1,"function":{"arguments":"\"answer\",\"content\":\"done\"}"}}]}),Some("tool_calls")));
    if done {
        body.push_str("data: [DONE]\n\n");
    }
    stream(body)
}

struct Work {
    _dir: tempfile::TempDir,
    home: Home,
    thread: Thread,
    client: GatewayClient,
}

impl Work {
    fn new(endpoint: &str) -> Result<Self> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("work");
        fs::create_dir(&root)?;
        fs::write(root.join("seed"), "original\n")?;
        let home = Home::open(&dir.path().join("bro"))?;
        let mut thread = Thread::new(
            root.canonicalize()?,
            endpoint.into(),
            "fixture".into(),
            None,
            None,
            ToolMode::Coding,
        )?;
        thread.start("Inspect and write".into(), None)?;
        home.admit(&thread)?;
        let client = GatewayClient::new(endpoint, Some("scoped-test-key"))?;
        Ok(Self {
            _dir: dir,
            home,
            thread,
            client,
        })
    }
}

#[tokio::test]
async fn approval_restart_keeps_confirmed_results_and_continuity() -> Result<()> {
    for approved in [true, false] {
        let final_reply =
            stream(chunk(json!({"content":"Finished."}), Some("stop")) + "data: [DONE]\n\n");
        let mock = Mock::start(vec![call_stream(true), final_reply])?;
        let Work {
            _dir,
            home,
            mut thread,
            client,
        } = Work::new(&mock.endpoint)?;
        runner::run(&home, &mut thread, &client).await?;
        let mut restored = home.load(thread.thread_id)?;
        let request_id = restored
            .current()?
            .pending()
            .context("expected approval")?
            .request_id
            .clone();
        ensure!(
            !restored.workspace.join("answer").exists(),
            "write ran before approval"
        );
        fs::write(restored.workspace.join("seed"), "changed after read")?;
        assert!(runner::answer(&home, &mut restored, "stale", approved).is_err());
        runner::answer(&home, &mut restored, &request_id, approved)?;
        let mut restored = home.load(thread.thread_id)?;
        runner::run(&home, &mut restored, &client).await?;
        assert_eq!(restored.current()?.status(false), TurnStatus::Completed);
        assert_eq!(restored.workspace.join("answer").exists(), approved);
        // Another explicit resume of an already settled Turn must stay offline.
        runner::run(&home, &mut restored, &client).await?;
        let requests = mock.join()?;
        assert_eq!(requests.len(), 2);
        let history = requests[1]["messages"]
            .as_array()
            .context("missing history")?;
        let assistant = history
            .iter()
            .find(|m| m["role"] == "assistant")
            .context("missing assistant")?;
        assert_eq!(assistant["reasoning_content"], "retained");
        assert_eq!(
            assistant["tool_calls"][1]["extra_content"]["google"]["thought_signature"],
            "signed"
        );
        let results: Vec<_> = history.iter().filter(|m| m["role"] == "tool").collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["tool_call_id"], "read1");
        assert!(
            results[0]["content"]
                .as_str()
                .context("missing read output")?
                .contains("original")
        );
        assert_eq!(results[1]["tool_call_id"], "write1");
        let saved = serde_json::to_string(&restored)?;
        assert!(!saved.contains("scoped-test-key"));
    }
    Ok(())
}

#[tokio::test]
async fn truncated_paid_stream_refuses_any_replay() -> Result<()> {
    let mock = Mock::start(vec![call_stream(false)])?;
    let Work {
        _dir,
        home,
        mut thread,
        client,
    } = Work::new(&mock.endpoint)?;
    assert!(runner::run(&home, &mut thread, &client).await.is_err());
    let mut restored = home.load(thread.thread_id)?;
    assert_eq!(
        restored.current()?.status(false),
        TurnStatus::RecoveryRequired
    );
    assert!(restored.start("retry".into(), None).is_err());
    assert!(runner::run(&home, &mut restored, &client).await.is_err());
    assert!(!restored.workspace.join("answer").exists());
    assert_eq!(mock.join()?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn lost_worker_with_tool_intent_blocks_even_after_cancel() -> Result<()> {
    let mock = Mock::start(vec![call_stream(true)])?;
    let Work {
        _dir,
        home,
        mut thread,
        client,
    } = Work::new(&mock.endpoint)?;
    runner::run(&home, &mut thread, &client).await?;
    let pending = thread
        .current()?
        .pending()
        .context("missing pending call")?
        .request_id
        .clone();
    runner::answer(&home, &mut thread, &pending, true)?;
    thread.response_mut()?.calls[1].state = ToolState::ToolIntent;
    home.save(&thread)?;
    let mut loaded = home.load(thread.thread_id)?;
    assert_eq!(loaded.current()?.next(), Next::Blocked);
    runner::cancel_turn(&home, &mut loaded)?;
    assert_eq!(
        home.load(thread.thread_id)?.current()?.status(false),
        TurnStatus::RecoveryRequired
    );
    assert!(loaded.start("new".into(), None).is_err());
    assert!(!loaded.workspace.join("answer").exists());
    assert_eq!(mock.join()?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn cancelling_waiting_approval_settles_without_effects() -> Result<()> {
    let mock = Mock::start(vec![call_stream(true)])?;
    let Work {
        _dir,
        home,
        mut thread,
        client,
    } = Work::new(&mock.endpoint)?;
    runner::run(&home, &mut thread, &client).await?;
    runner::cancel_turn(&home, &mut thread)?;
    let loaded = home.load(thread.thread_id)?;
    assert_eq!(loaded.current()?.status(false), TurnStatus::Cancelled);
    assert!(loaded.current()?.pending().is_none());
    assert!(!loaded.workspace.join("answer").exists());
    let last = loaded.current()?.steps.last().context("missing step")?;
    assert!(
        matches!(&last.state,ModelState::ModelResponse {response} if matches!(response.calls[1].state,ToolState::ToolResult{effect:bitrouter_ai::harness::runtime::EffectStatus::NotExecuted,..}))
    );
    assert_eq!(mock.join()?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn permission_denial_after_dispatch_preserves_receipts_and_ownership() -> Result<()> {
    let body =
        r#"{"error":{"message":"denied","type":"permission_error","code":"permission_denied"}}"#;
    let mock = Mock::start(vec![format!(
        "HTTP/1.1 403 Forbidden\r\nX-BitRouter-Request-Id: req-fixture\r\nX-BitRouter-Execution-Id: exec-fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )])?;
    let Work {
        _dir,
        home,
        mut thread,
        client,
    } = Work::new(&mock.endpoint)?;
    assert!(runner::run(&home, &mut thread, &client).await.is_err());
    let mut loaded = home.load(thread.thread_id)?;
    assert_eq!(
        loaded.current()?.status(false),
        TurnStatus::RecoveryRequired
    );
    let step = &loaded.current()?.steps[0];
    assert_eq!(step.request_id.as_deref(), Some("req-fixture"));
    assert_eq!(step.execution_id.as_deref(), Some("exec-fixture"));
    assert!(loaded.current()?.response().is_none());
    assert!(loaded.start("retry".into(), None).is_err());
    assert!(runner::run(&home, &mut loaded, &client).await.is_err());
    assert_eq!(mock.join()?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn malformed_success_preserves_each_unambiguous_receipt_independently() -> Result<()> {
    for (headers, request, execution) in [
        (
            "X-BitRouter-Request-Id: first\r\nX-BitRouter-Request-Id: second\r\nX-BitRouter-Execution-Id: execution\r\n",
            None,
            Some("execution"),
        ),
        (
            "X-BitRouter-Request-Id: request\r\nX-BitRouter-Execution-Id: invalid identity\r\n",
            Some("request"),
            None,
        ),
    ] {
        let mock = Mock::start(vec![format!(
            "HTTP/1.1 200 OK\r\n{headers}Content-Type: text/event-stream\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )])?;
        let Work {
            _dir,
            home,
            mut thread,
            client,
        } = Work::new(&mock.endpoint)?;
        assert!(runner::run(&home, &mut thread, &client).await.is_err());
        let restored = home.load(thread.thread_id)?;
        assert_eq!(
            restored.current()?.status(false),
            TurnStatus::RecoveryRequired
        );
        let step = &restored.current()?.steps[0];
        assert_eq!(step.request_id.as_deref(), request);
        assert_eq!(step.execution_id.as_deref(), execution);
        assert_eq!(mock.join()?.len(), 1);
    }
    Ok(())
}

/// Lose the store acknowledgement at the parent join or at terminal settlement,
/// on either side of the atomic rename. Each paid send must occur exactly once.
#[tokio::test]
async fn confirmed_child_and_terminal_recover_on_either_side_of_a_lost_ack() -> Result<()> {
    struct Fault<'a> {
        home: &'a Home,
        committed: bool,
        terminal: bool,
    }
    impl runner::Journal for Fault<'_> {
        fn save(&self, thread: &Thread) -> Result<()> {
            let turn = thread.current()?;
            let joining = !turn.children.is_empty()
                && turn.response().is_some_and(|r| {
                    r.calls
                        .iter()
                        .any(|i| matches!(i.state, ToolState::ToolResult { .. }))
                });
            let inject = if self.terminal {
                turn.outcome.is_some()
            } else {
                joining
            };
            if !inject || self.committed {
                self.home.save(thread)?;
            }
            ensure!(!inject, "injected lost acknowledgement");
            Ok(())
        }
    }
    for (terminal, committed) in [(false, false), (false, true), (true, false), (true, true)] {
        let reply = |delta, finish| stream(chunk(delta, Some(finish)) + "data: [DONE]\n\n");
        let model = Mock::start(vec![
            reply(
                json!({"role":"assistant","tool_calls":[{"index":0,"type":"function","id":"spawn","function":{
                "name":"spawn_agent","arguments":json!({"task":{"text":"inspect"}}).to_string()}}]}),
                "tool_calls",
            ),
            reply(
                json!({"role":"assistant","content":"child evidence"}),
                "stop",
            ),
            reply(
                json!({"role":"assistant","content":"parent consumed evidence"}),
                "stop",
            ),
        ])?;
        let Work {
            _dir,
            home,
            mut thread,
            client,
        } = Work::new(&model.endpoint)?;
        thread.enable_managed();
        home.save(&thread)?;
        let tree = crate::tree::Scheduler::restore(&thread)?;
        let result = runner::drive(
            &Fault {
                home: &home,
                committed,
                terminal,
            },
            &mut thread,
            &client,
            &tokio_util::sync::CancellationToken::new(),
            Some(&tree),
            0,
        )
        .await;
        ensure!(result.is_err_and(|e| e.to_string().contains("lost acknowledgement")));
        let mut restored = home.load(thread.thread_id)?;
        let child = restored
            .current()?
            .children
            .values()
            .next()
            .context("child")?;
        ensure!(
            child.current()?.steps.len() == 1
                && child.current()?.status(false) == TurnStatus::Completed
        );
        runner::run(&home, &mut restored, &client).await?;
        runner::run(&home, &mut restored, &client).await?;
        let requests = model.join()?;
        ensure!(requests.len() == 3);
        let delivered: Vec<_> = requests[2]["messages"]
            .as_array()
            .context("messages")?
            .iter()
            .filter(|m| m["role"] == "tool")
            .collect();
        ensure!(
            delivered.len() == 1
                && delivered[0]["tool_call_id"] == "spawn"
                && delivered[0]["content"]
                    .as_str()
                    .is_some_and(|s| s.contains("child evidence"))
        );
    }
    Ok(())
}

#[tokio::test]
async fn pre_header_disconnect_keeps_the_already_durable_wire_correlation() -> Result<()> {
    use std::io::{BufRead, BufReader};
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let Work {
        _dir,
        home,
        mut thread,
        client,
    } = Work::new(&endpoint)?;
    let checkpoint = _dir
        .path()
        .join("bro/threads")
        .join(format!("{}.json", thread.thread_id));
    let peer = std::thread::spawn(move || -> Result<_> {
        let (socket, _) = listener.accept()?;
        socket.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
        let mut input = BufReader::new(socket);
        let mut headers = String::new();
        while !headers.ends_with("\r\n\r\n") {
            ensure!(input.read_line(&mut headers)? != 0);
        }
        let headers = headers.to_ascii_lowercase();
        let id = headers
            .lines()
            .find_map(|s| s.strip_prefix("x-bitrouter-request-id: "))
            .context("missing wire correlation")?;
        let saved: serde_json::Value = serde_json::from_slice(&fs::read(checkpoint)?)?;
        assert_eq!(saved["turns"][0]["steps"][0]["caller_request_id"], id);
        assert_eq!(
            saved["turns"][0]["steps"][0]["state"]["record"],
            "model_request"
        );
        Ok(uuid::Uuid::parse_str(id)?)
        // Drop the socket before sending response headers.
    });
    assert!(runner::run(&home, &mut thread, &client).await.is_err());
    let wire_id = peer
        .join()
        .map_err(|_| anyhow::anyhow!("gateway fixture failed"))??;
    let mut loaded = home.load(thread.thread_id)?;
    let step = &loaded.current()?.steps[0];
    assert_eq!(step.caller_request_id, Some(wire_id));
    assert!(step.request_id.is_none() && step.execution_id.is_none());
    assert_eq!(
        loaded.current()?.status(false),
        TurnStatus::RecoveryRequired
    );
    assert!(runner::run(&home, &mut loaded, &client).await.is_err());
    assert_eq!(loaded.current()?.steps.len(), 1);
    Ok(())
}

#[tokio::test]
async fn missing_or_conflicting_success_echo_cannot_authorize_model_output() -> Result<()> {
    for echo in ["", "X-BitRouter-Request-Id: different\r\n"] {
        let reply = call_stream(true).replace("X-BitRouter-Request-Id: <caller-id>\r\n", echo);
        let mock = Mock::start(vec![reply])?;
        let Work {
            _dir,
            home,
            mut thread,
            client,
        } = Work::new(&mock.endpoint)?;
        assert!(runner::run(&home, &mut thread, &client).await.is_err());
        let loaded = home.load(thread.thread_id)?;
        assert_eq!(
            loaded.current()?.status(false),
            TurnStatus::RecoveryRequired
        );
        assert!(loaded.current()?.pending().is_none());
        assert!(loaded.current()?.steps[0].caller_request_id.is_some());
        assert_eq!(mock.join()?.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn verification_result_publication_never_repeats_the_shell() -> Result<()> {
    use crate::verification::{Check, State};
    struct LoseResult<'a> {
        home: &'a Home,
        published: bool,
    }
    impl runner::Journal for LoseResult<'_> {
        fn save(&self, thread: &Thread) -> Result<()> {
            let result = thread.current()?.evidence().is_some();
            if !result || self.published {
                self.home.save(thread)?;
            }
            ensure!(!result, "lost result acknowledgement");
            Ok(())
        }
    }
    for published in [false, true] {
        let reply = stream(chunk(json!({"content":"Ready."}), Some("stop")) + "data: [DONE]\n\n");
        let mock = Mock::start(vec![reply])?;
        let Work {
            _dir,
            home,
            mut thread,
            client,
        } = Work::new(&mock.endpoint)?;
        let command = "printf x >> marker";
        thread.configure_check(command.into());
        let mut check = Check::new(command)?;
        check.approval.approved = Some(true);
        thread.current_mut()?.check = Some(check);
        home.save(&thread)?;
        let result = runner::drive(
            &LoseResult {
                home: &home,
                published,
            },
            &mut thread,
            &client,
            &tokio_util::sync::CancellationToken::new(),
            None,
            0,
        )
        .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("lost result acknowledgement")
        );
        if !thread.workspace.join("marker").exists() {
            // This platform refused the check before dispatch, so no intent was written.
            ensure!(
                thread
                    .current()?
                    .evidence()
                    .and_then(|e| e.error.as_deref())
                    .is_some_and(|s| s.contains("sandbox unavailable")),
                "missing verification side effect"
            );
            assert_eq!(mock.join()?.len(), 1);
            continue;
        }
        let mut recovered = home.load(thread.thread_id)?;
        let check = recovered.current()?.check.as_ref().context("check")?;
        if published {
            assert!(matches!(check.state, State::Result { .. }));
        } else {
            assert!(matches!(check.state, State::Intent));
        }
        for _ in 0..2 {
            let result = runner::run(&home, &mut recovered, &client).await;
            assert_eq!(result.is_ok(), published);
            assert_eq!(fs::read_to_string(recovered.workspace.join("marker"))?, "x");
            assert_eq!(
                recovered.current()?.status(false),
                if published {
                    TurnStatus::Completed
                } else {
                    TurnStatus::RecoveryRequired
                }
            );
            recovered = home.load(thread.thread_id)?;
        }
        if !published {
            runner::cancel_turn(&home, &mut recovered)?;
            assert!(recovered.start("new".into(), None).is_err());
        }
        assert_eq!(mock.join()?.len(), 1);
    }
    Ok(())
}
