//! One ACP connection to the same locked, durable worker used by `bro thread`.
mod turn;
mod wire;

use super::handler::{manager_input, rejection};
use crate::checkpoint::{Home, Thread};
use agent_client_protocol_schema::{v1, v2};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::harness::agent::ToolMode;
use bitrouter_sdk::gateway::GatewayClient;
use serde::Deserialize;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinSet, LocalSet};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Clone)]
struct Connection {
    home: Rc<Home>,
    client: GatewayClient,
    endpoint: String,
    workspace: Option<PathBuf>,
    model: String,
    mode: ToolMode,
    output: mpsc::Sender<Value>,
    disconnected: CancellationToken,
    active: Rc<RefCell<HashMap<Uuid, CancellationToken>>>,
    permissions: Rc<RefCell<HashMap<String, oneshot::Sender<bool>>>>,
}

impl Connection {
    fn frame(&self, frame: Value) -> Result<()> {
        ensure!(
            !self.disconnected.is_cancelled(),
            "ACP manager disconnected"
        );
        if self.output.try_send(frame).is_err() {
            // An observer that falls behind can replay the checkpoint. Its
            // output capacity must never decide a paid model or tool outcome.
            self.disconnected.cancel();
            anyhow::bail!("ACP manager must reconnect to replay history");
        }
        Ok(())
    }

    fn reply(&self, id: &Value, value: Value) -> Result<()> {
        self.frame(json!({"jsonrpc":"2.0","id":id,"result":value}))
    }

    fn update(&self, session: Uuid, update: Value) {
        let _ = self.frame(json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":session,"update":update}}));
    }

    fn load(&self, id: Uuid) -> Result<Thread> {
        let thread = self.home.load(id)?;
        ensure!(
            thread.endpoint == self.endpoint,
            "session is bound to a different gateway"
        );
        thread.verify_workspace()?;
        ensure!(
            self.workspace
                .as_ref()
                .is_none_or(|root| thread.workspace.starts_with(root)),
            "session outside configured workspace"
        );
        ensure!(
            !thread.managed() && thread.parent_id.is_none(),
            "session requires its managed worker"
        );
        ensure!(
            self.mode != ToolMode::ReadOnly || thread.tool_mode == self.mode,
            "session has a different tool grant"
        );
        Ok(thread)
    }
}

pub(super) async fn run(
    home: &Path,
    workspace: Option<&Path>,
    endpoint: &str,
    token: Option<&str>,
    model: &str,
    read_only: bool,
) -> Result<()> {
    ensure!(!model.trim().is_empty(), "native model required");
    let (output, frames) = mpsc::channel(128);
    let connection = Connection {
        home: Rc::new(Home::open(home)?),
        client: GatewayClient::new(endpoint, token)?,
        endpoint: endpoint.into(),
        workspace: workspace.map(Path::to_path_buf),
        model: model.into(),
        mode: if read_only {
            ToolMode::ReadOnly
        } else {
            ToolMode::Coding
        },
        output,
        disconnected: CancellationToken::new(),
        active: Default::default(),
        permissions: Default::default(),
    };
    LocalSet::new().run_until(serve(connection, frames)).await
}

async fn serve(connection: Connection, frames: mpsc::Receiver<Value>) -> Result<()> {
    let mut manager = manager_input()?;
    let mut workers = JoinSet::new();
    let mut sessions = HashSet::new();
    let mut version = None;
    let mut writer = tokio::task::spawn_local(write_frames(frames));
    let mut writer_joined = false;
    let shutdown = async {
        #[cfg(unix)]
        {
            let mut signal =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            tokio::select! { result = tokio::signal::ctrl_c() => result?, _ = signal.recv() => {} }
        }
        #[cfg(not(unix))]
        tokio::signal::ctrl_c().await?;
        Ok::<_, std::io::Error>(())
    };
    tokio::pin!(shutdown);
    let result = loop {
        tokio::select! {
            signal = &mut shutdown => {
                for turn in connection.active.borrow().values() { turn.cancel(); }
                break signal.context("waiting for shutdown");
            }
            result = &mut writer => {
                writer_joined = true;
                break result.context("ACP writer stopped")?;
            }
            _ = connection.disconnected.cancelled() => break Err(anyhow::anyhow!("ACP output requires reconnect")),
            result = workers.join_next(), if !workers.is_empty() => {
                if let Some(Err(error)) = result { break Err(error.into()); }
            }
            frame = manager.recv() => match frame {
                None => break Ok(()),
                Some(Err(error)) => break Err(error),
                Some(Ok(frame)) => {
                    let result = handle(&connection, &mut version, &mut sessions, &mut workers, &frame).await;
                    if let Err(error) = result && let Some(id) = frame.get("id")
                        && let Err(send) = connection.frame(rejection(id, -32602, &error.to_string())) {
                        break Err(send);
                    }
                }
            }
        }
    };
    // Finish admitted model work to its next durable boundary. A lost
    // permission waiter leaves its original approval pending for a new peer.
    connection.disconnected.cancel();
    while let Some(joined) = workers.join_next().await {
        joined?;
    }
    drop(connection);
    if !writer_joined {
        writer.await??;
    }
    result
}

async fn write_frames(mut frames: mpsc::Receiver<Value>) -> Result<()> {
    // A full OS pipe can block forever. Keep stdout off the Tokio blocking
    // pool so a stalled manager cannot prevent runtime shutdown.
    use std::io::Write;
    let (pipe, mut incoming) = mpsc::channel::<(Value, oneshot::Sender<std::io::Result<()>>)>(1);
    std::thread::Builder::new()
        .name("acp-manager-stdout".into())
        .spawn(move || {
            let mut stdout = std::io::BufWriter::new(std::io::stdout().lock());
            while let Some((frame, ack)) = incoming.blocking_recv() {
                let result = serde_json::to_writer(&mut stdout, &frame)
                    .map_err(std::io::Error::from)
                    .and_then(|_| stdout.write_all(b"\n"))
                    .and_then(|_| stdout.flush());
                let failed = result.is_err();
                if ack.send(result).is_err() || failed {
                    break;
                }
            }
        })?;
    while let Some(frame) = frames.recv().await {
        let (ack, written) = oneshot::channel();
        pipe.send((frame, ack)).await.context("ACP stdout closed")?;
        tokio::time::timeout(Duration::from_secs(5), written)
            .await
            .context("ACP peer stopped reading")???;
    }
    Ok(())
}

fn id(params: &Value) -> Result<Uuid> {
    Ok(Uuid::parse_str(
        params["sessionId"].as_str().context("sessionId required")?,
    )?)
}

fn cwd(params: &Value) -> Result<PathBuf> {
    for name in ["additionalDirectories", "mcpServers"] {
        if let Some(value) = params.get(name) {
            ensure!(
                value.as_array().is_some_and(Vec::is_empty),
                "unsupported {name}"
            );
        }
    }
    let path = Path::new(
        params["cwd"]
            .as_str()
            .context("absolute session cwd required")?,
    );
    ensure!(
        path.is_absolute() && path.is_dir(),
        "absolute session directory required"
    );
    Ok(path.canonicalize()?)
}

async fn handle(
    c: &Connection,
    version: &mut Option<u64>,
    attached: &mut HashSet<Uuid>,
    workers: &mut JoinSet<()>,
    frame: &Value,
) -> Result<()> {
    let params = &frame["params"];
    let Some(method) = frame["method"].as_str() else {
        if let Some(id) = frame["id"].as_str()
            && let Some(answer) = c.permissions.borrow_mut().remove(id)
        {
            // Every reply settles this callback. Only an explicit selection can
            // grant the tool, including when a buggy manager sends both fields.
            let approved = frame.get("error").is_none()
                && frame
                    .pointer("/result/outcome/outcome")
                    .and_then(Value::as_str)
                    == Some("selected")
                && frame
                    .pointer("/result/outcome/optionId")
                    .and_then(Value::as_str)
                    == Some("allow_once");
            let _ = answer.send(approved);
        }
        return Ok(());
    };
    if method == "session/cancel" {
        let session = id(params)?;
        ensure!(attached.contains(&session), "session is not attached");
        if let Some(token) = c.active.borrow().get(&session) {
            token.cancel();
        }
        if let Some(rpc) = frame.get("id") {
            c.reply(rpc, json!({}))?;
        }
        return Ok(());
    }
    let Some(rpc) = frame.get("id") else {
        return Ok(());
    };
    if method == "initialize" {
        ensure!(version.is_none(), "already initialized");
        let v = params["protocolVersion"]
            .as_u64()
            .context("protocolVersion required")?;
        ensure!(matches!(v, 1 | 2), "unsupported ACP version");
        if v == 1 {
            v1::InitializeRequest::deserialize(params)?;
        } else {
            v2::InitializeRequest::deserialize(params)?;
        }
        *version = Some(v);
        return c.reply(rpc, wire::initialize(v));
    }
    let Some(v) = *version else {
        return c.frame(rejection(rpc, -32002, "initialize required"));
    };
    match method {
        "session/new" => {
            ensure!(
                params.pointer("/_meta/bitrouter/idempotencyKey").is_none(),
                "keyed session creation unavailable"
            );
            let cwd = cwd(params)?;
            ensure!(
                c.workspace
                    .as_ref()
                    .is_none_or(|root| cwd.starts_with(root)),
                "session outside configured workspace"
            );
            let thread = Thread::new(cwd, c.endpoint.clone(), c.model.clone(), None, None, c.mode)?;
            let _guard = c.home.worker(thread.thread_id)?;
            c.home.admit(&thread)?;
            attached.insert(thread.thread_id);
            c.reply(rpc, json!({"sessionId":thread.thread_id}))
        }
        "session/list" => {
            ensure!(params["cursor"].is_null(), "unsupported directory cursor");
            let requested = params
                .get("cwd")
                .filter(|v| !v.is_null())
                .map(|_| cwd(params))
                .transpose()?;
            let mut sessions = Vec::new();
            for session in c.home.ids()? {
                let thread = c.home.load(session)?;
                if c.workspace
                    .as_ref()
                    .is_none_or(|root| thread.workspace.starts_with(root))
                    && requested
                        .as_ref()
                        .is_none_or(|path| path == &thread.workspace)
                    && thread.endpoint == c.endpoint
                    && thread.parent_id.is_none()
                    && !thread.managed()
                    && (c.mode != ToolMode::ReadOnly || thread.tool_mode == c.mode)
                {
                    sessions.push(json!({"sessionId":session,"cwd":thread.workspace}));
                }
            }
            c.reply(rpc, json!({"sessions":sessions}))
        }
        "session/load" if v == 2 => c.frame(rejection(rpc, -32601, "use session/resume")),
        "session/load" | "session/resume" => {
            let session = id(params)?;
            let guard = c.home.worker(session)?;
            let thread = c.load(session)?;
            ensure!(
                cwd(params)? == thread.workspace,
                "session workspace differs from its grant"
            );
            ensure!(
                !c.active.borrow().contains_key(&session),
                "session is active"
            );
            let cursor = &params["replayFrom"];
            ensure!(
                cursor.is_null() || cursor["type"] == "start",
                "unsupported replay cursor"
            );
            if method == "session/load" || !cursor.is_null() {
                wire::history(c, v, &thread).await?;
            }
            attached.insert(session);
            let latest = thread
                .turns
                .last()
                .map(|t| json!({"turnId":t.turn_id, "status":t.status(false)}));
            c.reply(
                rpc,
                json!({"_meta":{"bitrouter":{"version":1, "latestTurn":latest}}}),
            )?;
            if thread
                .turns
                .last()
                .is_some_and(|turn| turn.outcome.is_none())
            {
                launch(c, v, thread, guard, None, workers)?;
            }
            Ok(())
        }
        "session/prompt" => {
            let session = id(params)?;
            ensure!(attached.contains(&session), "session is not attached");
            let (guard, running) = c.home.observe_worker(session)?;
            let mut thread = c.load(session)?;
            let prompt = wire::prompt(v, params)?;
            let key = params
                .pointer("/_meta/bitrouter/idempotencyKey")
                .map(|v| v.as_str().context("invalid idempotency key"))
                .transpose()?;
            if let Some(key) = key
                && let Some(old) = thread.accepted_request(key, &prompt)?
            {
                let turn = thread
                    .turns
                    .iter()
                    .find(|t| t.turn_id == old)
                    .context("missing admitted turn")?;
                return c.reply(rpc, wire::prompt_result(v, turn)?);
            }
            ensure!(!running, "another process owns this thread");
            thread.start(prompt, key.map(str::to_owned))?;
            c.home.admit(&thread)?;
            let turn = thread.current()?;
            c.update(
                session,
                wire::message(v, "user", turn.turn_id, &turn.prompt, true),
            );
            if v == 2 {
                c.update(session, wire::state(turn));
                c.reply(rpc, wire::prompt_result(v, thread.current()?)?)?;
            }
            launch(c, v, thread, guard, (v == 1).then(|| rpc.clone()), workers)
        }
        _ => c.frame(rejection(rpc, -32601, "method unavailable")),
    }
}

fn launch(
    c: &Connection,
    version: u64,
    mut thread: Thread,
    guard: File,
    reply: Option<Value>,
    workers: &mut JoinSet<()>,
) -> Result<()> {
    let c = c.clone();
    let session = thread.thread_id;
    let cancel = CancellationToken::new();
    ensure!(
        !c.active.borrow().contains_key(&session),
        "session already active"
    );
    c.active.borrow_mut().insert(session, cancel.clone());
    workers.spawn_local(async move {
        let _guard = guard;
        let result = turn::drive(&c, version, &mut thread, &cancel).await;
        c.active.borrow_mut().remove(&session);
        if version == 2 && result.is_err() {
            c.update(session,json!({"sessionUpdate":"state_update","state":"requires_action"}));
        }
        if let Some(id) = reply {
            let response = result.and_then(|_| {
                let turn = thread.current()?;
                if cancel.is_cancelled() {
                    Ok(json!({"stopReason":"cancelled","_meta":{"bitrouter":{"turnId":turn.turn_id,"status":turn.status(false)}}}))
                } else {
                    wire::prompt_result(version, turn)
                }
            });
            let frame = match response {
                Ok(value) => json!({"jsonrpc":"2.0","id":id,"result":value}),
                Err(_) => rejection(&id, -32603, "native turn requires inspection"),
            };
            if c.frame(frame).is_err() {
                c.disconnected.cancel();
            }
        }
    });
    Ok(())
}
