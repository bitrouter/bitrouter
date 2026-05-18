//! [`Executor`] that forwards [`AcpRequest`]s to a per-agent subprocess via
//! newline-delimited JSON-RPC.
//!
//! The ACP wire format is defined by the spec:
//! - Stdio transport / line framing:
//!   <https://agentclientprotocol.com/protocol/transports>
//! - JSON-RPC 2.0 envelope:
//!   <https://www.jsonrpc.org/specification>
//! - Method catalogue (initialize, session/new, session/prompt, …):
//!   <https://agentclientprotocol.com/protocol/schema>
//!
//! ## Design
//!
//! Each configured agent has its own long-lived subprocess. The executor
//! pools them lazily: the first [`Executor::execute`] call for an agent
//! spawns it; subsequent calls reuse the same process. Two background tasks
//! per agent handle the wire:
//!
//! - **Writer**: drains an mpsc of outgoing JSON-RPC envelopes, serialises
//!   each as a single line, writes to the child's stdin.
//! - **Reader**: reads lines from the child's stdout, parses the JSON-RPC
//!   envelope, and either resolves the matching pending request (response
//!   to a client-initiated request) or fans the message out on a broadcast
//!   channel (server-initiated notification / request).
//!
//! Notifications and server-initiated requests are surfaced via
//! [`AcpStdioExecutor::subscribe`], which the `agent-proxy` CLI bridge uses
//! to relay them back to its downstream consumer.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};

use super::transport::AcpTransport;
use super::{AcpRequest, AcpResponse, AcpTarget, Executor};
use crate::error::{BitrouterError, Result};

/// Capacity of the per-agent broadcast channel that fans server-initiated
/// messages (notifications + server→client requests) out to subscribers.
/// Sized to absorb a burst of `session/update`-style streaming
/// notifications without dropping; if a subscriber lags past this the
/// broadcast channel surfaces a `Lagged(n)` error which the subscriber
/// must handle.
const SERVER_MESSAGE_CHANNEL_CAPACITY: usize = 256;

/// Sentinel for stdout buffer growth: ACP messages are typically small
/// (a few KB), but large `session/update` payloads with embedded
/// attachments can push past that. Stop reading a line at this size to
/// keep one rogue upstream from exhausting memory.
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// [`Executor`] that pools per-agent stdio subprocesses.
#[derive(Default)]
pub struct AcpStdioExecutor {
    connections: Arc<Mutex<HashMap<String, Arc<AgentConnection>>>>,
}

impl AcpStdioExecutor {
    /// Fresh executor with an empty pool.
    pub fn new() -> Self {
        Self::default()
    }

    /// Subscribe to the server-initiated message stream for `agent`. Returns
    /// `None` if no connection to that agent exists yet (subscribe after
    /// the first `execute()` call, or call this after [`Self::ensure_connected`]).
    pub async fn subscribe(&self, agent: &str) -> Option<broadcast::Receiver<serde_json::Value>> {
        self.connections
            .lock()
            .await
            .get(agent)
            .map(|c| c.server_messages.subscribe())
    }

    /// Ensure a connection to `target.agent_name` exists, spawning if
    /// necessary. Used by entry points (e.g. the `agent-proxy` CLI) that
    /// want to attach a subscriber *before* the first request.
    pub async fn ensure_connected(&self, target: &AcpTarget) -> Result<()> {
        let _ = self.connection_for(target).await?;
        Ok(())
    }

    async fn connection_for(&self, target: &AcpTarget) -> Result<Arc<AgentConnection>> {
        // Fast path — but skip a dead connection so a one-off crashed child
        // doesn't poison the agent for the rest of the process's lifetime.
        // (Acceptance criteria: if an upstream agent crashes, the next
        // request should respawn it transparently.)
        {
            let mut guard = self.connections.lock().await;
            if let Some(existing) = guard.get(&target.agent_name) {
                if !existing.is_dead.load(Ordering::Acquire) {
                    return Ok(Arc::clone(existing));
                }
                guard.remove(&target.agent_name);
            }
        }
        // Slow path. We drop the lock across the spawn so a slow startup
        // for one agent doesn't block lookups for another. If two requests
        // race to dial the same agent, both will spawn a child; the second
        // spawn's value silently replaces the first in the pool. The
        // losing `AgentConnection`'s reaper still runs against its own
        // `Child`, killing the orphan via `kill_on_drop` once the
        // `AgentConnection` is dropped.
        let connection = AgentConnection::spawn(
            &target.agent_name,
            &target.transport,
            Arc::clone(&self.connections),
        )
        .await?;
        let arc = Arc::new(connection);
        self.connections
            .lock()
            .await
            .insert(target.agent_name.clone(), Arc::clone(&arc));
        Ok(arc)
    }
}

#[async_trait]
impl Executor for AcpStdioExecutor {
    async fn execute(&self, target: &AcpTarget, request: &AcpRequest) -> Result<AcpResponse> {
        let connection = self.connection_for(target).await?;
        let value = connection
            .request(request.method.clone(), request.params.clone())
            .await?;
        Ok(AcpResponse {
            request_id: request.request_id.clone(),
            result: value,
        })
    }
}

/// Map of pending client-initiated request ids → response channels.
type PendingResponses = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value>>>>>;

/// One live upstream agent process.
struct AgentConnection {
    /// Channel handing outgoing JSON-RPC envelopes to the writer task.
    request_tx: mpsc::UnboundedSender<OutgoingMessage>,
    /// Pending client→server requests indexed by the wire id we allocated.
    pending: PendingResponses,
    /// Broadcast of server-initiated messages (notifications + requests).
    server_messages: broadcast::Sender<serde_json::Value>,
    /// Monotonic id allocator for client-initiated requests.
    next_id: AtomicU64,
    /// Set to true once the reaper or the writer observes a fatal condition
    /// (child exit, broken stdin). Read inside the pending lock by
    /// [`Self::request`] so a request arriving after the connection has gone
    /// dark fails fast instead of leaking its oneshot into [`Self::pending`].
    is_dead: Arc<AtomicBool>,
    /// Server-facing tag for error messages.
    agent_name: String,
}

/// An envelope the writer task pulls off the request channel.
struct OutgoingMessage {
    line: String,
}

impl AgentConnection {
    async fn spawn(
        agent_name: &str,
        transport: &AcpTransport,
        pool: Arc<Mutex<HashMap<String, Arc<AgentConnection>>>>,
    ) -> Result<Self> {
        let (mut child, stdin, stdout) = spawn_child(agent_name, transport)?;
        let (request_tx, request_rx) = mpsc::unbounded_channel::<OutgoingMessage>();
        let pending: PendingResponses = Arc::new(Mutex::new(HashMap::new()));
        let (server_messages, _) = broadcast::channel(SERVER_MESSAGE_CHANNEL_CAPACITY);
        let is_dead = Arc::new(AtomicBool::new(false));

        // Writer task. When stdin writes fail, the writer marks the
        // connection dead and drains pending so any in-flight `request()`
        // call that already handed its message to the writer doesn't hang
        // on the orphaned oneshot.
        tokio::spawn(writer_loop(
            stdin,
            request_rx,
            Arc::clone(&pending),
            Arc::clone(&is_dead),
            agent_name.to_string(),
        ));

        // Reader task.
        tokio::spawn(reader_loop(
            stdout,
            Arc::clone(&pending),
            server_messages.clone(),
            agent_name.to_string(),
        ));

        // Reaper: when the child exits, mark the connection dead so future
        // `request()` calls fail fast before inserting a pending oneshot,
        // then fail every still-in-flight request so existing callers
        // don't hang. Both halves run under the pending lock so a
        // concurrent `request()` either observes `is_dead` and bails or
        // already inserted its oneshot before us and we drain it.
        let reaper_pending = Arc::clone(&pending);
        let reaper_dead = Arc::clone(&is_dead);
        let reaper_name = agent_name.to_string();
        let reaper_pool = pool;
        tokio::spawn(async move {
            let exit_status = child.wait().await;
            mark_dead_and_drain(
                &reaper_pending,
                &reaper_dead,
                &reaper_name,
                format!("agent process exited (status: {exit_status:?})"),
            )
            .await;
            // Evict the dead entry from the pool so the next request to
            // this agent name spawns a fresh subprocess instead of
            // reusing the corpse. Compare-pointer-eq the slot's Arc with
            // our `is_dead` Arc so we don't accidentally evict a
            // replacement connection that the slow-path of
            // `connection_for` may have installed while we were waiting
            // on `child.wait`.
            let mut guard = reaper_pool.lock().await;
            let evict = guard
                .get(&reaper_name)
                .is_some_and(|c| Arc::ptr_eq(&c.is_dead, &reaper_dead));
            if evict {
                guard.remove(&reaper_name);
            }
        });

        Ok(Self {
            request_tx,
            pending,
            server_messages,
            next_id: AtomicU64::new(1),
            is_dead,
            agent_name: agent_name.to_string(),
        })
    }

    async fn request(
        &self,
        method: String,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let envelope = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&envelope).map_err(|e| {
            BitrouterError::internal(format!(
                "acp '{}' {method}: serialising request: {e}",
                self.agent_name
            ))
        })?;
        let (tx, rx) = oneshot::channel::<Result<serde_json::Value>>();
        // Insert under the pending lock while checking `is_dead` so we are
        // strictly ordered against the reaper / writer drain: either we
        // insert before the drain (and they wake us with the failure
        // message), or we observe `is_dead` and bail without inserting.
        {
            let mut guard = self.pending.lock().await;
            if self.is_dead.load(Ordering::Acquire) {
                return Err(BitrouterError::Upstream {
                    status: 502,
                    message: format!(
                        "acp '{}': connection is dead (child exited or stdin failed)",
                        self.agent_name
                    ),
                });
            }
            guard.insert(id, tx);
        }
        if self.request_tx.send(OutgoingMessage { line }).is_err() {
            // Writer task is gone — usually because the child already died
            // and the reaper drained pending. Clear our placeholder so the
            // pending map doesn't leak the closed oneshot.
            self.pending.lock().await.remove(&id);
            return Err(BitrouterError::Upstream {
                status: 502,
                message: format!(
                    "acp '{}': writer task gone (child likely exited)",
                    self.agent_name
                ),
            });
        }
        match rx.await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(BitrouterError::Upstream {
                status: 502,
                message: format!(
                    "acp '{}' {method}: response sender dropped (reader task gone)",
                    self.agent_name
                ),
            }),
        }
    }
}

/// Mark the connection dead AND drain every pending oneshot with a uniform
/// `Upstream` error. Called by the reaper on child exit and by the writer
/// on a stdin failure. The `is_dead` store happens under the pending lock
/// so that [`AgentConnection::request`] cannot insert a fresh oneshot
/// between the store and the drain.
async fn mark_dead_and_drain(
    pending: &PendingResponses,
    is_dead: &Arc<AtomicBool>,
    agent_name: &str,
    reason: String,
) {
    let mut guard = pending.lock().await;
    is_dead.store(true, Ordering::Release);
    let pending_count = guard.len();
    for (_, tx) in guard.drain() {
        let _ = tx.send(Err(BitrouterError::Upstream {
            status: 502,
            message: format!(
                "acp '{agent_name}': {reason}; {pending_count} pending request(s) aborted"
            ),
        }));
    }
}

fn spawn_child(
    agent_name: &str,
    transport: &AcpTransport,
) -> Result<(Child, ChildStdin, ChildStdout)> {
    match transport {
        AcpTransport::Stdio { command, args, env } => {
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(args);
            for (k, v) in env {
                cmd.env(k, v);
            }
            cmd.stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                // Inherit stderr so an operator running `bitrouter
                // agent-proxy` (or the daemon) sees the upstream's
                // diagnostic output without an extra pump.
                .stderr(std::process::Stdio::inherit())
                .kill_on_drop(true);
            let mut child = cmd.spawn().map_err(|e| BitrouterError::Upstream {
                status: 502,
                message: format!("acp '{agent_name}': spawning '{command}': {e}"),
            })?;
            let stdin = child.stdin.take().ok_or_else(|| {
                BitrouterError::internal(format!(
                    "acp '{agent_name}': failed to capture child stdin"
                ))
            })?;
            let stdout = child.stdout.take().ok_or_else(|| {
                BitrouterError::internal(format!(
                    "acp '{agent_name}': failed to capture child stdout"
                ))
            })?;
            Ok((child, stdin, stdout))
        }
    }
}

async fn writer_loop(
    mut stdin: ChildStdin,
    mut rx: mpsc::UnboundedReceiver<OutgoingMessage>,
    pending: PendingResponses,
    is_dead: Arc<AtomicBool>,
    agent_name: String,
) {
    while let Some(OutgoingMessage { mut line }) = rx.recv().await {
        line.push('\n');
        if let Err(e) = stdin.write_all(line.as_bytes()).await {
            tracing::warn!(agent = %agent_name, %e, "acp writer: stdin write failed; closing");
            // The OutgoingMessage we just consumed will never reach the
            // upstream — its caller is waiting on the pending-map oneshot.
            // Mark the connection dead and drain so that caller (and every
            // other in-flight one) sees a failure instead of hanging.
            mark_dead_and_drain(
                &pending,
                &is_dead,
                &agent_name,
                format!("stdin write failed: {e}"),
            )
            .await;
            return;
        }
        if let Err(e) = stdin.flush().await {
            tracing::warn!(agent = %agent_name, %e, "acp writer: stdin flush failed; closing");
            mark_dead_and_drain(
                &pending,
                &is_dead,
                &agent_name,
                format!("stdin flush failed: {e}"),
            )
            .await;
            return;
        }
    }
    // Receiver closed (executor dropped): drop stdin so the child sees EOF
    // and shuts down cleanly. We do NOT mark the connection dead here —
    // this is the orderly-shutdown path; the reaper observes the child's
    // exit and runs the drain.
}

async fn reader_loop(
    stdout: ChildStdout,
    pending: PendingResponses,
    server_messages: broadcast::Sender<serde_json::Value>,
    agent_name: String,
) {
    let mut reader = BufReader::new(stdout);
    let mut buf = String::new();
    loop {
        buf.clear();
        let read = read_line_bounded(&mut reader, &mut buf, MAX_LINE_BYTES).await;
        match read {
            Ok(0) => {
                tracing::debug!(agent = %agent_name, "acp reader: child stdout EOF");
                break;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(agent = %agent_name, %e, "acp reader: stdout read failed; closing");
                break;
            }
        }
        let line = buf.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    agent = %agent_name,
                    %e,
                    line_preview = %preview(line, 200),
                    "acp reader: unparseable line; dropping"
                );
                continue;
            }
        };
        dispatch_incoming(&pending, &server_messages, &agent_name, value).await;
    }
    // EOF / read error: nothing more to dispatch. The reaper task observes
    // the child's exit independently.
}

async fn dispatch_incoming(
    pending: &PendingResponses,
    server_messages: &broadcast::Sender<serde_json::Value>,
    agent_name: &str,
    value: serde_json::Value,
) {
    let id = value.get("id");
    let has_method = value.get("method").is_some();
    match (id, has_method) {
        // Server-initiated notification (no id, has method).
        (None, true) => {
            let _ = server_messages.send(value);
        }
        // Response to a client-initiated request (has id, no method).
        (Some(id_value), false) => {
            let Some(id_u64) = id_value.as_u64() else {
                tracing::warn!(
                    agent = %agent_name,
                    "acp reader: response id was not the unsigned integer we allocated; dropping"
                );
                return;
            };
            let sender = pending.lock().await.remove(&id_u64);
            let Some(tx) = sender else {
                tracing::warn!(
                    agent = %agent_name,
                    id = id_u64,
                    "acp reader: response for unknown id; dropping"
                );
                return;
            };
            // JSON-RPC: success has `result`, failure has `error`.
            if let Some(error) = value.get("error") {
                let message = error
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(no message)");
                let code = error.get("code").and_then(|v| v.as_i64()).unwrap_or(0);
                let _ = tx.send(Err(BitrouterError::Upstream {
                    status: 502,
                    message: format!("acp '{agent_name}' upstream error {code}: {message}"),
                }));
            } else {
                let result = value
                    .get("result")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let _ = tx.send(Ok(result));
            }
        }
        // Server-initiated request (has both id and method). v1.0 cannot
        // answer these in-line because the executor's API is one-way; we
        // broadcast so a subscriber (the `agent-proxy` CLI) can relay it.
        (Some(_), true) => {
            let _ = server_messages.send(value);
        }
        // Malformed: drop with a log.
        (None, false) => {
            tracing::warn!(
                agent = %agent_name,
                "acp reader: message with neither id nor method; dropping"
            );
        }
    }
}

async fn read_line_bounded(
    reader: &mut BufReader<ChildStdout>,
    buf: &mut String,
    max_bytes: usize,
) -> std::io::Result<usize> {
    // Walks the BufReader's internal chunk a slice at a time so a rogue
    // upstream that never emits a `\n` can't drive us into an unbounded
    // allocation. The naive `read_until` would fill the whole line first
    // and only then check the cap.
    let mut raw: Vec<u8> = Vec::with_capacity(256);
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            break; // EOF
        }
        let newline_at = chunk.iter().position(|&b| b == b'\n');
        match newline_at {
            Some(i) => {
                let take = i + 1;
                if raw.len() + take > max_bytes {
                    return Err(std::io::Error::other(format!(
                        "acp line exceeded {max_bytes} bytes"
                    )));
                }
                raw.extend_from_slice(&chunk[..take]);
                reader.consume(take);
                break;
            }
            None => {
                if raw.len() + chunk.len() > max_bytes {
                    // Consume what we already have so the next read doesn't
                    // re-see the same bytes; then surface the cap error.
                    let n = chunk.len();
                    reader.consume(n);
                    return Err(std::io::Error::other(format!(
                        "acp line exceeded {max_bytes} bytes"
                    )));
                }
                raw.extend_from_slice(chunk);
                let n = chunk.len();
                reader.consume(n);
            }
        }
    }
    match std::str::from_utf8(&raw) {
        Ok(s) => {
            buf.push_str(s);
            Ok(raw.len())
        }
        Err(e) => Err(std::io::Error::other(format!("acp line not utf-8: {e}"))),
    }
}

fn preview(s: &str, max_chars: usize) -> String {
    s.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caller::{CallerContext, PaymentMethod};

    fn req(agent: &str, method: &str) -> AcpRequest {
        AcpRequest::new(
            agent,
            method,
            serde_json::json!({}),
            CallerContext::new("k", "u", PaymentMethod::None),
        )
    }

    fn target(name: &str, command: &str) -> AcpTarget {
        AcpTarget {
            agent_name: name.into(),
            transport: AcpTransport::Stdio {
                command: command.into(),
                args: vec![],
                env: HashMap::new(),
            },
        }
    }

    #[test]
    fn executor_constructs_with_empty_pool() {
        let _ = AcpStdioExecutor::new();
    }

    /// `/bin/false` exits before any handshake — the writer or reader task
    /// observes the EOF, the reaper drains pending requests, and the
    /// caller sees a clean 502.
    #[tokio::test]
    async fn child_that_dies_immediately_surfaces_as_502() {
        let exec = AcpStdioExecutor::new();
        let err = exec
            .execute(&target("ghost", "/bin/false"), &req("ghost", "initialize"))
            .await
            .unwrap_err();
        assert_eq!(err.status(), 502, "unexpected: {err}");
        assert!(
            err.to_string().contains("acp 'ghost'"),
            "tag missing: {err}"
        );
    }

    /// Spawn failure (command doesn't exist) is also 502 — not a panic.
    #[tokio::test]
    async fn spawn_failure_surfaces_as_502() {
        let exec = AcpStdioExecutor::new();
        let err = exec
            .execute(
                &target("ghost", "/definitely/no/such/bin/bitrouter-acp-test"),
                &req("ghost", "initialize"),
            )
            .await
            .unwrap_err();
        assert_eq!(err.status(), 502, "unexpected: {err}");
    }

    /// End-to-end happy path against a `bash`-implemented echo agent. The
    /// script reads one JSON line, echoes a `{result: ...}` response, then
    /// exits. We verify: ids round-trip, params arrive at the upstream,
    /// the result returns to the caller.
    #[tokio::test]
    async fn echo_agent_round_trips_a_request_response() {
        // A POSIX shell one-liner that:
        //   - reads one line from stdin (the JSON-RPC request)
        //   - extracts the numeric id via sed
        //   - writes a JSON-RPC response with the same id and a known result
        // Avoids any external dep (no `jq` etc.) so the test runs in CI.
        let script = r#"
            read line
            id=$(echo "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
            printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1}}\n' "$id"
        "#;
        let exec = AcpStdioExecutor::new();
        let target = AcpTarget {
            agent_name: "echo".into(),
            transport: AcpTransport::Stdio {
                command: "bash".into(),
                args: vec!["-c".into(), script.into()],
                env: HashMap::new(),
            },
        };
        let resp = exec
            .execute(&target, &req("echo", "initialize"))
            .await
            .unwrap();
        assert_eq!(resp.result["protocolVersion"], 1);
    }

    /// Same as above but the upstream returns a JSON-RPC `error` body. We
    /// surface it as a 502 Upstream so the caller can react.
    #[tokio::test]
    async fn upstream_jsonrpc_error_surfaces_as_502() {
        let script = r#"
            read line
            id=$(echo "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
            printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"unknown method"}}\n' "$id"
        "#;
        let exec = AcpStdioExecutor::new();
        let target = AcpTarget {
            agent_name: "bad".into(),
            transport: AcpTransport::Stdio {
                command: "bash".into(),
                args: vec!["-c".into(), script.into()],
                env: HashMap::new(),
            },
        };
        let err = exec
            .execute(&target, &req("bad", "session/new"))
            .await
            .unwrap_err();
        assert_eq!(err.status(), 502);
        assert!(
            err.to_string().contains("unknown method"),
            "unexpected: {err}"
        );
    }

    /// Regression for the Phase-3-review-discovered leak: when an agent
    /// crashes between requests, the pool must evict the dead connection
    /// and the next request must respawn transparently. Before the fix,
    /// the second request would either hang or be silently rejected by
    /// the pool's reuse of the corpse.
    #[tokio::test]
    async fn pool_evicts_dead_connection_and_respawns_on_next_request() {
        // First child: answer once and exit. Second child: answer once
        // again. We expect the executor to spawn child #1, observe its
        // exit, evict, then spawn child #2 on the second `execute`.
        let script = r#"
            read line
            id=$(echo "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
            printf '{"jsonrpc":"2.0","id":%s,"result":{"call":1}}\n' "$id"
            exit 0
        "#;
        let exec = AcpStdioExecutor::new();
        let target = AcpTarget {
            agent_name: "respawn".into(),
            transport: AcpTransport::Stdio {
                command: "bash".into(),
                args: vec!["-c".into(), script.into()],
                env: HashMap::new(),
            },
        };
        let r1 = exec
            .execute(&target, &req("respawn", "initialize"))
            .await
            .unwrap();
        assert_eq!(r1.result["call"], 1);
        // Give the reaper a moment to observe the child exit + evict.
        // A real upstream's crash is observed asynchronously; the next
        // `execute` must tolerate either the eviction happening before
        // or interleaving with its arrival.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let r2 = exec
            .execute(&target, &req("respawn", "initialize"))
            .await
            .unwrap();
        assert_eq!(r2.result["call"], 1, "respawn produced fresh value");
    }

    /// Regression for the unbounded-allocation in `read_line_bounded`:
    /// emit > MAX_LINE_BYTES of bytes with no newline. The reader loop
    /// observes the cap error and exits without panicking. The pending
    /// request is then drained by the reaper when the child exits.
    #[tokio::test]
    async fn reader_caps_oversized_line_without_unbounded_allocation() {
        // Emit 17 MiB of `x` bytes without a newline, then exit. v1's
        // MAX_LINE_BYTES is 16 MiB, so the reader should bail before
        // exhausting memory. The request times out via the reaper after
        // the child exits.
        let script = r#"
            read line
            head -c 17825792 /dev/zero | tr '\0' 'x'
        "#;
        let exec = AcpStdioExecutor::new();
        let target = AcpTarget {
            agent_name: "huge".into(),
            transport: AcpTransport::Stdio {
                command: "bash".into(),
                args: vec!["-c".into(), script.into()],
                env: HashMap::new(),
            },
        };
        let err = exec
            .execute(&target, &req("huge", "initialize"))
            .await
            .unwrap_err();
        assert_eq!(err.status(), 502);
    }

    /// Notifications (no id) flow out via the broadcast channel rather than
    /// the response oneshot. Subscribe first, then drive a request that
    /// triggers an upstream notification before the response.
    #[tokio::test]
    async fn server_notifications_reach_subscribers() {
        let script = r#"
            read line
            id=$(echo "$line" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
            printf '{"jsonrpc":"2.0","method":"session/update","params":{"chunk":"hi"}}\n'
            printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
        "#;
        let exec = AcpStdioExecutor::new();
        let target = AcpTarget {
            agent_name: "stream".into(),
            transport: AcpTransport::Stdio {
                command: "bash".into(),
                args: vec!["-c".into(), script.into()],
                env: HashMap::new(),
            },
        };
        // Spawn the connection first so we can subscribe before the
        // request triggers the notification.
        exec.ensure_connected(&target).await.unwrap();
        let mut rx = exec.subscribe("stream").await.unwrap();
        let exec_arc = Arc::new(exec);
        let exec_for_task = Arc::clone(&exec_arc);
        let target_for_task = target.clone();
        let response_handle = tokio::spawn(async move {
            exec_for_task
                .execute(&target_for_task, &req("stream", "session/prompt"))
                .await
        });
        // The notification arrives first; the response shortly after.
        let notification = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await
            .expect("notification timeout")
            .expect("broadcast recv");
        assert_eq!(notification["method"], "session/update");
        assert_eq!(notification["params"]["chunk"], "hi");
        let resp = response_handle.await.unwrap().unwrap();
        assert_eq!(resp.result["ok"], true);
    }
}
