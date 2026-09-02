//! Agent-process transport, and the thread-owned connection the engine uses.
//!
//! [`AgentProcess`] is the ACP component that spawns one harness child and
//! speaks to it over stdio: stripped inherited markers, config-authored env
//! precedence, stderr draining, process-group teardown, and a prompt that
//! fails rather than hangs when the child dies. Both the connection-level
//! [`Controller`](crate::acp::controller::Controller) and the shared
//! [`AcpClient`](crate::acp::client::AcpClient) reach a harness through it.
//!
//! [`UpstreamConnection`] is an [`AcpClient`] over an [`AgentProcess`], plus a
//! dedicated thread running its own multi-thread tokio runtime — the shape
//! [`engine::Session`](crate::acp::engine::Session) was built against, kept
//! until that type retires. Every method delegates. New code connects an
//! [`AcpClient`] directly and drives it on the caller's runtime.

use std::collections::HashMap;
use std::path::PathBuf;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    InitializeRequest, InitializeResponse, McpServer, PromptRequest, PromptResponse, SessionUpdate,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectTo, ConnectionTo};
use futures::Stream;
use futures::channel::oneshot;
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::acp::client::{AcpClient, ClientOptions, PendingPermission, SessionIds};
use crate::acp::telemetry::SharedContextUsage;
use crate::acp::translate::SessionUpdateKind;

/// The ACP-over-stdio transport wired to a spawned agent child.
type AgentTransport =
    ByteStreams<Compat<tokio::process::ChildStdin>, Compat<tokio::process::ChildStdout>>;

/// Reusable ACP agent-process connector.
///
/// It carries the established child policy: stripped inherited markers,
/// config-authored env precedence, stderr draining, process-group teardown,
/// and prompt failure when the child dies.
#[derive(Clone)]
pub struct AgentProcess {
    command: String,
    args: Vec<String>,
    env: HashMap<String, String>,
    strip_inherited_env: Vec<String>,
}

impl std::fmt::Debug for AgentProcess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentProcess")
            .field("command", &"[CONFIGURED]")
            .field("arg_count", &self.args.len())
            .field("env_names", &self.env.keys().collect::<Vec<_>>())
            .field("env_values", &"[REDACTED]")
            .field("strip_inherited_env", &self.strip_inherited_env)
            .finish()
    }
}

impl AgentProcess {
    /// Construct a child connector from a configured stdio invocation.
    pub fn new(
        command: impl Into<String>,
        args: Vec<String>,
        env: HashMap<String, String>,
    ) -> Self {
        Self {
            command: command.into(),
            args,
            env,
            strip_inherited_env: Vec::new(),
        }
    }

    /// Remove additional ambient variables before applying the configured
    /// child environment. Explicit values in `env` still win.
    #[must_use]
    pub fn strip_inherited_env(mut self, names: Vec<String>) -> Self {
        self.strip_inherited_env = names;
        self
    }
}

impl ConnectTo<Client> for AgentProcess {
    async fn connect_to(
        self,
        client: impl ConnectTo<Agent>,
    ) -> Result<(), agent_client_protocol::Error> {
        let (transport, child) = spawn_agent_process(
            &self.command,
            &self.args,
            &self.env,
            &self.strip_inherited_env,
        )
        .map_err(agent_client_protocol::util::internal_error)?;
        let (kill_tx, kill_rx) = oneshot::channel::<()>();
        let (dead_tx, mut dead_rx) = oneshot::channel::<()>();
        let (done_tx, done_rx) = oneshot::channel::<()>();
        spawn_child_reaper(child, kill_rx, dead_tx, done_tx);

        let protocol = ConnectTo::<Client>::connect_to(transport, client);
        tokio::pin!(protocol);
        let result = tokio::select! {
            result = &mut protocol => result,
            _ = &mut dead_rx => Err(agent_client_protocol::util::internal_error(
                "agent process exited while the ACP controller was connected",
            )),
        };
        let _ = kill_tx.send(());
        if tokio::time::timeout(std::time::Duration::from_secs(2), done_rx)
            .await
            .is_err()
        {
            tracing::warn!("agent child reaper did not confirm within 2s");
        }
        result
    }
}

/// A live ACP `Client` connection to one agent child, driven on its own thread.
///
/// `spawn` runs `initialize` only; the session itself is created later via
/// [`new_session`](Self::new_session) so the caller can relay a manager's
/// `cwd` and `mcpServers` into it instead of fabricating them at launch.
pub struct UpstreamConnection {
    /// The shared client. Every method here delegates to it.
    client: AcpClient,
    /// Keeps the driver thread alive for the connection's lifetime.
    _thread: std::thread::JoinHandle<()>,
}

/// Inherited env vars an agent child must never see. The substrate launches
/// an **independent** agent session, so a leaked "you are running inside
/// another agent" marker is categorically false for the child — and actively
/// harmful: Claude Code's nested-session guard refuses to start when
/// `CLAUDECODE` leaks through ("Claude Code cannot be launched inside another
/// Claude Code session"), which broke spawning `claude-acp` from any
/// bitrouter run inside a Claude session. Removal happens **before** the
/// transport/launch env overlay is applied, so an explicitly configured
/// `env:` value still wins.
const STRIPPED_INHERITED_ENV: &[&str] = &["CLAUDECODE"];

/// Build the agent child's `Command`: stripped inherited markers, then the
/// caller's env overlay, piped stdio, and (unix) its own process group so
/// teardown can kill the whole wrapper chain (`npx → node`, `uvx → python`).
fn agent_command(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
    strip_inherited_env: &[String],
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(command);
    for key in STRIPPED_INHERITED_ENV
        .iter()
        .copied()
        .chain(strip_inherited_env.iter().map(String::as_str))
    {
        cmd.env_remove(key);
    }
    cmd.args(args)
        .envs(env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Belt-and-braces: the reaper is the real teardown; this covers the
        // child handle being dropped without it ever running.
        .kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    cmd
}

/// Spawn `command args` with `env` applied, wired for ACP over stdio.
///
/// The child is made **its own process-group leader** (unix) so teardown can
/// kill the whole tree: agents are commonly wrapper chains (`npx → node`,
/// `uvx → python`), and killing only the immediate child orphans the real
/// agent — the process re-parents to pid 1 and does not reliably exit on
/// stdin EOF. Must run inside a tokio runtime (both call sites do). Shared by
/// [`AgentProcess`] and [`health_check`] so both paths spawn identically.
///
/// The child's stderr is **captured**, not inherited, and re-emitted line by
/// line through `tracing` under the `acp::agent` target. Inheriting it wrote
/// the agent's diagnostics straight at the terminal's fd 2, where they raced
/// an inline TUI's drawing and could not be captured at all. Going through
/// `tracing` means whatever subscriber the binary installed decides where the
/// lines land, so the agent's log and the substrate's own interleave into one
/// destination in the order they happened.
fn spawn_agent_process(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
    strip_inherited_env: &[String],
) -> anyhow::Result<(AgentTransport, tokio::process::Child)> {
    let mut cmd = agent_command(command, args, env, strip_inherited_env);
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawning agent '{command}': {e}"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("agent child has no stdin pipe"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("agent child has no stdout pipe"))?;
    if let Some(stderr) = child.stderr.take() {
        forward_agent_stderr(command.to_string(), stderr);
    }
    Ok((
        ByteStreams::new(stdin.compat_write(), stdout.compat()),
        child,
    ))
}

/// Drain the agent child's stderr into `tracing`, one event per line, under
/// the `acp::agent` target.
///
/// Detached rather than joined: the reader ends on its own when the pipe
/// closes at child exit, and making teardown wait on it would let a child
/// that never closes stderr stall the shutdown path.
///
/// Lines are emitted at `info`. Agents write ordinary progress there as well
/// as failures, so `warn` would cry wolf; a subscriber that wants them quieter
/// can filter the target.
fn forward_agent_stderr(command: String, stderr: tokio::process::ChildStderr) {
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        // A read error means the pipe is gone; there is nothing to report it
        // to that would not itself be this log.
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::info!(target: "acp::agent", agent = %command, "{line}");
        }
    });
}

/// SIGKILL the child's whole process group (it is its own group leader via
/// `process_group(0)`). `ESRCH` just means everyone is already gone. On
/// non-unix targets group semantics don't apply; `kill_on_drop` covers the
/// direct child there.
///
/// Goes through `rustix`'s safe wrapper rather than a raw `libc::killpg`:
/// this crate is `#![forbid(unsafe_code)]`, and that guarantee is worth more
/// than saving a dependency already present in the build graph.
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    // A pid too large for the platform's pid type, or a group that has already
    // exited (`ESRCH`), are both no-ops by design.
    if let Ok(raw) = i32::try_from(pid)
        && let Some(pid) = rustix::process::Pid::from_raw(raw)
    {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

/// Own the agent child for its whole life. Two exits:
///
/// - the child dies on its own (agent crash/EOF) → SIGKILL its process group
///   anyway (a dead wrapper can leave the real agent running), then signal
///   `dead_tx` so the command loop ends;
/// - a kill order arrives — `kill_rx` firing **or its sender dropping**
///   (teardown, caller panic, `health_check` timeout: all collapse to the
///   same cleanup) → group-kill, then reap the direct child.
///
/// Either way `done_tx` confirms once the group is killed and the child
/// reaped, so teardown can wait for it before dropping the runtime.
fn spawn_child_reaper(
    mut child: tokio::process::Child,
    kill_rx: oneshot::Receiver<()>,
    dead_tx: oneshot::Sender<()>,
    done_tx: oneshot::Sender<()>,
) {
    tokio::spawn(async move {
        let pid = child.id();
        tokio::select! {
            _ = child.wait() => {
                if let Some(pid) = pid {
                    kill_process_group(pid);
                }
            }
            // Resolves on an explicit send AND on sender drop.
            _ = kill_rx => {
                if let Some(pid) = pid {
                    kill_process_group(pid);
                }
                let _ = child.wait().await;
            }
        }
        let _ = dead_tx.send(());
        let _ = done_tx.send(());
    });
}

impl UpstreamConnection {
    /// Spawn the agent process, connect as an ACP `Client`, and run
    /// `initialize`. Returns once the handshake completes and the command loop
    /// is resident, or an error if spawn/handshake failed.
    pub async fn spawn(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
    ) -> anyhow::Result<Self> {
        Self::spawn_with_stripped_env(command, args, env, &[]).await
    }

    /// Like [`spawn`](Self::spawn), but first removes `strip_inherited_env`
    /// names from the inherited environment, so an isolated caller can stop
    /// ambient credentials crossing into the agent while still letting a
    /// deliberately configured `env` entry win.
    pub async fn spawn_with_stripped_env(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        strip_inherited_env: &[String],
    ) -> anyhow::Result<Self> {
        let process = AgentProcess::new(command.to_string(), args.to_vec(), env.clone())
            .strip_inherited_env(strip_inherited_env.to_vec());
        // The deadline stays with `engine::Session` on this path; the shared
        // client enforces its own only where a caller asked for one.
        let (ready, driver) = AcpClient::wire(process, ClientOptions::default());
        let thread = std::thread::Builder::new()
            .name("bitrouter-acp-up".to_string())
            .spawn(move || {
                match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                {
                    // Dropping `driver` unpolled closes the handshake channel,
                    // so `ready` below reports the failure rather than hanging.
                    Err(e) => tracing::error!(error = %e, "failed to start the ACP runtime"),
                    Ok(rt) => rt.block_on(driver),
                }
            })?;

        Ok(Self {
            client: ready.await?,
            _thread: thread,
        })
    }

    /// Create the upstream session: `session/new` with `cwd` and the given MCP
    /// servers. Returns the minted wire identity.
    pub async fn new_session(
        &self,
        cwd: PathBuf,
        mcp_servers: Vec<McpServer>,
    ) -> anyhow::Result<SessionIds> {
        self.client.new_session(cwd, mcp_servers).await
    }

    /// The upstream agent's `initialize` response, captured at handshake.
    pub fn upstream_init(&self) -> &InitializeResponse {
        self.client.upstream_init()
    }

    /// Handle to the latest context-window usage reported by the upstream.
    pub fn context_usage(&self) -> SharedContextUsage {
        self.client.context_usage()
    }

    /// Subscribe to the stream of translated `session/update` notifications.
    /// **Lossy under lag**, as documented on the shared client.
    pub fn subscribe_updates(
        &self,
    ) -> std::pin::Pin<Box<dyn Stream<Item = SessionUpdateKind> + Send>> {
        self.client.subscribe_updates()
    }

    /// Subscribe to the stream of **raw** ACP `session/update` notifications,
    /// untranslated. **Lossy under lag**, as documented on the shared client.
    pub fn subscribe_raw_updates(
        &self,
    ) -> std::pin::Pin<Box<dyn Stream<Item = SessionUpdate> + Send>> {
        self.client.subscribe_raw_updates()
    }

    /// Take the stream of pending permission requests. Single-consumer: the
    /// first call returns the receiver; later calls return an empty stream.
    pub fn subscribe_permissions(
        &self,
    ) -> std::pin::Pin<Box<dyn Stream<Item = PendingPermission> + Send>> {
        self.client.subscribe_permissions()
    }

    /// Send a typed `PromptRequest` and return the typed `PromptResponse`.
    pub async fn prompt_typed(&self, req: PromptRequest) -> anyhow::Result<PromptResponse> {
        self.client.prompt_typed(req).await
    }

    /// Text convenience over [`prompt_typed`](Self::prompt_typed).
    pub async fn prompt(&self, session_id: &str, text: &str) -> anyhow::Result<PromptResponse> {
        self.client.prompt(session_id, text).await
    }

    /// Send a `session/cancel` notification for `session_id`.
    pub async fn cancel(&self, session_id: &str) -> anyhow::Result<()> {
        self.client.cancel(session_id).await
    }

    /// Tear the connection down deterministically, killing the agent child.
    /// Idempotent.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.client.shutdown().await
    }
}

/// How long `health_check` waits for `initialize` before declaring the agent
/// unhealthy. Generous enough for a cold npm start; tight enough to keep
/// `bitrouter agents check` snappy when an agent hangs.
const HEALTH_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Spawn the agent, run ACP `initialize` only (no session), return elapsed on
/// success or an error string. Used by `bitrouter agents check`.
///
/// `env` is applied to the spawned child process (same plumbing as
/// [`UpstreamConnection::spawn`]) so an agent that needs API-key vars answers
/// the health-check.
///
/// Tears the connection down (drops) immediately after `initialize` succeeds
/// or after `HEALTH_CHECK_TIMEOUT` (10s) elapses.
pub async fn health_check(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> Result<std::time::Duration, String> {
    tokio::time::timeout(HEALTH_CHECK_TIMEOUT, health_check_inner(command, args, env))
        .await
        .unwrap_or_else(|_| {
            Err(format!(
                "initialize timed out after {HEALTH_CHECK_TIMEOUT:?}"
            ))
        })
}

async fn health_check_inner(
    command: &str,
    args: &[String],
    env: &HashMap<String, String>,
) -> Result<std::time::Duration, String> {
    let (transport, child) =
        spawn_agent_process(command, args, env, &[]).map_err(|e| format!("spawn failed: {e}"))?;
    // Reaper teardown: explicitly ordered (and awaited) below so the group is
    // gone before `agents check` moves on; if the caller's 10s timeout cancels
    // this future mid-await instead, `kill_tx` drops and the receiver resolves
    // all the same — the reaper group-kills + reaps on both paths.
    let (kill_tx, kill_rx) = oneshot::channel::<()>();
    let (dead_tx, _dead_rx) = oneshot::channel::<()>();
    let (done_tx, done_rx) = oneshot::channel::<()>();
    spawn_child_reaper(child, kill_rx, dead_tx, done_tx);

    let (result_tx, result_rx) =
        futures::channel::oneshot::channel::<Result<std::time::Duration, String>>();
    let result_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(result_tx)));
    let closure_result_tx = result_tx.clone();

    let connect_result = agent_client_protocol::Client
        .builder()
        .name("bitrouter-health-check")
        .connect_with(transport, |connection: ConnectionTo<Agent>| async move {
            let started = std::time::Instant::now();
            let init_result = connection
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await;
            let outcome = match init_result {
                Ok(_) => Ok(started.elapsed()),
                Err(e) => Err(format!("initialize failed: {e}")),
            };
            let tx = closure_result_tx
                .lock()
                .ok()
                .and_then(|mut guard| guard.take());
            if let Some(tx) = tx {
                let _ = tx.send(outcome);
            }
            // Return Ok so the connection closes cleanly (no command loop needed).
            Ok(())
        })
        .await;

    // The check is done either way: kill the agent's process group and wait
    // for the reap so no wrapper-chain grandchild outlives the CLI.
    let _ = kill_tx.send(());
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), done_rx).await;

    // If the result was already sent via the closure, use it. Otherwise surface
    // the connect-level error (spawn failed, process exited before initialize, etc.).
    match result_rx.await {
        Ok(outcome) => outcome,
        Err(_) => {
            // Closure never ran or never sent — surface the connect error.
            match connect_result {
                Ok(()) => Err("agent exited before initialize".to_string()),
                Err(e) => Err(format!("connect failed: {e}")),
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[test]
    fn stable_v1_initialize_keeps_numeric_wire_version() -> anyhow::Result<()> {
        let request = InitializeRequest::new(ProtocolVersion::V1);
        let wire = serde_json::to_value(request)?;

        assert_eq!(wire["protocolVersion"], serde_json::json!(1));
        assert!(wire.get("clientCapabilities").is_some());
        Ok(())
    }

    #[test]
    fn agent_process_debug_redacts_arguments_and_environment_values() {
        let process = AgentProcess::new(
            "/private/bin/agent-secret-path",
            vec!["--token=argument-secret".to_string()],
            HashMap::from([(
                "ANTHROPIC_AUTH_TOKEN".to_string(),
                "environment-secret".to_string(),
            )]),
        );
        let rendered = format!("{process:?}");
        assert!(!rendered.contains("argument-secret"), "{rendered}");
        assert!(!rendered.contains("environment-secret"), "{rendered}");
        assert!(!rendered.contains("agent-secret-path"), "{rendered}");
        assert!(rendered.contains("ANTHROPIC_AUTH_TOKEN"), "{rendered}");
    }

    #[test]
    fn agent_child_never_inherits_nested_session_markers() {
        let cmd = agent_command("echo", &[], &HashMap::new(), &[]);
        let removed: Vec<_> = cmd
            .as_std()
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert!(
            removed.contains(&"CLAUDECODE".to_string()),
            "CLAUDECODE must be stripped (claude's nested-session guard): {removed:?}"
        );

        // An explicitly configured env value still wins over the strip.
        let mut env = HashMap::new();
        env.insert("CLAUDECODE".to_string(), "1".to_string());
        let cmd = agent_command("echo", &[], &env, &[]);
        let explicit = cmd
            .as_std()
            .get_envs()
            .find(|(k, _)| k.to_string_lossy() == "CLAUDECODE")
            .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()));
        assert_eq!(explicit.as_deref(), Some("1"), "explicit config env wins");

        let stripped = vec!["BITROUTER_API_KEY".to_string()];
        let cmd = agent_command("echo", &[], &HashMap::new(), &stripped);
        let removed: Vec<_> = cmd
            .as_std()
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        assert!(removed.contains(&"BITROUTER_API_KEY".to_string()));

        let env = HashMap::from([("BITROUTER_API_KEY".to_string(), "explicit".to_string())]);
        let cmd = agent_command("echo", &[], &env, &stripped);
        let explicit = cmd
            .as_std()
            .get_envs()
            .find(|(key, _)| key.to_string_lossy() == "BITROUTER_API_KEY")
            .and_then(|(_, value)| value.map(|value| value.to_string_lossy().into_owned()));
        assert_eq!(explicit.as_deref(), Some("explicit"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn connects_initializes_and_prompts() {
        let script = r#"
            while read line; do
              id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
              case "$line" in
                *initialize*)   printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
                *session/new*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
                *session/prompt*) printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"u1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hi"}}}}\n';
                                  printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id";;
              esac
            done
        "#;
        let conn =
            UpstreamConnection::spawn("bash", &["-c".into(), script.into()], &HashMap::new())
                .await
                .expect("spawn");
        let ids = conn
            .new_session(std::path::PathBuf::from("/"), vec![])
            .await
            .expect("session/new");
        let usid = ids.acp_session_id;
        assert_eq!(usid, "u1");
        let mut updates = conn.subscribe_updates();
        let resp = conn.prompt(&usid, "do X").await.expect("prompt");
        assert!(format!("{resp:?}").contains("EndTurn"));
        let ev = updates.next().await.expect("update");
        assert!(format!("{ev:?}").contains("hi"), "unexpected: {ev:?}");
    }

    /// Safety invariant: dropping a [`PendingPermission`] without resolving it
    /// must make the upstream handler answer `Deny`, so the agent never hangs.
    ///
    /// The stub sends a `session/request_permission` whose only option is a
    /// `reject_once` kind (id `rej`). The test subscribes to permissions,
    /// receives the [`PendingPermission`], and **drops** it. `select_option`
    /// maps the defaulted `Deny` onto the `reject_once` option, so the client's
    /// response selects `rej`. The stub reads that response line, echoes the
    /// chosen optionId back as a `session/update`, and completes the prompt; the
    /// test asserts the echoed id is `rej`.
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_pending_permission_defaults_to_deny() {
        let script = r#"
            while read line; do
              id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
              case "$line" in
                *initialize*)   printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
                *session/new*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
                *session/prompt*)
                    # Ask for permission; the only option is a reject_once kind.
                    printf '{"jsonrpc":"2.0","id":"99","method":"session/request_permission","params":{"sessionId":"u1","toolCall":{"toolCallId":"tc1","title":"do thing"},"options":[{"optionId":"rej","name":"Reject","kind":"reject_once"}]}}\n'
                    # Read the client's permission response and echo its optionId.
                    read resp
                    chosen=$(echo "$resp" | sed -n 's/.*"optionId":"\([^"]*\)".*/\1/p')
                    printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"u1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"chose:%s"}}}}\n' "$chosen"
                    printf '{"jsonrpc":"2.0","id":"%s","result":{"stopReason":"end_turn"}}\n' "$id";;
              esac
            done
        "#;
        let conn =
            UpstreamConnection::spawn("bash", &["-c".into(), script.into()], &HashMap::new())
                .await
                .expect("spawn");
        let usid = conn
            .new_session(std::path::PathBuf::from("/"), vec![])
            .await
            .expect("session/new")
            .acp_session_id;
        let mut updates = conn.subscribe_updates();
        let mut perms = conn.subscribe_permissions();

        // Drive the prompt concurrently; it completes only after the permission
        // round-trip finishes.
        let prompt = tokio::spawn(async move { conn.prompt(&usid, "do X").await });

        // Receive the pending permission and DROP it without resolving.
        let pending = perms.next().await.expect("permission request");
        assert_eq!(pending.tool_call.fields.title.as_deref(), Some("do thing"));
        drop(pending);

        // The echoed update proves the client answered with the reject option.
        let mut saw_reject = false;
        for _ in 0..4 {
            if let Some(ev) = updates.next().await
                && format!("{ev:?}").contains("chose:rej")
            {
                saw_reject = true;
                break;
            }
        }
        assert!(
            saw_reject,
            "dropped permission did not default to Deny/reject"
        );

        let resp = prompt.await.expect("join").expect("prompt");
        assert!(format!("{resp:?}").contains("EndTurn"));
    }

    /// An explicit `shutdown` confirms teardown promptly, after which the
    /// command loop is gone: further commands fail fast instead of hanging.
    /// A second `shutdown` is an idempotent no-op.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_confirms_teardown_and_closes_loop() {
        let script = r#"
            while read line; do
              id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
              case "$line" in
                *initialize*)  printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
                *session/new*) printf '{"jsonrpc":"2.0","id":"%s","result":{"sessionId":"u1"}}\n' "$id";;
              esac
            done
        "#;
        let conn =
            UpstreamConnection::spawn("bash", &["-c".into(), script.into()], &HashMap::new())
                .await
                .expect("spawn");

        conn.shutdown().await.expect("shutdown confirms");

        // The loop is gone: a prompt fails fast on the closed command channel.
        let err = conn.prompt("u1", "x").await;
        assert!(err.is_err(), "prompt after shutdown must fail, got Ok");

        // Idempotent.
        conn.shutdown().await.expect("second shutdown is a no-op");
    }

    /// The whole process GROUP dies at shutdown, wrapper chains included.
    /// The agent is spawned as `bash → bash <inner script>` (mimicking
    /// `npx → node`): the INNER process writes its pid to a file, and after
    /// `shutdown` that pid must be gone — killing only the outer wrapper
    /// (the old ChildGuard behavior) left it orphaned on pid 1.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_kills_wrapper_chain_process_group() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("inner.pid");
        let inner = dir.path().join("inner.sh");
        std::fs::write(
            &inner,
            format!(
                r#"echo $$ > {pid}
while read line; do
  id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
  case "$line" in
    *initialize*) printf '{{"jsonrpc":"2.0","id":"%s","result":{{"protocolVersion":1}}}}\n' "$id";;
  esac
done
"#,
                pid = pid_file.display()
            ),
        )
        .expect("write inner script");

        // `; :` keeps the outer bash alive as a parent instead of exec-ing
        // the inner command (which would collapse the chain to one process).
        let outer = format!("bash {} ; :", inner.display());
        let conn = UpstreamConnection::spawn("bash", &["-c".into(), outer], &HashMap::new())
            .await
            .expect("spawn wrapper chain");

        // The inner (grand)child is alive and identified.
        let mut inner_pid = String::new();
        for _ in 0..100 {
            if let Ok(raw) = std::fs::read_to_string(&pid_file) {
                let raw = raw.trim().to_string();
                if !raw.is_empty() {
                    inner_pid = raw;
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(!inner_pid.is_empty(), "inner script never reported its pid");
        assert!(pid_alive(&inner_pid), "inner process should be alive");

        conn.shutdown().await.expect("shutdown");

        // The grandchild must die with the group, not linger orphaned.
        let mut gone = false;
        for _ in 0..100 {
            if !pid_alive(&inner_pid) {
                gone = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(
            gone,
            "wrapper-chain grandchild (pid {inner_pid}) survived shutdown"
        );
    }

    /// `kill -0` liveness probe.
    #[cfg(unix)]
    fn pid_alive(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// An agent that dies mid-session must not wedge the connection: the whole
    /// connection future is raced against the reaper's death signal (a
    /// ByteStreams transport EOF does NOT fail in-flight requests on its own),
    /// so a pending prompt fails fast instead of hanging forever.
    #[cfg(unix)]
    #[tokio::test]
    async fn agent_crash_fails_pending_commands_fast() {
        // Answers the handshake, lingers briefly (so `spawn` completes), then
        // dies with the prompt in flight.
        let script = r#"
            read line
            id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
            printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id"
            sleep 0.3
            exit 0
        "#;
        let conn =
            UpstreamConnection::spawn("bash", &["-c".into(), script.into()], &HashMap::new())
                .await
                .expect("spawn");

        // The child dies with this prompt unanswered. It must resolve to an
        // error promptly (bounded), never hang.
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            conn.prompt("u1", "anyone home?"),
        )
        .await;
        match outcome {
            Ok(result) => assert!(result.is_err(), "prompt to a dead agent must fail"),
            Err(_) => panic!("prompt to a dead agent hung instead of failing fast"),
        }
    }

    /// health_check: a stub that answers `initialize` → returns Ok with an
    /// elapsed duration.
    #[cfg(unix)]
    #[tokio::test]
    async fn health_check_succeeds_when_agent_answers_initialize() {
        let script = r#"
            while read line; do
              id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
              case "$line" in
                *initialize*) printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id";;
              esac
            done
        "#;
        let result = health_check("bash", &["-c".into(), script.into()], &HashMap::new()).await;
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
    }

    /// health_check: a stub that returns a JSON-RPC error for `initialize` →
    /// `Err(_)`. Uses a bash script that replies with an error immediately so
    /// the test does not hit the timeout.
    #[cfg(unix)]
    #[tokio::test]
    async fn health_check_fails_when_agent_returns_error() {
        let script = r#"
            while read line; do
              id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
              case "$line" in
                *initialize*) printf '{"jsonrpc":"2.0","id":"%s","error":{"code":-32600,"message":"not supported"}}\n' "$id";;
              esac
            done
        "#;
        let result = health_check("bash", &["-c".into(), script.into()], &HashMap::new()).await;
        assert!(result.is_err(), "expected Err, got: {result:?}");
    }

    /// health_check: env vars reach the spawned child. The stub answers
    /// `initialize` with success ONLY when `$HEALTHVAR` is set, otherwise it
    /// returns a JSON-RPC error. Run twice: once with the var (expect Ok),
    /// once with empty env (expect Err). Proves env plumbing end-to-end and
    /// gives mixed success/failure coverage.
    #[cfg(unix)]
    #[tokio::test]
    async fn health_check_passes_env_to_child() {
        let script = r#"
            while read line; do
              id=$(echo "$line" | sed -n 's/.*"id":"\([^"]*\)".*/\1/p')
              case "$line" in
                *initialize*)
                  if [ -n "$HEALTHVAR" ]; then
                    printf '{"jsonrpc":"2.0","id":"%s","result":{"protocolVersion":1}}\n' "$id"
                  else
                    printf '{"jsonrpc":"2.0","id":"%s","error":{"code":-32000,"message":"HEALTHVAR unset"}}\n' "$id"
                  fi;;
              esac
            done
        "#;
        let args = ["-c".to_string(), script.to_string()];

        let mut env = HashMap::new();
        env.insert("HEALTHVAR".to_string(), "1".to_string());
        let with_env = health_check("bash", &args, &env).await;
        assert!(
            with_env.is_ok(),
            "expected Ok with env set, got: {with_env:?}"
        );

        let without_env = health_check("bash", &args, &HashMap::new()).await;
        assert!(
            without_env.is_err(),
            "expected Err without env, got: {without_env:?}"
        );
    }
}
