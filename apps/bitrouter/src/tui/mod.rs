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

    // ── Run; the loop owns full session teardown. ──
    let mut terminal = setup_terminal().context("entering raw mode")?;
    let result = event_loop(&mut terminal, state, rx, sessions, &catalog, base_repo, tx).await;
    restore_terminal(&mut terminal).ok();
    result
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
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

    loop {
        if let Err(e) = terminal.draw(|f| ui::render(&state, f)) {
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
                Some(Ok(_)) => None,           // resize/mouse ignored in M1
                Some(Err(_)) | None => Some(AppEvent::Key(quit_key())),
            },
            maybe_in = rx.recv() => match maybe_in {
                Some(Incoming::Update { record_id, update }) => {
                    Some(AppEvent::Update { record_id, update })
                }
                Some(Incoming::Permission { record_id, pending: p }) => {
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
                        };
                        rt.pending.insert(record_id, *p);
                        Some(ev)
                    } else {
                        // Pane already closed; dropping `p` denies the request.
                        None
                    }
                }
                Some(Incoming::Exited { record_id }) => Some(AppEvent::Exited { record_id }),
                None => Some(AppEvent::Key(quit_key())),
            },
        };

        let Some(app_event) = app_event else { continue };
        let effects = reduce(&mut state, &app_event);
        for effect in effects {
            apply_effect(effect, &mut state, &mut rt).await;
        }
        // Reap finished prompt tasks so the map stays bounded over a long session.
        rt.prompt_tasks.retain(|_, handles| {
            handles.retain(|h| !h.is_finished());
            !handles.is_empty()
        });
    }
}

/// Apply one reducer effect against the live session registry: send prompts,
/// resolve permissions, and (async) spawn/close sessions.
async fn apply_effect(effect: Effect, state: &mut AppState, rt: &mut Runtime<'_>) {
    match effect {
        Effect::Quit => {}
        // Wired in M2b Task 6 (actual terminal bell).
        Effect::Bell => {}
        Effect::Prompt { record_id, text } => {
            if let Some(sess) = rt.sessions.get(&record_id) {
                let sess = Arc::clone(sess);
                let handle = tokio::spawn(async move {
                    if let Err(e) = sess.prompt(&text).await {
                        tracing::warn!(error = %e, "prompt failed");
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
