//! `bitrouter acp` subcommands — headless ACP session surface.
//!
//! Three entry points:
//!
//! - [`serve`] — launch a session and expose it as a vanilla ACP Agent over
//!   **stdio** until the manager disconnects. Used by GUIs and orchestrating
//!   agents that speak ACP.
//!
//! - [`prompt`] — launch a session, subscribe to updates, send one prompt, and
//!   stream each event as a self-describing **NDJSON** line to `out`. Exits
//!   after the prompt resolves (or immediately after submission when `no_wait`
//!   is true).
//!
//! - [`sessions`] — list the durable session records under the current repo's
//!   `.bitrouter/sessions/`, newest first.
//!
//! ## NDJSON format
//!
//! Update lines carry the [`SessionUpdateKind`] directly — the `type` tag
//! value is the snake_case variant name (`message_chunk`, `thought_chunk`,
//! `tool_call`, `tool_call_update`). The terminal result line is:
//!
//! ```json
//! {"type":"result","stop_reason":"end_turn"}
//! ```
//!
//! The `stop_reason` value is the ACP wire form (snake_case, via serde) so a
//! downstream parser sees the same spelling the protocol uses.
//!
//! In `--no-wait` mode the only line emitted is:
//!
//! ```json
//! {"type":"submitted"}
//! ```
//!
//! Both functions load their `Config` via the standard resolution chain (see
//! [`bitrouter::paths`]) and build a [`ConfigAcpRoutingTable`] from
//! `config.agents` — the same table the GUI renderer uses.

use std::sync::Arc;

use anyhow::{Context, Result};
use bitrouter_sdk::acp::ConfigAcpRoutingTable;
use bitrouter_sdk::config::Config;
use futures::StreamExt;
use serde::Serialize;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use bitrouter_substrate::engine::LaunchOptions;
use bitrouter_substrate::telemetry::RequestCompleted;
use bitrouter_substrate::translate::SessionUpdateKind;
use bitrouter_substrate::worktree::WorktreeSpec;

// ── NDJSON helpers ────────────────────────────────────────────────────────────

/// Terminal result line emitted after the prompt resolves.
///
/// Generic over the stop-reason type so the ACP `StopReason` (which derives
/// `serde::Serialize` with snake_case rename) renders its wire form directly —
/// `"end_turn"`, not the Rust `Debug` spelling `"EndTurn"`. Keeping it generic
/// also avoids naming `agent_client_protocol_schema` here (it isn't a direct
/// dependency of this crate).
#[derive(Serialize)]
struct ResultLine<S: Serialize> {
    #[serde(rename = "type")]
    kind: &'static str,
    stop_reason: S,
}

/// Write one NDJSON line (JSON + `\n`) to `out`.
async fn write_ndjson_line<W, T>(out: &mut W, value: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let mut line = serde_json::to_string(value).context("serialising NDJSON line")?;
    line.push('\n');
    out.write_all(line.as_bytes())
        .await
        .context("writing NDJSON line")
}

// ── serve ─────────────────────────────────────────────────────────────────────

/// Warm-session behavior for [`serve`]: after the stdio manager disconnects,
/// keep the session alive and accept manager reattach connections on a
/// per-session unix socket until no manager has been connected for
/// `idle_timeout`.
#[derive(Debug, Clone)]
pub struct WarmOptions {
    pub idle_timeout: std::time::Duration,
}

/// Launch a session for `agent_id` and serve it as a vanilla ACP Agent over
/// **stdio** until the manager disconnects.
///
/// Config is taken by value (already loaded by the caller); `options` carries
/// the worktree spec, transcript switch, and per-turn timeout resolved from
/// the CLI flags (see [`launch_options`]).
///
/// With `warm`, the session survives manager disconnects: reattach
/// connections are accepted on `.bitrouter/sessions/<record_id>.sock` — the
/// **same NDJSON JSON-RPC framing as stdio** over a unix socket (no bespoke
/// protocol; ACP's standardized remote transport replaces this when it
/// ships). A reconnecting manager runs `initialize` → `session/load` (full
/// transcript replay) → continues. The session shuts down after
/// `idle_timeout` with no manager attached.
pub async fn serve(
    config: Config,
    agent_id: &str,
    options: LaunchOptions,
    warm: Option<WarmOptions>,
) -> Result<()> {
    #[cfg(not(unix))]
    if warm.is_some() {
        anyhow::bail!("--warm requires unix domain sockets (unix-only in v1)");
    }
    let catalog = catalog_from_config(&config)?;
    let base_repo = std::env::current_dir().context("resolving current directory")?;
    // Deferred open: the upstream `session/new` runs when the manager sends
    // its own `session/new`, so the manager's cwd + mcpServers are relayed.
    let session = bitrouter_substrate::engine::Session::launch_deferred(
        &catalog,
        agent_id,
        base_repo.clone(),
        options,
    )
    .await
    .with_context(|| format!("launching acp session for agent '{agent_id}'"))?;
    let exporter = attach_observability(&config, agent_id, &session).await;
    let session = Arc::new(session);

    // Warm: bind the reattach socket up front so the record advertises it for
    // the session's whole life (a manager can attach even before the stdio
    // manager disconnects — connections are served one at a time).
    #[cfg(unix)]
    let reattach = match &warm {
        Some(_) => {
            // Sockets live under the (short, stable) bitrouter home, NOT the
            // repo: `sun_path` caps unix socket paths at ~104 bytes on macOS,
            // which a deeply nested repo blows through. The record stores the
            // absolute path, so discovery is location-independent.
            let dir = socket_dir();
            tokio::fs::create_dir_all(&dir)
                .await
                .with_context(|| format!("creating {}", dir.display()))?;
            let record_id = &session.state().record_id;
            let short: String = record_id.chars().take(16).collect();
            let path = dir.join(format!("{short}.sock"));
            // A stale socket file from a dead process blocks bind; the name is
            // session-unique, so removing it is safe.
            let _ = tokio::fs::remove_file(&path).await;
            let listener = tokio::net::UnixListener::bind(&path)
                .with_context(|| format!("binding reattach socket {}", path.display()))?;
            session.advertise_socket(path.clone()).await;
            Some((listener, path))
        }
        None => None,
    };

    let mut served = bitrouter_substrate::down::serve(Arc::clone(&session)).await;

    // Warm loop: the stdio manager is gone; accept reattach connections until
    // the idle timeout elapses with no manager.
    #[cfg(unix)]
    if let (Some(warm), Some((listener, socket_path))) = (&warm, &reattach) {
        use agent_client_protocol::ByteStreams;
        use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
        loop {
            match tokio::time::timeout(warm.idle_timeout, listener.accept()).await {
                Err(_) => {
                    tracing::info!(
                        idle = ?warm.idle_timeout,
                        "no manager reattached within the idle timeout; shutting down"
                    );
                    break;
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "reattach accept failed; shutting down");
                    break;
                }
                Ok(Ok((stream, _addr))) => {
                    tracing::info!("manager reattached over {}", socket_path.display());
                    let (read_half, write_half) = stream.into_split();
                    let transport = ByteStreams::new(write_half.compat_write(), read_half.compat());
                    served =
                        bitrouter_substrate::down::serve_on(Arc::clone(&session), transport).await;
                }
            }
        }
        let _ = tokio::fs::remove_file(socket_path).await;
    }

    // No manager left: shut the session down deliberately so the worktree
    // policy is honored (same semantics as `prompt`). Once serving ends, the
    // forwarding tasks have released their clones, so we are the sole owner.
    match Arc::try_unwrap(session) {
        Ok(session) => session
            .shutdown()
            .await
            .context("shutting down acp session")?,
        Err(_) => tracing::warn!("session still referenced after serve; skipping shutdown"),
    }
    if let Some(exporter) = exporter {
        // Flush the span batch before exit; spans are lost otherwise.
        exporter.shutdown();
    }
    served.map_err(|e| anyhow::anyhow!("acp serve: {e}"))
}

// ── attach ────────────────────────────────────────────────────────────────────

/// Bridge this process's stdio to a warm session's reattach socket: a plain
/// bidirectional byte pump (both sides speak the stdio NDJSON JSON-RPC
/// framing, so no parsing is involved). Resolves `record_prefix` against the
/// current repo's session records; the record must advertise a socket (the
/// session is running `serve --warm`). Ends when either side closes.
#[cfg(unix)]
pub async fn attach(record_prefix: &str) -> Result<()> {
    use bitrouter_substrate::record::RecordStore;

    let base = std::env::current_dir().context("resolving current directory")?;
    let records = RecordStore::new(&base).list().await?;
    let matches: Vec<_> = records
        .iter()
        .filter(|r| r.record_id.starts_with(record_prefix))
        .collect();
    let record = match matches.as_slice() {
        [] => anyhow::bail!(
            "no session record matches '{record_prefix}' (see `bitrouter acp sessions`)"
        ),
        [record] => *record,
        _ => anyhow::bail!(
            "'{record_prefix}' matches {} sessions; use more of the record id",
            matches.len()
        ),
    };
    let Some(socket) = &record.socket else {
        anyhow::bail!(
            "session {} has no reattach socket — it is not running `acp serve --warm`",
            &record.record_id[..8.min(record.record_id.len())]
        );
    };
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .with_context(|| {
            format!(
                "connecting to {} (is the session still alive?)",
                socket.display()
            )
        })?;
    let (mut sock_read, mut sock_write) = stream.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    tokio::select! {
        r = tokio::io::copy(&mut sock_read, &mut stdout) => { r.context("socket → stdout")?; }
        r = tokio::io::copy(&mut stdin, &mut sock_write) => { r.context("stdin → socket")?; }
    }
    Ok(())
}

/// Unix-only: reattach rides unix domain sockets in v1.
#[cfg(not(unix))]
pub async fn attach(_record_prefix: &str) -> Result<()> {
    anyhow::bail!("`bitrouter acp attach` requires unix domain sockets (unix-only in v1)")
}

// ── prompt ────────────────────────────────────────────────────────────────────

/// Launch a session for `agent_id`, send one prompt, and stream each
/// [`SessionUpdateKind`] as a self-describing NDJSON line to `out`.
///
/// When `no_wait` is false (the default): subscribe to updates, send the
/// prompt, stream updates while the prompt is in flight, emit a terminal
/// `{"type":"result","stop_reason":"…"}` line, then shut down the session.
///
/// When `no_wait` is true: shut down the session immediately after emitting
/// `{"type":"submitted"}`. The agent child is terminated; callers needing a
/// persistent session should use `bitrouter acp serve` instead.
pub async fn prompt<W>(
    config: Config,
    agent_id: &str,
    options: LaunchOptions,
    text: &str,
    no_wait: bool,
    out: &mut W,
) -> Result<()>
where
    W: AsyncWrite + Unpin + Send,
{
    let catalog = catalog_from_config(&config)?;
    let base_repo = std::env::current_dir().context("resolving current directory")?;
    let session =
        bitrouter_substrate::engine::Session::launch(&catalog, agent_id, base_repo, options)
            .await
            .with_context(|| format!("launching acp session for agent '{agent_id}'"))?;
    let exporter = attach_observability(&config, agent_id, &session).await;

    if no_wait {
        // v1 no-wait: emit ack, then shut down immediately. The agent child is
        // killed on shutdown. Callers needing a persistent background session
        // should use `bitrouter acp serve` instead.
        write_ndjson_line(out, &serde_json::json!({ "type": "submitted" })).await?;
        session
            .shutdown()
            .await
            .context("shutting down acp session")?;
        if let Some(exporter) = exporter {
            exporter.shutdown();
        }
        return Ok(());
    }

    let outcome = prompt_wait(session, text, out).await;
    if let Some(exporter) = exporter {
        // Flush the span batch before exit; spans are lost otherwise.
        exporter.shutdown();
    }
    outcome
}

/// Inner implementation for the wait (non-`--no-wait`) path. Separated so the
/// early-return in the `no_wait` branch above doesn't borrow `session` past its
/// drop point.
async fn prompt_wait<W>(
    session: bitrouter_substrate::engine::Session,
    text: &str,
    out: &mut W,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    // Subscribe to updates BEFORE prompting so no streamed update is missed.
    let mut updates = session.updates();

    // Drive updates and the prompt concurrently. The loop returns the resolved
    // `PromptResponse` directly, so there is no `Option` to unwrap afterward.
    let response = {
        let prompt_future = session.prompt(text);
        tokio::pin!(prompt_future);

        loop {
            tokio::select! {
                biased;

                result = &mut prompt_future => {
                    let response = result.context("acp prompt failed")?;
                    // Non-blocking drain of any already-buffered updates.
                    drain_pending_updates(&mut updates, out).await?;
                    break response;
                }

                maybe_update = updates.next() => {
                    if let Some(update) = maybe_update {
                        // Emit the SessionUpdateKind directly; its own `type`
                        // tag (e.g. "message_chunk") makes it self-describing.
                        write_ndjson_line(out, &update).await?;
                    }
                }
            }
        }
    };

    // Emit the terminal result line. `response.stop_reason` is an ACP
    // `StopReason` that serializes to its snake_case wire form (e.g.
    // `"end_turn"`).
    write_ndjson_line(
        out,
        &ResultLine {
            kind: "result",
            stop_reason: response.stop_reason,
        },
    )
    .await?;

    session
        .shutdown()
        .await
        .context("shutting down acp session")?;
    Ok(())
}

// ── sessions ──────────────────────────────────────────────────────────────────

/// List the session records under the current repo's `.bitrouter/sessions/`,
/// newest first: short record id, agent, status, age, and worktree.
///
/// A record left `running` by a substrate process that died without shutting
/// down is shown as `dead` (its pid no longer exists) rather than trusted.
pub async fn sessions<W>(out: &mut W) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    use bitrouter_substrate::record::{RecordStatus, RecordStore, now_unix};

    let base = std::env::current_dir().context("resolving current directory")?;
    let store = RecordStore::new(&base);
    let mut records = store.list().await?;
    if records.is_empty() {
        out.write_all(b"no sessions recorded under .bitrouter/sessions\n")
            .await
            .context("writing output")?;
        return Ok(());
    }
    records.sort_by(|a, b| b.started_at.cmp(&a.started_at));

    let now = now_unix();
    let mut buf = String::from("RECORD    AGENT             STATUS   AGE      WORKTREE\n");
    for r in records {
        let status = match r.status {
            RecordStatus::Exited => "exited",
            RecordStatus::Running if pid_alive(r.pid) => "running",
            RecordStatus::Running => "dead",
        };
        let short_id: String = r.record_id.chars().take(8).collect();
        let worktree = r
            .worktree
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "-".to_string());
        buf.push_str(&format!(
            "{short_id:<9} {agent:<17} {status:<8} {age:<8} {worktree}\n",
            agent = r.agent_id,
            age = format_age(now.saturating_sub(r.started_at)),
        ));
    }
    out.write_all(buf.as_bytes())
        .await
        .context("writing output")
}

/// Whether `pid` is a live process. Used to demote a stale `running` record
/// (left behind by a killed substrate) to `dead` in the listing.
fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // `kill -0` probes existence without signalling. EPERM (owned by
        // another user) exits non-zero, which conservatively reads as dead —
        // acceptable, since substrate sessions run as the invoking user.
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Render an age in seconds as a compact human unit (`42s`, `7m`, `3h`, `2d`).
fn format_age(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Attach observability to a session when the observe config opts telemetry
/// in: every turn is drained to stderr (always) and, with an exporter,
/// emitted as an OTel GenAI `invoke_agent` span; tool calls become
/// `execute_tool` spans from the translated update stream. Returns the
/// exporter so the caller can flush it (`shutdown`) before exit.
async fn attach_observability(
    config: &Config,
    agent_id: &str,
    session: &bitrouter_substrate::engine::Session,
) -> Option<Arc<bitrouter_observe::otel::OtelExporter>> {
    let exporter = crate::assemble::build_otel_exporter_standalone(config).await;
    let recorder = exporter.as_ref().map(|exporter| {
        Arc::new(bitrouter_observe::acp::AcpSpanRecorder::new(
            exporter,
            agent_id,
            session.state().record_id.clone(),
        ))
    });

    // Telemetry drain: stderr log per turn (always) + invoke_agent span.
    if let Some(mut rx) = session.telemetry() {
        let recorder = recorder.clone();
        tokio::spawn(async move {
            while let Some(record) = rx.recv().await {
                if let Some(recorder) = &recorder {
                    recorder.turn_completed(&bitrouter_observe::acp::TurnRecord {
                        stop_reason: record.stop_reason.clone(),
                        latency: std::time::Duration::from_millis(record.latency_ms),
                        context_used: record.context.map(|c| c.used),
                        context_size: record.context.map(|c| c.size),
                    });
                }
                drain_telemetry_record(record);
            }
        });
    }

    // Tool spans from the translated update stream (exporter-gated: without
    // one there is nothing to emit to).
    if let Some(recorder) = recorder {
        let mut updates = session.updates();
        tokio::spawn(async move {
            use bitrouter_substrate::translate::ToolStatus;
            while let Some(update) = updates.next().await {
                match update {
                    SessionUpdateKind::ToolCall {
                        id, title, status, ..
                    } => match status {
                        ToolStatus::Pending | ToolStatus::Running => {
                            recorder.tool_started(id, title);
                        }
                        ToolStatus::Ok => recorder.tool_finished(&id, true, Some(&title)),
                        ToolStatus::Failed => recorder.tool_finished(&id, false, Some(&title)),
                    },
                    SessionUpdateKind::ToolCallUpdate {
                        id, status, title, ..
                    } => match status {
                        Some(ToolStatus::Ok) => {
                            recorder.tool_finished(&id, true, title.as_deref());
                        }
                        Some(ToolStatus::Failed) => {
                            recorder.tool_finished(&id, false, title.as_deref());
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        });
    }

    exporter
}

/// Emit one telemetry record to stderr via tracing. Stdout must stay clean
/// (ACP JSON-RPC for `serve`, NDJSON for `prompt`), so telemetry goes to
/// `tracing::info!` which the acp CLI routes to stderr.
fn drain_telemetry_record(r: RequestCompleted) {
    tracing::info!(
        agent = %r.agent,
        stop_reason = %r.stop_reason,
        latency_ms = r.latency_ms,
        context_used = r.context.map(|c| c.used),
        context_size = r.context.map(|c| c.size),
        "acp turn completed"
    );
}

/// Build [`LaunchOptions`] from the CLI flags shared by `serve` and `prompt`:
/// `--worktree`/`--rm-worktree` (retention is the default — removal destroys
/// the agent's uncommitted work, so it is strictly opt-in), `--no-transcript`
/// (the durable transcript is on by default), and `--turn-timeout <secs>`.
pub fn launch_options(
    worktree: Option<&str>,
    rm_worktree: bool,
    no_transcript: bool,
    turn_timeout_secs: Option<u64>,
) -> LaunchOptions {
    LaunchOptions {
        worktree: worktree.map(|name| WorktreeSpec {
            name: name.to_string(),
            remove_on_shutdown: rm_worktree,
        }),
        transcript: !no_transcript,
        turn_timeout: turn_timeout_secs.map(std::time::Duration::from_secs),
    }
}

/// Directory warm-session reattach sockets are bound in: `$BITROUTER_HOME`
/// (when set) or `~/.bitrouter`, plus `sock/`. Deliberately NOT under the
/// repo — unix `sun_path` is ~104 bytes on macOS and repo paths run long.
#[cfg(unix)]
fn socket_dir() -> std::path::PathBuf {
    use std::path::PathBuf;
    std::env::var_os("BITROUTER_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|home| PathBuf::from(home).join(".bitrouter"))
        })
        .unwrap_or_else(std::env::temp_dir)
        .join("sock")
}

/// Build a [`ConfigAcpRoutingTable`] from the `agents` section of `config`.
fn catalog_from_config(config: &Config) -> Result<ConfigAcpRoutingTable> {
    ConfigAcpRoutingTable::from_configs(config.agents.iter().map(|(k, v)| (k.clone(), v.clone())))
        .context("building acp routing table from config")
}

/// Non-blocking drain: write any updates immediately available in the broadcast
/// buffer, then stop. Called after the prompt resolves to flush trailing updates.
///
/// Uses a biased `tokio::select!` where the first arm is `updates.next()` and
/// the second is an always-ready no-op. With `biased`, the first arm wins when
/// a value is immediately available; the second arm wins (returning `None`) when
/// no update is buffered, ending the drain.
async fn drain_pending_updates<W>(
    updates: &mut (impl futures::Stream<Item = SessionUpdateKind> + Unpin),
    out: &mut W,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    loop {
        let maybe = tokio::select! {
            biased;
            v = updates.next() => v,
            _ = std::future::ready(()) => None,
        };
        match maybe {
            Some(update) => write_ndjson_line(out, &update).await?,
            None => break,
        }
    }
    Ok(())
}
