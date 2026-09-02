//! The interactive half of `bitrouter chat` — the view, the loop, and the keys.
//!
//! `acp_cli` launches the session and decides its routing; from the moment
//! there is a running `Session` and a terminal to draw it on, everything is
//! here. The split follows what the two halves *reach*: the launch preamble
//! holds `Config`, the config source, and the daemon's control socket, while
//! this module holds a journal, a writer, and stdin. It reaches neither the
//! daemon module nor the route-control type — the routing surface arrives as
//! a `SessionProviders`, already built. (Spelled in prose rather than as
//! paths, because the guard in [`crate::chat`] scans this file's source and a
//! doc comment naming them would trip it.)
//!
//! `bitrouter-tui` draws; it does not read. Keys, raw mode, and the session's
//! lifetime belong here, because they are properties of *this* process rather
//! than of anything ACP carries. The three exits are documented on
//! [`crate::chat`].

use anyhow::{Context, Result};
use futures::StreamExt;

use agent_client_protocol::schema::v1::{LlmProtocol, SessionUpdate};

use crate::acp_cli::SessionProviders;

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

/// Draw a launched session until it ends.
///
/// Everything this needs was decided by the caller: the session is running,
/// the routing surface is built, and `routable` is the answer to "would the
/// picker do anything" — asked there because the daemon and the launch token
/// are the caller's to know. From here down it is a journal, a writer, and
/// stdin.
pub(crate) async fn run(
    session: bitrouter_sdk::acp::engine::Session,
    providers: SessionProviders,
    routable: bool,
    via: Option<String>,
) -> Result<()> {
    // The view opens **before** the stdin owner: the writer reads the cursor
    // once, here, and on a real terminal that is a DSR query whose answer a
    // reader already sitting on stdin would take.
    let mut view = bitrouter_tui::view::View::open(via).context("opening the view")?;

    // Raw mode starts here, after the last plain `eprintln!` — a cooked
    // newline in a raw terminal does not return the carriage. From this point
    // the terminal echoes nothing and delivers no SIGINT: both are ours.
    let mut stdin = crate::chat::input::Stdin::open().context("taking the terminal for input")?;
    // The three exits, all landing at `lifecycle::restore()`: `Stdin`'s drop
    // covers the normal one and every `?` on the way out, the panic hook
    // covers the second, and `Shutdown` is the third — a signal runs no Rust
    // the loop controls, so it has to be awaited rather than caught.
    bitrouter_tui::lifecycle::install_panic_restore();
    let mut shutdown = crate::chat::signals::Shutdown::install();

    // Permission requests block the turn until a person answers. They are the
    // journal's, but they arrive on their own channel rather than as updates.
    let (permission_tx, mut permission_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut pending = session.permissions();
    let permissions = tokio::spawn(async move {
        while let Some(request) = pending.next().await {
            if permission_tx.send(request).is_err() {
                break;
            }
        }
    });

    // The **raw** ACP stream, not the translated one: the journal is a
    // protocol client, and translating first would lose exactly the fidelity
    // it exists to retain.
    let mut updates = session.raw_updates();
    let shared = std::sync::Arc::new(std::sync::Mutex::new(
        bitrouter_tui::journal::Journal::default(),
    ));
    // A signal, not a payload: the pump has already applied the update by the
    // time this arrives, so the loop's only job is to decide when to paint.
    let (dirty_tx, mut dirty_rx) = tokio::sync::mpsc::unbounded_channel();
    let pump_journal = std::sync::Arc::clone(&shared);
    let pump = tokio::spawn(async move {
        while let Some(update) = updates.next().await {
            bitrouter_tui::view::lock(&pump_journal).apply(update);
            if dirty_tx.send(()).is_err() {
                return;
            }
        }
    });

    // Every prompt gets its own id so two in a row cannot merge into one run —
    // which they otherwise would, if the agent answered the first with
    // nothing at all.
    let mut prompts = 0_usize;
    let mut abnormal = false;

    'session: loop {
        view.set_input("");
        view.paint(&shared).context("painting a frame")?;
        let read = tokio::select! {
            // Raw mode means nothing is echoed unless the footer echoes it.
            read = stdin.read_line(|echo| match echo {
                crate::chat::input::Echo::Changed(typed) => {
                    view.set_input(typed);
                    view.paint(&shared).map_err(Into::into)
                }
                crate::chat::input::Echo::Redraw(typed) => {
                    view.set_input(typed);
                    view.redraw(&shared).map_err(Into::into)
                }
            }) => read?,
            // A signal at an idle prompt ends the session the same way Ctrl-D
            // does — through teardown, not around it.
            () = shutdown.recv() => break 'session,
        };
        let line = match read {
            crate::chat::input::Prompt::Line(line) => line,
            crate::chat::input::Prompt::End => break,
        };
        view.set_input("");
        // The last turn's word stands until this one starts, so a stop reason
        // is readable for as long as the reader is deciding what to say next
        // rather than for the one frame between them.
        view.clear_notice();
        // A line of exactly `/commands` lists what the agent itself offers.
        // Ours are hardcoded; the agent's arrive on `AvailableCommandsUpdate`
        // and were invisible until now.
        if line.trim() == "/commands" {
            view.notice_lines(bitrouter_tui::render::session::commands(
                bitrouter_tui::view::lock(&shared).commands(),
            ));
            view.paint(&shared).context("painting a frame")?;
            continue;
        }
        // A line of exactly `/route` opens the picker. Only offered when the
        // session can honour it — see `can_reroute`.
        if line.trim() == "/route" {
            if routable {
                pick_provider(&mut view, &shared, &mut stdin, &providers).await?;
            } else {
                view.notice(
                    "this session cannot be rerouted (running direct, or its credential is \
                     its own and cannot be attributed)",
                );
                view.paint(&shared).context("painting a frame")?;
            }
            continue;
        }
        // The prompt joins the document in the user's own voice, so it scrolls
        // with the answer it asked for.
        prompts = prompts.saturating_add(1);
        bitrouter_tui::view::lock(&shared).apply(SessionUpdate::UserMessageChunk(prompt_chunk(
            &line, prompts,
        )));
        view.paint(&shared).context("painting a frame")?;

        let turn = session.prompt(&line);
        tokio::pin!(turn);
        // The tick is the streaming frame budget. It lives here rather than at
        // the prompt because streaming is the only thing that needs
        // coalescing: a keystroke and a permission paint immediately.
        let mut ticker = tokio::time::interval(bitrouter_tui::writer::Schedule::INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut schedule = bitrouter_tui::writer::Schedule::default();
        // Both are handled *after* the `select!` rather than inside a branch:
        // the branch bodies run while the arms' futures are still borrowed,
        // and one of those arms holds `stdin` — which is exactly what
        // answering a permission needs.
        let mut requested = None;
        let mut cancelled = false;
        let mut redraw = false;
        let outcome = loop {
            let mut paint = false;
            tokio::select! {
                update = dirty_rx.recv() => match update {
                    // Already in the journal; the tick decides when it is seen.
                    Some(()) => {
                        schedule.wake(
                            bitrouter_tui::writer::Trigger::Update,
                            std::time::Instant::now(),
                        );
                    }
                    None => continue,
                },
                _ = ticker.tick() => {
                    paint = schedule.wake(
                        bitrouter_tui::writer::Trigger::Tick,
                        std::time::Instant::now(),
                    );
                },
                request = permission_rx.recv() => match request {
                    Some(request) => requested = Some(request),
                    None => continue,
                },
                event = stdin.next_event() => match event {
                    // Ctrl-C and `Esc` during a turn are a cancel, not an
                    // exit: the session survives it and the next prompt is
                    // drawn. No modal can be open here — one would own the key
                    // stream while it ran — so `Esc` has nothing else to close.
                    Some(event) => {
                        cancelled = bitrouter_tui::editor::is_cancel(&event);
                        redraw = bitrouter_tui::editor::is_redraw(&event);
                    }
                    // The terminal went away. Nothing can answer the rest of
                    // this turn, so stop waiting on it.
                    None => cancelled = true,
                },
                result = &mut turn => break Some(result),
                // Mid-turn, a signal still leaves by the front door: the agent
                // is shut down and the terminal restored on the way out.
                () = shutdown.recv() => break 'session,
            }
            if std::mem::take(&mut redraw) {
                view.redraw(&shared).context("repainting")?;
                schedule.wake(
                    bitrouter_tui::writer::Trigger::Key,
                    std::time::Instant::now(),
                );
            } else if paint {
                view.paint(&shared).context("painting a frame")?;
            }
            if let Some(request) = requested.take() {
                if cancelled {
                    // Cancelling with a question outstanding answers it — the
                    // one thing it must never do is leave it to be answered by
                    // a keystroke meant for something else.
                    deny(request);
                } else {
                    answer_permission(&mut view, &shared, &mut stdin, request).await?;
                    schedule.wake(
                        bitrouter_tui::writer::Trigger::Permission,
                        std::time::Instant::now(),
                    );
                }
            }
            if cancelled {
                break None;
            }
        };
        if outcome.is_none() {
            // Tell the agent, not merely ourselves: dropping the turn future
            // stops this side waiting and leaves the agent working.
            let told = session.cancel().await;
            // And leave no question hanging. A permission the agent asked
            // after the cancel has nobody to answer it, and an unanswered
            // question must resolve to deny — never to consent, and never by
            // sitting there until a later keystroke picks an option.
            while let Ok(request) = permission_rx.try_recv() {
                deny(request);
            }
            bitrouter_tui::view::lock(&shared).set_pending_permission(None);
            if let Err(e) = told {
                tracing::warn!(error = %e, "cancelling the turn");
            }
        }
        // Whatever the pump applied between its last signal and the turn
        // resolving is already in the journal; draining the signals is what
        // makes the frame below include it.
        while dirty_rx.try_recv().is_ok() {}
        view.notice(match outcome {
            None => "[turn cancelled]".to_string(),
            Some(Ok(response)) => format!("[{:?}]", response.stop_reason),
            Some(Err(e)) => {
                // A failed turn is the abnormal exit the log tail exists for;
                // it is written after teardown, when the terminal is the
                // shell's again.
                abnormal = true;
                format!("turn failed: {e}")
            }
        });
        // A settled turn is immediate: it is the moment the reader is waiting
        // for, not streaming noise.
        schedule.wake(
            bitrouter_tui::writer::Trigger::TurnSettled,
            std::time::Instant::now(),
        );
        view.paint(&shared).context("painting a frame")?;
    }

    pump.abort();
    permissions.abort();
    // A session whose agent could not be shut down cleanly ended abnormally
    // too, and the log is where the reason is.
    let shutdown = session.shutdown().await;
    abnormal = abnormal || shutdown.is_err();
    // Leave the cursor below the document, then give the terminal back —
    // in that order, because the log tail below is written as ordinary
    // output and must land in a terminal that is the shell's again.
    view.finish().context("closing the view")?;
    drop(stdin);
    if abnormal {
        write_session_log_tail(&mut std::io::stdout())?;
    }
    shutdown.context("shutting down chat session")
}

/// Answer a permission nobody is going to answer.
///
/// A turn can be cancelled with a question outstanding, and a cancelled turn
/// must never resolve to consent. This is the same `Prompt::deny()` path an
/// `Esc` at the prompt takes, so there is exactly one rule for "no answer" and
/// it is the safe one.
fn deny(request: bitrouter_sdk::acp::client::PendingPermission) {
    let prompt = bitrouter_tui::permission::Prompt::new(
        request.tool_call.fields.title.clone(),
        request.tool_call.tool_call_id.0.to_string(),
        request.options.clone(),
    );
    request.resolve(unanswered(&prompt));
}

/// What an unanswered permission resolves to.
///
/// An explicit reject when the agent offered one, so it hears a decision it
/// understands. Otherwise **cancelled** — never a selection, because the only
/// options left would be ones that say yes.
fn unanswered(
    prompt: &bitrouter_tui::permission::Prompt,
) -> agent_client_protocol::schema::v1::RequestPermissionOutcome {
    use agent_client_protocol::schema::v1::{RequestPermissionOutcome, SelectedPermissionOutcome};

    match prompt.deny() {
        Some(id) => RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
        None => RequestPermissionOutcome::Cancelled,
    }
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
/// A permission request is denied rather than asked, because there is nobody
/// to ask: the terminal that would carry the question is the one that isn't
/// there. Denying is the same answer this path gives an unanswerable prompt
/// anywhere else, and it is never mistaken for consent.
pub(crate) async fn chat_plain(session: bitrouter_sdk::acp::engine::Session) -> Result<()> {
    use std::io::Write as _;

    use agent_client_protocol::schema::v1::{RequestPermissionOutcome, SelectedPermissionOutcome};
    use futures::FutureExt as _;
    use tokio::io::AsyncBufReadExt as _;

    let mut out = std::io::stdout();
    let mut journal = bitrouter_tui::journal::Journal::default();
    let mut cache = bitrouter_tui::writer::Cache::default();
    let registry = bitrouter_tui::render::Registry::default();
    // How many rows of the document have already been written.
    let mut written = 0_usize;
    let mut prompts = 0_usize;
    let mut updates = session.raw_updates();
    let mut permissions = session.permissions();
    // The only reader of stdin on this path, and it takes no raw mode — the
    // one that does (`chat::input::Stdin`) needs a terminal, which is exactly
    // what is missing here.
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();

    let ended = loop {
        let Some(line) = lines.next_line().await.context("reading stdin")? else {
            break Ok(());
        };
        if line.trim().is_empty() {
            continue;
        }
        if line.trim() == "/route" {
            writeln!(out, "/route needs a terminal").context("writing to stdout")?;
            continue;
        }
        prompts = prompts.saturating_add(1);
        journal.apply(SessionUpdate::UserMessageChunk(prompt_chunk(
            &line, prompts,
        )));

        let turn = session.prompt(&line);
        tokio::pin!(turn);
        let outcome = loop {
            tokio::select! {
                update = updates.next() => if let Some(update) = update {
                    journal.apply(update);
                },
                request = permissions.next() => if let Some(request) = request {
                    let prompt = bitrouter_tui::permission::Prompt::new(
                        request.tool_call.fields.title.clone(),
                        request.tool_call.tool_call_id.0.to_string(),
                        request.options.clone(),
                    );
                    request.resolve(match prompt.deny() {
                        Some(id) => RequestPermissionOutcome::Selected(
                            SelectedPermissionOutcome::new(id),
                        ),
                        None => RequestPermissionOutcome::Cancelled,
                    });
                    writeln!(out, "  permission denied: no terminal to ask on")
                        .context("writing the permission decision")?;
                },
                result = &mut turn => break result,
            }
        };
        // Whatever the agent emitted between its last update and the turn
        // resolving. There is nothing to flush: the journal has no buffered
        // state, which is the whole reason it replaced `Transcript`.
        while let Some(Some(update)) = updates.next().now_or_never() {
            journal.apply(update);
        }
        let document = cache.document(&journal, &registry, bitrouter_tui::plain::PIPED, &[]);
        bitrouter_tui::plain::write(&mut out, document.get(written..).unwrap_or_default())
            .context("writing the session to stdout")?;
        written = document.len();
        match outcome {
            Ok(response) => {
                writeln!(out, "[{:?}]", response.stop_reason).context("writing the stop reason")?
            }
            Err(e) => {
                writeln!(out, "turn failed: {e}").context("writing the failure")?;
                write_session_log_tail(&mut out)?;
            }
        }
    };

    let shutdown = session.shutdown().await;
    if shutdown.is_err() {
        write_session_log_tail(&mut out)?;
    }
    ended.and(shutdown.context("shutting down chat session"))
}

/// Draw the provider picker and, on a selection, actually change the route.
///
/// The order matters: `providers/set` is issued first and the list is re-read
/// afterwards, so what the user ends up seeing is the route the daemon is
/// serving rather than the one they asked for. A `set` that fails leaves the
/// old route marked, and says why.
async fn pick_provider(
    view: &mut bitrouter_tui::view::View,
    shared: &std::sync::Mutex<bitrouter_tui::journal::Journal>,
    stdin: &mut crate::chat::input::Stdin,
    providers: &SessionProviders,
) -> Result<()> {
    use bitrouter_sdk::acp::down::ProviderSurface;
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

    let listed = providers.list().await;
    let Some(picker) = bitrouter_tui::picker::Picker::open(true, &listed) else {
        view.notice("no routable providers to choose between");
        view.paint(shared).context("painting a frame")?;
        return Ok(());
    };
    // The picker is a footer row for as long as it is open, repainted in
    // place like everything else down there.
    view.open_modal(picker.render());
    view.paint(shared).context("painting a frame")?;

    // Keys come from the session's one stdin owner, which already holds raw
    // mode: a modal that took and dropped it would be a second owner, and the
    // window between them is where a keystroke gets lost.
    let chosen = loop {
        match stdin.next_event().await {
            Some(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                // A control chord is never a choice — Ctrl-C closes the
                // picker instead of selecting whatever `c` happens to be.
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    break None;
                }
                match key.code {
                    KeyCode::Char(c) => {
                        if let Some(id) = picker.choose(c) {
                            break Some(id);
                        }
                    }
                    KeyCode::Esc => break None,
                    _ => {}
                }
            }
            // The terminal went away mid-question; the route is unchanged.
            None => break None,
            Some(_) => {}
        }
    };

    view.close_modal();
    let Some(id) = chosen else {
        view.notice("route unchanged");
        view.paint(shared).context("painting a frame")?;
        return Ok(());
    };

    // Attempt it, then report what is actually in force — never what was asked
    // for. `providers/set` can legitimately refuse.
    let request = agent_client_protocol::schema::v1::SetProviderRequest::new(
        agent_client_protocol::schema::v1::ProviderId::new(id.clone()),
        LlmProtocol::Other(String::new()),
        String::new(),
    );
    match providers.set(request).await {
        Ok(()) => {
            let confirmed = providers.list().await;
            let in_force = confirmed
                .iter()
                .find(|p| p.current.is_some())
                .map(|p| p.provider_id.0.to_string())
                .unwrap_or_else(|| "unchanged".to_string());
            view.notice(format!("route: {in_force}"));
            // The footer names the route for the rest of the session, not just
            // for this frame.
            view.set_route(Some(in_force));
        }
        Err(e) => {
            view.notice(format!("route unchanged: {e}"));
        }
    }
    view.paint(shared).context("painting a frame")
}

/// Draw a permission request and block on the user's answer.
///
/// Reads keys directly rather than through the line-based stdin loop: a
/// permission answer is one keystroke, and requiring enter would make the
/// fast path — deny — slower than the dangerous one.
///
/// Every path that is not an explicit choice resolves to deny or cancel.
/// A prompt that resolved ambiguity as consent would be worse than no prompt.
async fn answer_permission(
    view: &mut bitrouter_tui::view::View,
    shared: &std::sync::Mutex<bitrouter_tui::journal::Journal>,
    stdin: &mut crate::chat::input::Stdin,
    request: bitrouter_sdk::acp::client::PendingPermission,
) -> Result<()> {
    use agent_client_protocol::schema::v1::{RequestPermissionOutcome, SelectedPermissionOutcome};
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

    let prompt = bitrouter_tui::permission::Prompt::new(
        request.tool_call.fields.title.clone(),
        request.tool_call.tool_call_id.0.to_string(),
        request.options.clone(),
    );
    // The journal holds the open question, so every frame drawn while it is
    // open shows it — including one painted by something else entirely.
    bitrouter_tui::view::lock(shared).set_pending_permission(Some(prompt.clone()));
    view.paint(shared).context("painting a frame")?;

    let deny = || unanswered(&prompt);

    // Keys come from the session's one stdin owner. It already holds raw mode
    // for the whole session, so there is no mode to take here and none to
    // leave behind if this future is dropped.
    let outcome = loop {
        match stdin.next_event().await {
            Some(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                // Ctrl-C answers the question the only way an interrupt can be
                // read: no. Passing the chord's letter to `choose` could
                // select an option, which is the one outcome a cancel must
                // never produce.
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    break deny();
                }
                match key.code {
                    KeyCode::Char(c) => {
                        if let Some(id) = prompt.choose(c) {
                            break RequestPermissionOutcome::Selected(
                                SelectedPermissionOutcome::new(id),
                            );
                        }
                    }
                    KeyCode::Esc => break deny(),
                    _ => {}
                }
            }
            // The terminal went away mid-question. Deny — an unanswerable
            // prompt must not become an allow.
            None => break deny(),
            Some(_) => {}
        }
    };

    let chosen = matches!(outcome, RequestPermissionOutcome::Selected(_));
    request.resolve(outcome);
    // Answered: the question stops being asked, and what was decided is said
    // once rather than left on screen as though it were still open.
    bitrouter_tui::view::lock(shared).set_pending_permission(None);
    view.notice(if chosen {
        "permission answered"
    } else {
        "permission denied"
    });
    view.paint(shared).context("painting a frame")
}

#[cfg(test)]
mod cancel_tests {
    use agent_client_protocol::schema::v1::{
        PermissionOption, PermissionOptionId, PermissionOptionKind, RequestPermissionOutcome,
    };

    use super::*;

    fn prompt(options: Vec<PermissionOption>) -> bitrouter_tui::permission::Prompt {
        bitrouter_tui::permission::Prompt::new(Some("Write src/main.rs".to_string()), "t1", options)
    }

    fn option(id: &str, kind: PermissionOptionKind) -> PermissionOption {
        PermissionOption::new(
            PermissionOptionId::new(id.to_string()),
            id.to_string(),
            kind,
        )
    }

    /// The rule a cancelled turn depends on: a question nobody answered
    /// resolves to the agent's own reject option.
    ///
    /// Cancelling is not consenting. A turn can be cancelled with a permission
    /// outstanding — `Esc` or Ctrl-C while the agent is asking — and the
    /// cancel path answers it here rather than leaving it for whichever key
    /// happens to arrive next.
    #[test]
    fn an_unanswered_permission_takes_the_reject_option() {
        let offered = prompt(vec![
            option("allow", PermissionOptionKind::AllowOnce),
            option("allow-always", PermissionOptionKind::AllowAlways),
            option("reject", PermissionOptionKind::RejectOnce),
        ]);
        let chosen = match unanswered(&offered) {
            RequestPermissionOutcome::Selected(selected) => Some(selected.option_id.0.to_string()),
            // `Cancelled`, or a variant added after this build — either way,
            // not a selection.
            _ => None,
        };
        assert_eq!(
            chosen.as_deref(),
            Some("reject"),
            "the reject option, not the first one offered"
        );
    }

    /// And when the agent offered no way to say no, the answer is **cancelled**
    /// — never one of the options, because every option left says yes.
    #[test]
    fn an_unanswered_permission_never_resolves_to_consent() {
        let only_yes = prompt(vec![
            option("allow", PermissionOptionKind::AllowOnce),
            option("allow-always", PermissionOptionKind::AllowAlways),
        ]);
        assert!(
            matches!(unanswered(&only_yes), RequestPermissionOutcome::Cancelled),
            "an unanswerable question must not become an allow"
        );

        // Nor when the agent offered nothing at all.
        assert!(matches!(
            unanswered(&prompt(Vec::new())),
            RequestPermissionOutcome::Cancelled
        ));
    }
}
