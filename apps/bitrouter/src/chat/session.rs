//! Plain compatibility rendering and session-log support for `bitrouter chat`.
//!
//! Interactive conversations use the canonical Code driver. This module keeps
//! only the redirected-input/output renderer and the session-log helpers it
//! needs, so legacy scripts retain their stable transcript behavior.

use anyhow::{Context, Result};
use futures::StreamExt;

use agent_client_protocol::schema::v1::SessionUpdate;
use bitrouter_sdk::acp::client::AcpClient;

use crate::chat::effects::Wire;

/// This process's session log, once the subscriber has opened one.
///
/// A global because the path is decided during subscriber init — before any
/// command is dispatched, and the only moment it exists. A session that ends
/// badly needs it much later, and threading it through every launch path to
/// serve the failure case would cost more than it is worth.
static SESSION_LOG: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Record where this process's session log lives. Called once at startup.
pub fn remember_session_log(path: std::path::PathBuf) {
    let _ = SESSION_LOG.set(path);
}

/// Write the end of the session log, naming the file.
///
/// Called only when something went wrong: a permanent pane would cost rows on
/// every session to serve the rare one that fails. An unreadable log is
/// reported as unreadable rather than skipped — "there is no log" is the kind
/// of thing a user needs told, not hidden.
///
/// The path is this process's, so it is read here; the tail is *rendered* by
/// the crate, which is why nothing below names a widget.
fn write_session_log_tail(out: &mut impl std::io::Write) -> Result<()> {
    let Some(path) = SESSION_LOG.get() else {
        return Ok(());
    };
    let log = match std::fs::read_to_string(path) {
        Ok(log) => log,
        Err(e) => format!("(could not read the session log: {e})"),
    };
    bitrouter_tui::plain::write(
        out,
        &bitrouter_tui::log_tail::render(path, &log, bitrouter_tui::log_tail::TAIL_LINES),
    )
    .context("writing the session log tail")
}

/// Show the end of the session log on **stderr**, for a failure that happened
/// before there was a view to draw it in.
///
/// A piped launch that dies during the handshake returns an error before a
/// transcript exists. Its harness child's stderr is in the session file, so
/// writing the tail beside the failure lets the caller see the actionable
/// diagnostic without knowing the file path.
///
/// The launch error outranks this, so a tail that cannot be written is
/// dropped rather than replacing the reason the launch failed.
pub(crate) fn report_failed_launch() {
    let _ = write_session_log_tail(&mut std::io::stderr());
}

/// The user's own prompt, as the update the agent would have sent for it.
///
/// Keyed, because the journal's sticky rule continues an open run on an
/// unkeyed chunk: two prompts in a row with no answer between them would
/// otherwise become one paragraph.
fn prompt_chunk(line: &str, nth: usize) -> agent_client_protocol::schema::v1::ContentChunk {
    use agent_client_protocol::schema::v1::{ContentBlock, ContentChunk, MessageId, TextContent};

    let mut chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(line.to_string())));
    chunk.message_id = Some(MessageId::from(format!("chat:prompt:{nth}")));
    chunk
}

/// The same session, for a stdout that is not a terminal.
///
/// Deliberately not a second renderer: it is the same journal and the same
/// renderers, printed without a backend. What it drops is everything that
/// needs a screen — the footer, the picker, raw mode, painting in place — and
/// what it keeps is the transcript, which is the part a pipe can use.
///
/// The document is written **once per turn**, from the row after the last one
/// written. A pipe cannot take a row back, so nothing is emitted until the
/// turn that produces it has settled — which is also what makes in-place
/// patching arrive as one finished tool call rather than three.
///
/// A permission request is answered by the default headless policy — deny —
/// because there is nobody to ask: the terminal that would carry the question
/// is the one that isn't there. It is the same rule `acp prompt` runs under
/// with no flag, through the same wire, and it is never mistaken for consent.
///
/// # Teardown belongs to the caller
///
/// The client is borrowed, not owned: it is one half of a controller the
/// caller launched and must reap. So this returns when stdin ends and leaves
/// shutdown to `chat`, which is also what makes the harness child's fate the
/// controller's rather than this loop's.
pub(crate) async fn chat_plain(
    client: &AcpClient,
    session_id: &str,
    agent_id: &str,
    recorder: Option<std::sync::Arc<bitrouter_telemetry::otel::acp::AcpSpanRecorder>>,
    surface: crate::actions::session::SessionSurface<'_>,
) -> Result<()> {
    use std::io::Write as _;

    use futures::FutureExt as _;
    use tokio::io::AsyncBufReadExt as _;

    let crate::actions::session::SessionSurface {
        commands,
        prompt_commands,
        ports,
    } = surface;

    let mut out = std::io::stdout();
    let mut transcript = bitrouter_tui::plain::Transcript::default();
    let policy = bitrouter_tui::permission::Policy::default();
    let mut wire = Wire::new(client, session_id);
    let mut prompts = 0_usize;
    let mut updates = client.subscribe_raw_updates();
    let mut permissions = client.subscribe_permissions();
    // The only reader of stdin on this path. It takes no raw mode because a
    // redirected stream has no terminal; canonical Code owns terminal input.
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();

    // The loop is the tail: with teardown moved to the caller there is
    // nothing left to do after it ends.
    loop {
        let Some(line) = lines.next_line().await.context("reading stdin")? else {
            break Ok(());
        };
        if line.trim().is_empty() {
            continue;
        }
        // The same resolver the terminal runs, so a name means one thing on
        // both. What differs is only what can be *done* with the result: the
        // picker and the journal need keys and a screen.
        let line = match bitrouter_tui::machine::resolve(&commands, &prompt_commands, &line) {
            bitrouter_tui::machine::Resolution::Owned { action, .. } => {
                let name = commands
                    .iter()
                    .find(|command| command.action == action)
                    .map_or(action, |command| command.name);
                writeln!(out, "/{name} needs a terminal").context("writing to stdout")?;
                continue;
            }
            // The same ports, rendered the same way; a pipe gets the bytes
            // where a terminal gets a notice.
            bitrouter_tui::machine::Resolution::Action { action, args } => {
                match ports.run(action, &args).await {
                    Ok(report) => out
                        .write_all(
                            &crate::output::Output::new(crate::output::Format::Human)
                                .render_to_vec(report.as_ref()),
                        )
                        .context("writing to stdout")?,
                    Err(error) => writeln!(out, "{error}").context("writing to stdout")?,
                }
                continue;
            }
            bitrouter_tui::machine::Resolution::Unavailable(reason) => {
                writeln!(out, "{reason}").context("writing to stdout")?;
                continue;
            }
            // Expanded identically here and in the terminal, through the one
            // resolver: a config command means the same thing on both.
            bitrouter_tui::machine::Resolution::Expand(prompt)
            | bitrouter_tui::machine::Resolution::Prompt(prompt) => prompt,
        };
        prompts = prompts.saturating_add(1);
        transcript.apply(SessionUpdate::UserMessageChunk(prompt_chunk(
            &line, prompts,
        )));

        let started = std::time::Instant::now();
        let turn = client.prompt(session_id, &line);
        tokio::pin!(turn);
        let outcome = loop {
            tokio::select! {
                update = updates.next() => if let Some(update) = update {
                    transcript.apply(update);
                },
                request = permissions.next() => if let Some(request) = request {
                    let (decision, prompt) = wire.answer(request, &policy).await;
                    writeln!(out, "  permission {decision}: {}", prompt.title())
                        .context("writing the permission decision")?;
                },
                result = &mut turn => break result,
            }
        };
        // Whatever the agent emitted between its last update and the turn
        // resolving. There is nothing to flush: the journal has no buffered
        // state, which is the whole reason it replaced `Transcript`.
        while let Some(Some(update)) = updates.next().now_or_never() {
            transcript.apply(update);
        }
        bitrouter_tui::plain::write(&mut out, &transcript.unwritten())
            .context("writing the session to stdout")?;
        match outcome {
            Ok(response) => {
                // The same record `prompt` reports, from the same round-trip:
                // the engine's pipeline hook used to produce these for both.
                crate::acp_cli::report_turn(
                    client,
                    agent_id,
                    recorder.as_ref(),
                    &response,
                    started.elapsed(),
                );
                writeln!(out, "[{:?}]", response.stop_reason).context("writing the stop reason")?
            }
            Err(e) => {
                writeln!(out, "turn failed: {e}").context("writing the failure")?;
                write_session_log_tail(&mut out)?;
            }
        }
    }
}
