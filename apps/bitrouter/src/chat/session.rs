//! The interactive half of `bitrouter chat` — the view, the loop, and the keys.
//!
//! `acp_cli` launches the session and decides its routing; from the moment
//! there is a controlled session and a terminal to draw it on, everything is
//! here. The split follows what the two halves *reach*: the launch preamble
//! holds `Config`, the config source, and the daemon's control socket, while
//! this module holds a journal, a writer, and stdin. It reaches neither the
//! daemon module nor the credential binding — the routing surface arrives
//! over ACP, as `_bitrouter/route/*` on the shared client, and the one handle
//! it holds on the launch half is the session's own teardown. (Spelled in
//! prose rather than as paths, because the guard in [`crate::chat`] scans
//! this file's source and a doc comment naming them would trip it.)
//!
//! `bitrouter-tui` draws; it does not read. Keys, raw mode, and the session's
//! lifetime belong here, because they are properties of *this* process rather
//! than of anything ACP carries. The three exits are documented on
//! [`crate::chat`].

use anyhow::{Context, Result};
use futures::StreamExt;

use agent_client_protocol::schema::v1::SessionUpdate;
use bitrouter_sdk::acp::client::{AcpClient, RouteError, RouteMethod};
use bitrouter_tui::permission::Prompt as PermissionPrompt;

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

/// Whether this session's route can be changed from the picker.
///
/// The contract's three-condition gate, asked of what the controller
/// advertised at handshake and of nothing else: the picker lists with one
/// method and sets with another, so both must be there. A controller with no
/// trusted local binding — `--direct`, an explicit `--base-url` — advertises
/// neither, and the picker is then absent rather than dead.
pub(crate) fn can_reroute(client: &AcpClient) -> bool {
    let capability = client.route_control();
    capability.allows(RouteMethod::List) && capability.allows(RouteMethod::Set)
}

/// Draw a launched session until it ends.
///
/// Everything this needs was decided by the caller: the harness is running
/// behind its controller, the session is open, and the client has read what
/// the controller advertises. From here down it is a journal, a writer, and
/// stdin.
///
/// # Teardown is the session's, and there is exactly one of it
///
/// Every exit — Ctrl-D or Ctrl-C at the prompt, stdin ending, a signal, and
/// every `?` inside [`drive`] — leaves through `session.shutdown()`, which is
/// what confirms the harness child was reaped and revokes the controller
/// credential. That is why the loop lives in `drive` and its error is
/// *carried* back here rather than returned: a `?` that walked out of this
/// function would give the terminal back (`Stdin`'s drop does that) and leave
/// the child unreaped.
///
/// The terminal is given back *after* teardown, so the log tail written for an
/// abnormal end lands in a terminal that is the shell's again.
pub(crate) async fn run(
    session: &mut crate::acp_cli::ControlledSession,
    session_id: &str,
    agent_id: &str,
    recorder: Option<std::sync::Arc<bitrouter_observe::acp::AcpSpanRecorder>>,
    via: Option<String>,
) -> Result<()> {
    let (mut view, mut stdin) = match open_terminal(via) {
        Ok(terminal) => terminal,
        // Nothing was drawn, but the harness is already running and its
        // credential is already issued. Both are still ours to give back.
        Err(error) => {
            session.shutdown().await;
            return Err(error);
        }
    };

    let outcome = drive(
        &mut view,
        &mut stdin,
        &session.client,
        session_id,
        agent_id,
        recorder,
    )
    .await;

    // A session whose agent could not be shut down cleanly ended abnormally
    // too, and the log is where the reason is. This is also where the child is
    // confirmed reaped and the controller credential revoked.
    let clean = session.shutdown().await;
    // Leave the cursor below the document, then give the terminal back — in
    // that order, because the log tail below is written as ordinary output and
    // must land in a terminal that is the shell's again.
    let finished = view.finish().context("closing the view");
    drop(stdin);
    if !clean || matches!(outcome, Ok(true) | Err(_)) {
        write_session_log_tail(&mut std::io::stdout())?;
    }
    // The drive's own failure is the most specific thing that went wrong, so
    // it outranks both of the others.
    outcome?;
    finished?;
    if clean {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "shutting down chat session: teardown did not confirm; see the session log"
        ))
    }
}

/// Take the screen, then the keyboard.
///
/// In that order: the writer reads the cursor once at construction, and on a
/// real terminal that is a DSR query whose answer a reader already sitting on
/// stdin would take.
fn open_terminal(
    via: Option<String>,
) -> Result<(bitrouter_tui::view::View, crate::chat::input::Stdin)> {
    let view = bitrouter_tui::view::View::open(via).context("opening the view")?;
    // Raw mode starts here, after the last plain `eprintln!` — a cooked
    // newline in a raw terminal does not return the carriage. From this point
    // the terminal echoes nothing and delivers no SIGINT: both are ours.
    let stdin = crate::chat::input::Stdin::open().context("taking the terminal for input")?;
    Ok((view, stdin))
}

/// Await the turn if there is one, and never resolve if there is not.
///
/// The same idiom [`crate::chat::signals::Shutdown`] uses for a missing signal
/// registration, and for the same reason: an arm with nothing behind it must
/// be *quiet*, and neither may reach for `.unwrap()` to say so.
///
/// `Pin<Box<dyn Future>>` is `Unpin`, so `&mut F` is itself a future, and
/// dropping that borrow when the `select!` loses the race does **not** drop
/// the boxed future. That is what makes the arm cancel-safe.
async fn in_flight<F: std::future::Future + Unpin>(slot: &mut Option<F>) -> F::Output {
    match slot {
        Some(turn) => turn.await,
        None => std::future::pending().await,
    }
}

/// The one loop: one `select!`, one state machine, one exit.
///
/// Returns whether the session ended abnormally — a failed turn, which is what
/// the session-log tail exists for. Everything it can fail at is a `?`, and
/// every one of those is caught by [`run`] rather than escaping teardown.
///
/// # Why every arm is cancel-safe
///
/// A flat `select!` is only correct if all of them are, so: `shutdown.recv()`
/// selects over `tokio::signal`'s own receivers, registered once at install;
/// the three channel receives are `UnboundedReceiver::recv`, which is why
/// stdin is a task-plus-channel rather than an inline `EventStream`;
/// `in_flight` is covered above; and `Interval::tick` is documented
/// cancel-safe. The select is deliberately **unbiased**: under a saturating
/// update stream a biased one would starve every arm below the journal's,
/// including the signal arm.
async fn drive(
    view: &mut bitrouter_tui::view::View,
    stdin: &mut crate::chat::input::Stdin,
    client: &AcpClient,
    session_id: &str,
    agent_id: &str,
    recorder: Option<std::sync::Arc<bitrouter_observe::acp::AcpSpanRecorder>>,
) -> Result<bool> {
    use agent_client_protocol::schema::v1::{
        ContentBlock, PromptRequest, PromptResponse, SessionId, TextContent,
    };
    use bitrouter_tui::machine::{Action, Effect, Notice, Routes, State, step};
    use bitrouter_tui::writer::{Schedule, Trigger};

    // The three exits, all landing at `lifecycle::restore()`: `Stdin`'s drop
    // covers the normal one and every `?` on the way out, the panic hook
    // covers the second, and `Shutdown` is the third — a signal runs no Rust
    // the loop controls, so it has to be awaited rather than caught.
    bitrouter_tui::lifecycle::install_panic_restore();
    let mut shutdown = crate::chat::signals::Shutdown::install();

    // Permission requests block the turn until a person answers. They arrive
    // on their own channel rather than as updates, because they are requests.
    let (permission_tx, mut permission_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut pending = client.subscribe_permissions();
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
    let mut updates = client.subscribe_raw_updates();
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

    let mut state = State::new(can_reroute(client));
    let mut schedule = Schedule::default();
    // One ticker for the session rather than one per turn. The tick arm is
    // gated on `state.streaming()`, so `tokio` never polls it at an idle
    // prompt and the timer is never armed; `Delay` then makes the first tick
    // of a new turn immediate, which is what is wanted.
    let mut ticker = tokio::time::interval(Schedule::INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Requests shown and not yet answered, keyed the way the machine routes
    // answers. Entries are **removed** when answered: a retained entry holds a
    // strong handle on the resolver, and the client's ledger holds a weak one
    // on purpose so that a dropped request still denies itself.
    let mut outstanding: std::collections::HashMap<
        String,
        bitrouter_sdk::acp::client::PendingPermission,
    > = std::collections::HashMap::new();
    // The turn in flight, if any. It cannot live in `State`: that is plain
    // data the machine mutates through `&mut`, and a future polled across
    // iterations cannot go there.
    let mut turn: Option<
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<PromptResponse>> + '_>>,
    > = None;
    let mut started = std::time::Instant::now();
    // Answers produced by an effect that awaited inline, dispatched before the
    // select is entered again.
    let mut queued: std::collections::VecDeque<Action> = std::collections::VecDeque::new();
    let mut abnormal = false;
    let mut exit = false;

    // The first frame: an empty prompt, before any key.
    view.set_input("");
    view.paint(&shared).context("painting a frame")?;

    while !exit {
        let action = match queued.pop_front() {
            Some(action) => action,
            None => tokio::select! {
                // A signal leaves by the front door from every phase: the
                // agent is shut down and the terminal restored on the way out.
                () = shutdown.recv() => Action::Signal,
                event = stdin.next_event() => match event {
                    Some(event) => Action::Key(event),
                    None => Action::InputClosed,
                },
                // Already in the journal; the machine and the schedule decide
                // when it is seen.
                Some(()) = dirty_rx.recv() => Action::Dirty,
                Some(request) = permission_rx.recv() => {
                    let prompt = prompt_of(&request);
                    outstanding.insert(request.request_id.clone(), request);
                    Action::Permission(prompt)
                }
                result = in_flight(&mut turn) => match result {
                    Ok(response) => {
                        // The same record `prompt` and the piped path report,
                        // from the same round-trip.
                        crate::acp_cli::report_turn(
                            client,
                            agent_id,
                            recorder.as_ref(),
                            &response,
                            started.elapsed(),
                        );
                        Action::TurnEnded(Ok(response.stop_reason))
                    }
                    // A failed turn is the abnormal exit the log tail exists
                    // for; it is written after teardown, when the terminal is
                    // the shell's again.
                    Err(error) => {
                        abnormal = true;
                        Action::TurnEnded(Err(format!("{error}")))
                    }
                },
                _ = ticker.tick(), if state.streaming() => Action::Tick,
            },
        };
        // Cleared here rather than in the arm above: an arm's body runs while
        // the futures the `select!` polled are still borrowed, and one of them
        // is holding `&mut turn`.
        if matches!(action, Action::TurnEnded(_)) {
            turn = None;
        }

        for effect in step(&mut state, action) {
            match effect {
                Effect::Paint(trigger) => {
                    if schedule.wake(trigger, std::time::Instant::now()) {
                        view.paint(&shared).context("painting a frame")?;
                    }
                }
                Effect::Redraw => {
                    view.redraw(&shared).context("repainting")?;
                    schedule.wake(Trigger::Key, std::time::Instant::now());
                }
                // Raw mode means nothing is echoed unless the footer echoes it.
                Effect::Echo => view.set_input(state.editor.line()),
                Effect::Notice(Notice::Say(text)) => view.notice(text),
                // Rendered here rather than in the machine, because it is the
                // journal that holds the agent's own command list and the
                // machine never reads the journal.
                Effect::Notice(Notice::Commands) => {
                    view.notice_lines(bitrouter_tui::render::session::commands(
                        bitrouter_tui::view::lock(&shared).commands(),
                    ));
                }
                Effect::ClearNotice => view.clear_notice(),
                Effect::Modal(Some(row)) => view.open_modal(row),
                Effect::Modal(None) => view.close_modal(),
                Effect::ShowPermission(prompt) => view.set_permission(prompt),
                Effect::Resolve { id, outcome } => {
                    if let Some(request) = outstanding.remove(&id) {
                        request.resolve(outcome);
                    }
                }
                Effect::Prompt { line, nth } => {
                    // The prompt joins the document in the user's own voice, so
                    // it scrolls with the answer it asked for.
                    bitrouter_tui::view::lock(&shared)
                        .apply(SessionUpdate::UserMessageChunk(prompt_chunk(&line, nth)));
                    started = std::time::Instant::now();
                    // The typed form rather than the text convenience: this
                    // one takes the request by value, so the future it returns
                    // borrows the client and nothing else — which is what lets
                    // it outlive the effect that started it.
                    turn = Some(Box::pin(client.prompt_typed(PromptRequest::new(
                        SessionId::new(session_id),
                        vec![ContentBlock::Text(TextContent::new(line))],
                    ))));
                }
                Effect::Cancel => {
                    // Tell the agent, not merely ourselves: dropping the turn
                    // future stops this side waiting and leaves the agent
                    // working.
                    turn = None;
                    let told = client.cancel(session_id).await;
                    // And leave no question hanging. The machine has already
                    // answered whatever it was holding; this answers anything
                    // the client emitted that never reached it — while the
                    // connection is still live, rather than at teardown.
                    outstanding.clear();
                    client.deny_outstanding_permissions();
                    if let Err(error) = told {
                        tracing::warn!(%error, "cancelling the turn");
                    }
                }
                // Awaited inline, exactly as the picker's own loop awaited it:
                // both are reachable only from an idle prompt, where no turn
                // is streaming and nothing else needs the loop.
                Effect::ListRoutes => {
                    queued.push_back(Action::Routes(match client.route_list(session_id).await {
                        Ok(listed) => Ok(Routes {
                            available: listed.available,
                            current: listed.current,
                        }),
                        Err(error) => Err(format!("{error}")),
                    }))
                }
                // Typed by the client, so a refused route and a vanished
                // binding read differently without parsing text.
                Effect::SetRoute(route) => queued.push_back(Action::Routed(
                    match client.route_set(session_id, &route).await {
                        Ok(in_force) => Ok(in_force),
                        Err(RouteError::InvalidRoute(message)) => {
                            Err(format!("route unchanged: {message}"))
                        }
                        Err(RouteError::Unavailable(message)) => Err(format!(
                            "route unchanged: route control is unavailable ({message})"
                        )),
                        Err(RouteError::Other(error)) => Err(format!("route unchanged: {error:#}")),
                    },
                )),
                Effect::RouteInForce(route) => view.set_route(route),
                Effect::Exit => exit = true,
            }
        }
    }

    pump.abort();
    permissions.abort();
    Ok(abnormal)
}

/// The prompt a pending request is drawn and answered as.
fn prompt_of(request: &bitrouter_sdk::acp::client::PendingPermission) -> PermissionPrompt {
    PermissionPrompt::new(
        request.request_id.clone(),
        request.tool_call.fields.title.clone(),
        request.tool_call.tool_call_id.0.to_string(),
        request.options.clone(),
    )
}

/// Answer a permission nobody is going to answer.
///
/// A turn can be cancelled with a question outstanding, and a cancelled turn
/// must never resolve to consent. This is the same `Prompt::unanswered()` path
/// an `Esc` at the prompt takes, so there is exactly one rule for "no answer"
/// and it is the safe one.
fn deny(request: bitrouter_sdk::acp::client::PendingPermission) {
    let outcome = prompt_of(&request).unanswered();
    request.resolve(outcome);
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
    recorder: Option<std::sync::Arc<bitrouter_observe::acp::AcpSpanRecorder>>,
) -> Result<()> {
    use std::io::Write as _;

    use futures::FutureExt as _;
    use tokio::io::AsyncBufReadExt as _;

    let mut out = std::io::stdout();
    let mut journal = bitrouter_tui::journal::Journal::default();
    let mut cache = bitrouter_tui::writer::Cache::default();
    let registry = bitrouter_tui::render::Registry::default();
    // How many rows of the document have already been written.
    let mut written = 0_usize;
    let mut prompts = 0_usize;
    let mut updates = client.subscribe_raw_updates();
    let mut permissions = client.subscribe_permissions();
    // The only reader of stdin on this path, and it takes no raw mode — the
    // one that does (`chat::input::Stdin`) needs a terminal, which is exactly
    // what is missing here.
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
        if line.trim() == "/route" {
            writeln!(out, "/route needs a terminal").context("writing to stdout")?;
            continue;
        }
        prompts = prompts.saturating_add(1);
        journal.apply(SessionUpdate::UserMessageChunk(prompt_chunk(
            &line, prompts,
        )));

        let started = std::time::Instant::now();
        let turn = client.prompt(session_id, &line);
        tokio::pin!(turn);
        let outcome = loop {
            tokio::select! {
                update = updates.next() => if let Some(update) = update {
                    journal.apply(update);
                },
                request = permissions.next() => if let Some(request) = request {
                    deny(request);
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
