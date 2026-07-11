//! `bitrouter tui` — in-process multi-agent manager.
mod event;
mod pump;
mod state;
mod ui;

use std::collections::HashMap;
use std::io::{self, Stdout};
use std::sync::Arc;

use anyhow::{Context, Result};
use bitrouter_substrate::engine::Session;
use bitrouter_substrate::translate::PermissionOutcome;
use crossterm::event::{Event as CtEvent, EventStream};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::tui::event::{AppEvent, Effect, Incoming, PermOption};
use crate::tui::state::{AppState, PaneState, reduce};

/// Launch the TUI with an initial agent; `n` (in AGENT mode) spawns more.
pub async fn run(agent_id: &str, worktree: Option<&str>) -> Result<()> {
    // ── Config + catalog. ──
    let source = crate::paths::resolve_config(None)?;
    let cfg = crate::paths::load_config(&source).await?;
    let catalog = bitrouter_sdk::acp::ConfigAcpRoutingTable::from_configs(
        cfg.agents.iter().map(|(k, v)| (k.clone(), v.clone())),
    )
    .context("building acp catalog from config.agents")?;
    let agent_ids: Vec<String> = cfg.agents.keys().cloned().collect();
    let base_repo = std::env::current_dir().context("resolving current directory")?;

    // ── Initial session. ──
    let session = Session::launch(&catalog, agent_id, base_repo.clone(), worktree)
        .await
        .with_context(|| format!("launching acp session for agent '{agent_id}'"))?;
    let record_id = session.state().record_id.clone();
    let session = Arc::new(session);

    // ── Channel + pump. The loop keeps `tx` to pump agents spawned later. ──
    let (tx, rx) = unbounded_channel::<Incoming>();
    pump::spawn(Arc::clone(&session), record_id.clone(), tx.clone());

    let mut sessions: HashMap<String, Arc<Session>> = HashMap::new();
    sessions.insert(record_id.clone(), session);

    // ── Initial state. ──
    let mut state = AppState::new(PaneState::new(record_id, agent_id.to_string()));
    state.set_available_agents(agent_ids);
    state.set_harness_map(
        cfg.agents
            .iter()
            .map(|(k, v)| (k.clone(), harness_tag(&v.transport)))
            .collect(),
    );
    // Agents route LLM traffic through `bitrouter serve`; probe it once so a
    // missing proxy is an up-front notice instead of silent agent failures.
    if let Some(warning) = probe_serve(&cfg.server.listen).await {
        state.notice = Some(warning);
    }
    // https://no-color.org: any non-empty value disables foreground colors.
    state.no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());

    // ── Run; the loop owns full session teardown. The panic hook guarantees
    // the terminal is restored even if the loop panics mid-draw. ──
    install_panic_restore(restore_terminal);
    let mut terminal = setup_terminal().context("entering raw mode")?;
    let result = event_loop(&mut terminal, state, rx, sessions, &catalog, base_repo, tx).await;
    restore_terminal();
    result
}

/// Terse harness tag for a pane header: the basename of the agent command
/// (e.g. `/usr/local/bin/claude` → `claude`).
fn harness_tag(transport: &bitrouter_sdk::acp::AcpTransport) -> String {
    match transport {
        bitrouter_sdk::acp::AcpTransport::Stdio { command, .. } => std::path::Path::new(command)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| command.clone()),
    }
}

/// Probe the configured `bitrouter serve` listen address; `Some(warning)` when
/// nothing accepts within 500ms. Wildcard binds probe via loopback.
async fn probe_serve(listen: &str) -> Option<String> {
    let addr = listen.replace("0.0.0.0", "127.0.0.1");
    let connect = tokio::net::TcpStream::connect(&addr);
    match tokio::time::timeout(std::time::Duration::from_millis(500), connect).await {
        Ok(Ok(_)) => None,
        _ => Some(format!(
            "bitrouter serve unreachable at {addr} — agents' LLM calls will fail; start it with `bitrouter serve`"
        )),
    }
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

/// Best-effort terminal restore (raw mode off, leave alt-screen, show cursor).
/// Needs no `Terminal` handle so the panic hook can call it from any thread;
/// errors are ignored — by then we're exiting anyway.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let mut out = io::stdout();
    let _ = execute!(out, LeaveAlternateScreen, crossterm::cursor::Show);
}

/// Chain `restore` in front of the current panic hook so a panic anywhere in
/// the TUI (draw, reducer, an effect) restores the user's terminal before the
/// default hook prints the message onto a readable screen.
fn install_panic_restore(restore: fn()) {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        prev(info);
    }));
}

/// Bundles what's needed to launch new sessions from inside the loop.
struct Spawner<'a> {
    catalog: &'a bitrouter_sdk::acp::ConfigAcpRoutingTable,
    base_repo: std::path::PathBuf,
    tx: UnboundedSender<Incoming>,
}

/// In-flight prompt tasks keyed by the owning session's `record_id`, so a
/// pane close can abort+await just that session's tasks.
type PromptTasks = HashMap<String, Vec<tokio::task::JoinHandle<()>>>;

/// The live session registry plus everything else `apply_effect` mutates,
/// bundled so the function stays under clippy's `too_many_arguments` threshold.
struct Runtime<'a> {
    sessions: HashMap<String, Arc<Session>>,
    pending: HashMap<String, bitrouter_substrate::up::PendingPermission>,
    prompt_tasks: PromptTasks,
    spawner: Spawner<'a>,
}

/// The core loop over a registry of sessions. Draws, muxes input vs pumped
/// events, reduces, and applies effects (including async spawn/close).
async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut state: AppState,
    mut rx: UnboundedReceiver<Incoming>,
    sessions: HashMap<String, Arc<Session>>,
    catalog: &bitrouter_sdk::acp::ConfigAcpRoutingTable,
    base_repo: std::path::PathBuf,
    tx: UnboundedSender<Incoming>,
) -> Result<()> {
    let mut rt = Runtime {
        sessions,
        pending: HashMap::new(),
        prompt_tasks: HashMap::new(),
        spawner: Spawner {
            catalog,
            base_repo,
            tx,
        },
    };
    let mut keys = EventStream::new();
    // Drives the running-agent spinner and gives coalesced batches a steady
    // redraw cadence.
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(200));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        if let Err(e) = terminal.draw(|f| ui::render(&mut state, f)) {
            cleanup(&mut rt).await;
            return Err(e).context("draw");
        }
        if state.should_quit {
            cleanup(&mut rt).await;
            return Ok(());
        }

        // Convert the next input/pumped item into a pure AppEvent.
        let app_event: Option<AppEvent> = tokio::select! {
            maybe_key = keys.next() => match maybe_key {
                Some(Ok(CtEvent::Key(k))) => Some(AppEvent::Key(k)),
                // Resize needs no state change: `None` continues to the top of
                // the loop, whose draw autoresizes to the new size. Mouse is
                // deliberately unhandled.
                Some(Ok(_)) => None,
                Some(Err(_)) | None => Some(AppEvent::Key(quit_key())),
            },
            _ = ticker.tick() => Some(AppEvent::Tick),
            maybe_in = rx.recv() => match maybe_in {
                Some(incoming) => convert_incoming(incoming, &mut rt),
                None => Some(AppEvent::Key(quit_key())),
            },
        };

        if let Some(app_event) = app_event {
            let effects = reduce(&mut state, &app_event);
            for effect in effects {
                apply_effect(effect, &mut state, &mut rt).await;
            }
        }
        // Frame coalescing: a chatty agent can emit hundreds of updates in a
        // burst; fold everything already queued before the next draw so the
        // terminal repaints once per batch, not once per chunk. The cap keeps
        // one agent from starving input indefinitely.
        let mut drained = 0;
        while drained < 256 {
            let incoming = match rx.try_recv() {
                Ok(i) => i,
                Err(_) => break,
            };
            drained += 1;
            if let Some(ev) = convert_incoming(incoming, &mut rt) {
                let effects = reduce(&mut state, &ev);
                for effect in effects {
                    apply_effect(effect, &mut state, &mut rt).await;
                }
            }
        }
        // Reap finished prompt tasks so the map stays bounded over a long session.
        rt.prompt_tasks.retain(|_, handles| {
            handles.retain(|h| !h.is_finished());
            !handles.is_empty()
        });
    }
}

/// Convert one pumped `Incoming` into a pure `AppEvent`, stashing resolvable
/// permission handles in the runtime. Shared by the select arm and the
/// coalescing drain.
fn convert_incoming(incoming: Incoming, rt: &mut Runtime<'_>) -> Option<AppEvent> {
    match incoming {
        Incoming::Update { record_id, update } => Some(AppEvent::Update { record_id, update }),
        Incoming::Permission {
            record_id,
            pending: p,
        } => {
            if rt.sessions.contains_key(&record_id) {
                // Stash the handle; hand the reducer display-only data.
                let ev = AppEvent::Permission {
                    record_id: record_id.clone(),
                    title: p.tool_call.fields.title.clone().unwrap_or_default(),
                    diff: p
                        .tool_call
                        .fields
                        .content
                        .as_deref()
                        .and_then(bitrouter_substrate::translate::render_diff),
                    options: perm_options(&p),
                    risk: classify_risk(&p.tool_call.fields, &rt.spawner.base_repo),
                };
                rt.pending.insert(record_id, *p);
                Some(ev)
            } else {
                // Pane already closed; dropping `p` denies the request.
                None
            }
        }
        Incoming::Exited { record_id } => Some(AppEvent::Exited { record_id }),
        Incoming::PromptFailed { record_id, error } => {
            Some(AppEvent::PromptFailed { record_id, error })
        }
    }
}

/// Apply one reducer effect against the live session registry: send prompts,
/// resolve permissions, and (async) spawn/close sessions.
async fn apply_effect(effect: Effect, state: &mut AppState, rt: &mut Runtime<'_>) {
    match effect {
        Effect::Quit => {}
        Effect::Bell => {
            // Ring the terminal bell (BEL). Best-effort; ignore write errors.
            use std::io::Write;
            let mut out = std::io::stdout();
            let _ = out.write_all(b"\x07");
            let _ = out.flush();
        }
        Effect::Prompt { record_id, text } => {
            if let Some(sess) = rt.sessions.get(&record_id) {
                let sess = Arc::clone(sess);
                let tx = rt.spawner.tx.clone();
                let rid = record_id.clone();
                let handle = tokio::spawn(async move {
                    if let Err(e) = sess.prompt(&text).await {
                        // Surface in the pane — a silent failure looks like a
                        // hung agent. Send fails only at teardown; ignore.
                        let _ = tx.send(Incoming::PromptFailed {
                            record_id: rid,
                            error: e.to_string(),
                        });
                    }
                });
                rt.prompt_tasks.entry(record_id).or_default().push(handle);
            }
        }
        Effect::ResolvePermission { record_id, outcome } => {
            if let Some(p) = rt.pending.remove(&record_id) {
                p.resolve(outcome);
            }
        }
        Effect::SpawnAgent { agent_id } => {
            match Session::launch(
                rt.spawner.catalog,
                &agent_id,
                rt.spawner.base_repo.clone(),
                None,
            )
            .await
            {
                Ok(sess) => {
                    let rid = sess.state().record_id.clone();
                    let sess = Arc::new(sess);
                    rt.sessions.insert(rid.clone(), Arc::clone(&sess));
                    pump::spawn(sess, rid.clone(), rt.spawner.tx.clone());
                    let _ = reduce(
                        state,
                        &AppEvent::AgentSpawned {
                            record_id: rid,
                            agent_id,
                        },
                    );
                }
                Err(e) => {
                    let _ = reduce(
                        state,
                        &AppEvent::AgentSpawnFailed {
                            agent_id,
                            error: e.to_string(),
                        },
                    );
                }
            }
        }
        Effect::CloseAgent { record_id } => {
            rt.pending.remove(&record_id);
            if let Some(handles) = rt.prompt_tasks.remove(&record_id) {
                for handle in handles {
                    handle.abort();
                    let _ = handle.await;
                }
            }
            if let Some(sess) = rt.sessions.remove(&record_id) {
                shutdown_session(sess).await;
            }
        }
    }
}

/// Abort and await every in-flight prompt task (across all sessions) so their
/// `Arc<Session>` clones are released before teardown takes sole ownership.
async fn abort_prompt_tasks(tasks: &mut PromptTasks) {
    for (_, handles) in tasks.drain() {
        for handle in handles {
            handle.abort();
            // An aborted handle resolves to a JoinError; discarding is fine.
            let _ = handle.await;
        }
    }
}

/// Abort in-flight prompt tasks, drop pending permissions (defaulting them to
/// Deny in the substrate), and shut down every session. Called on every
/// loop-exit path.
async fn cleanup(rt: &mut Runtime<'_>) {
    abort_prompt_tasks(&mut rt.prompt_tasks).await;
    // Dropping a PendingPermission defaults it to Deny in the substrate.
    rt.pending.clear();
    for (_, sess) in rt.sessions.drain() {
        shutdown_session(sess).await;
    }
}

/// Best-effort session shutdown: take sole ownership and shut down; warn (don't
/// fail) if a clone is still outstanding or shutdown errors.
async fn shutdown_session(sess: Arc<Session>) {
    match Arc::try_unwrap(sess) {
        Ok(only) => {
            if let Err(e) = only.shutdown().await {
                tracing::warn!(error = %e, "session shutdown failed");
            }
        }
        Err(_) => {
            tracing::warn!("session still referenced at teardown; worktree may leak");
        }
    }
}

/// Deterministic risk classification from the tool call's structured fields.
/// Conservative: only reads/searches and writes provably confined to the
/// project tree (`workroot`, which also contains `.bitrouter/worktrees/`)
/// classify Low; deletes, command execution, network access, unknown kinds,
/// and unverifiable writes are High. (Spend-based classification needs
/// metering data that isn't available at permission time.)
fn classify_risk(
    fields: &agent_client_protocol::schema::v1::ToolCallUpdateFields,
    workroot: &std::path::Path,
) -> crate::tui::event::Risk {
    use crate::tui::event::Risk;
    use agent_client_protocol::schema::v1::ToolKind;
    match fields.kind {
        Some(ToolKind::Read | ToolKind::Search | ToolKind::Think | ToolKind::SwitchMode) => {
            Risk::Low
        }
        Some(ToolKind::Edit | ToolKind::Move) => {
            let locations = fields.locations.as_deref().unwrap_or(&[]);
            if !locations.is_empty() && locations.iter().all(|l| l.path.starts_with(workroot)) {
                Risk::Low
            } else {
                // Outside the tree, or no locations to verify against.
                Risk::High
            }
        }
        // Delete, Execute (arbitrary commands), Fetch (network), Other/None.
        _ => Risk::High,
    }
}

/// Map a `PendingPermission`'s options to display data, matching `reduce_key`'s
/// y/a/n handling (allow-once / allow-always / deny).
fn perm_options(p: &bitrouter_substrate::up::PendingPermission) -> Vec<PermOption> {
    use agent_client_protocol::schema::v1::PermissionOptionKind;
    p.options
        .iter()
        .map(|o| {
            let (outcome, label) = match o.kind {
                PermissionOptionKind::AllowOnce => (PermissionOutcome::AllowOnce, "allow"),
                PermissionOptionKind::AllowAlways => {
                    (PermissionOutcome::AllowAlways, "allow always")
                }
                // Any reject/unknown kind maps to Deny — the reducer only offers y/a/n.
                _ => (PermissionOutcome::Deny, "deny"),
            };
            PermOption {
                outcome,
                label: label.to_string(),
            }
        })
        .collect()
}

fn quit_key() -> crossterm::event::KeyEvent {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::tui::event::Risk;
    use agent_client_protocol::schema::v1::{ToolCallLocation, ToolCallUpdateFields, ToolKind};

    fn fields(kind: Option<ToolKind>, paths: &[&str]) -> ToolCallUpdateFields {
        let locations: Vec<ToolCallLocation> =
            paths.iter().map(|p| ToolCallLocation::new(*p)).collect();
        ToolCallUpdateFields::new().kind(kind).locations(locations)
    }

    #[test]
    fn classify_risk_is_deterministic_over_kind_and_paths() {
        let root = std::path::Path::new("/repo");
        // Reads/searches: low regardless of location.
        for kind in [ToolKind::Read, ToolKind::Search, ToolKind::Think] {
            assert_eq!(
                super::classify_risk(&fields(Some(kind), &["/etc/passwd"]), root),
                Risk::Low,
                "{kind:?} is low"
            );
        }
        // Writes inside the tree (including bitrouter worktrees): low.
        assert_eq!(
            super::classify_risk(
                &fields(
                    Some(ToolKind::Edit),
                    &["/repo/src/x.rs", "/repo/.bitrouter/worktrees/w1/y.rs"]
                ),
                root
            ),
            Risk::Low
        );
        // Writes outside the tree: high.
        assert_eq!(
            super::classify_risk(
                &fields(Some(ToolKind::Edit), &["/home/user/.ssh/config"]),
                root
            ),
            Risk::High
        );
        // One outside path taints the whole request.
        assert_eq!(
            super::classify_risk(
                &fields(Some(ToolKind::Edit), &["/repo/src/x.rs", "/tmp/out"]),
                root
            ),
            Risk::High
        );
        // A write with no locations is unverifiable: high.
        assert_eq!(
            super::classify_risk(&fields(Some(ToolKind::Edit), &[]), root),
            Risk::High
        );
        // Deletes, execution, network, unknown: high.
        for kind in [
            ToolKind::Delete,
            ToolKind::Execute,
            ToolKind::Fetch,
            ToolKind::Other,
        ] {
            assert_eq!(
                super::classify_risk(&fields(Some(kind), &["/repo/src/x.rs"]), root),
                Risk::High,
                "{kind:?} is high"
            );
        }
        assert_eq!(
            super::classify_risk(&fields(None, &["/repo/src/x.rs"]), root),
            Risk::High,
            "missing kind is high"
        );
    }

    #[tokio::test]
    async fn probe_serve_ok_when_listening() -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?.to_string();
        assert!(super::probe_serve(&addr).await.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn probe_serve_warns_when_port_closed() -> anyhow::Result<()> {
        // Bind then drop to get a local port that is very likely closed.
        let addr = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            listener.local_addr()?.to_string()
        };
        let warning = super::probe_serve(&addr).await;
        assert!(
            warning.is_some_and(|w| w.contains("bitrouter serve")),
            "closed port must produce an actionable warning"
        );
        Ok(())
    }

    static RESTORED: AtomicBool = AtomicBool::new(false);

    fn mark_restored() {
        RESTORED.store(true, Ordering::SeqCst);
    }

    /// Fault-injection: a panic on any thread must run the chained restore
    /// (terminal un-rawed) before the previous hook prints the message. The
    /// injected restore records instead of touching the real terminal.
    #[test]
    fn panic_hook_restores_terminal_before_reporting() {
        super::install_panic_restore(mark_restored);
        let joined = std::thread::spawn(|| {
            // Deliberate fault injection — the behavior under test is the
            // panic hook itself, so the thread must actually panic.
            std::panic::panic_any("injected tui fault");
        })
        .join();
        assert!(joined.is_err(), "injected fault should panic the thread");
        assert!(
            RESTORED.load(Ordering::SeqCst),
            "panic hook must run the terminal restore"
        );
    }
}
