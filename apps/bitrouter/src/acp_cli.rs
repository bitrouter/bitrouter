//! `bitrouter acp` subcommands — headless ACP session surface.
//!
//! Two entry points:
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

/// Launch a session for `agent_id` and serve it as a vanilla ACP Agent over
/// **stdio** until the manager disconnects.
///
/// Config is taken by value (already loaded by the caller). `worktree` names
/// an optional git worktree to provision inside the current directory's repo
/// (created, or reused when it already exists). The worktree is **retained**
/// on exit — it holds the agent's work — unless `rm_worktree` opts in to
/// removal.
pub async fn serve(
    config: Config,
    agent_id: &str,
    worktree: Option<&str>,
    rm_worktree: bool,
) -> Result<()> {
    let catalog = catalog_from_config(&config)?;
    let base_repo = std::env::current_dir().context("resolving current directory")?;
    let session = bitrouter_substrate::engine::Session::launch(
        &catalog,
        agent_id,
        base_repo,
        worktree_spec(worktree, rm_worktree),
    )
    .await
    .with_context(|| format!("launching acp session for agent '{agent_id}'"))?;
    // Take the telemetry receiver BEFORE wrapping in Arc so we don't need &mut
    // through the shared reference. Drain-and-log to stderr; tracing already
    // goes to stderr for both acp modes so stdout (ACP JSON-RPC) stays clean.
    if let Some(mut rx) = session.telemetry() {
        tokio::spawn(async move {
            while let Some(r) = rx.recv().await {
                drain_telemetry_record(r);
            }
        });
    }
    let session = Arc::new(session);
    let served = bitrouter_substrate::down::serve(Arc::clone(&session)).await;

    // Manager disconnected: shut the session down deliberately so the worktree
    // policy is honored (same semantics as `prompt`). Once `serve` returns, the
    // forwarding tasks have released their clones, so we are the sole owner.
    match Arc::try_unwrap(session) {
        Ok(session) => session
            .shutdown()
            .await
            .context("shutting down acp session")?,
        Err(_) => tracing::warn!("session still referenced after serve; skipping shutdown"),
    }
    served.map_err(|e| anyhow::anyhow!("acp serve: {e}"))
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
    worktree: Option<&str>,
    rm_worktree: bool,
    text: &str,
    no_wait: bool,
    out: &mut W,
) -> Result<()>
where
    W: AsyncWrite + Unpin + Send,
{
    let catalog = catalog_from_config(&config)?;
    let base_repo = std::env::current_dir().context("resolving current directory")?;
    let session = bitrouter_substrate::engine::Session::launch(
        &catalog,
        agent_id,
        base_repo,
        worktree_spec(worktree, rm_worktree),
    )
    .await
    .with_context(|| format!("launching acp session for agent '{agent_id}'"))?;
    // Drain telemetry records to stderr (tracing → stderr) so stdout stays clean
    // (NDJSON output). The task ends naturally when the session/pipeline drops,
    // closing the sender and causing `recv()` to return `None`.
    if let Some(mut rx) = session.telemetry() {
        tokio::spawn(async move {
            while let Some(r) = rx.recv().await {
                drain_telemetry_record(r);
            }
        });
    }

    if no_wait {
        // v1 no-wait: emit ack, then shut down immediately. The agent child is
        // killed on shutdown. Callers needing a persistent background session
        // should use `bitrouter acp serve` instead.
        write_ndjson_line(out, &serde_json::json!({ "type": "submitted" })).await?;
        session
            .shutdown()
            .await
            .context("shutting down acp session")?;
        return Ok(());
    }

    prompt_wait(session, text, out).await
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

// ── helpers ───────────────────────────────────────────────────────────────────

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

/// Build the optional [`WorktreeSpec`] from the CLI's `--worktree` /
/// `--rm-worktree` flags. Retention is the default: removal destroys the
/// agent's uncommitted work, so it is strictly opt-in.
fn worktree_spec(worktree: Option<&str>, rm_worktree: bool) -> Option<WorktreeSpec> {
    worktree.map(|name| WorktreeSpec {
        name: name.to_string(),
        remove_on_shutdown: rm_worktree,
    })
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
