//! `bitrouter tui` — in-process multi-agent manager (M1: single agent).
mod event;
mod pump;
mod state;
mod ui;

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
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::tui::event::{AppEvent, Effect, Incoming, PermOption};
use crate::tui::state::{AppState, PaneState, reduce};

/// Launch the TUI against `agent_id`, optionally inside a git worktree `name`.
pub async fn run(agent_id: &str, worktree: Option<&str>) -> Result<()> {
    // ── Load config + build the agent catalog (same path acp_cli uses). ──
    let source = crate::paths::resolve_config(None)?;
    let cfg = crate::paths::load_config(&source).await?;
    let catalog = bitrouter_sdk::acp::ConfigAcpRoutingTable::from_configs(
        cfg.agents.iter().map(|(k, v)| (k.clone(), v.clone())),
    )
    .context("building acp catalog from config.agents")?;
    let base_repo = std::env::current_dir().context("resolving current directory")?;

    // ── Launch the single session. ──
    let session = Session::launch(&catalog, agent_id, base_repo, worktree)
        .await
        .with_context(|| format!("launching acp session for agent '{agent_id}'"))?;
    let record_id = session.state().record_id.clone();
    let session = Arc::new(session);

    // ── Channel + pump. ──
    let (tx, rx) = unbounded_channel::<Incoming>();
    pump::spawn(Arc::clone(&session), record_id.clone(), tx.clone());

    // ── Initial state. ──
    let state = AppState::new(PaneState::new(record_id.clone(), agent_id.to_string()));

    // ── Run with the terminal in raw/alt-screen mode; always restore. ──
    let mut terminal = setup_terminal().context("entering raw mode")?;
    let result = event_loop(&mut terminal, state, rx, &session).await;
    restore_terminal(&mut terminal).ok();

    // ── Teardown the session. ──
    if let Ok(only) = Arc::try_unwrap(session) {
        only.shutdown().await.context("session shutdown")?;
    }
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

/// The core loop: draw, then select terminal input vs pumped session events,
/// reduce each into state, and apply the returned effects.
async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut state: AppState,
    mut rx: UnboundedReceiver<Incoming>,
    session: &Arc<Session>,
) -> Result<()> {
    // Handles to pending permissions, keyed by record_id, resolved on keypress.
    let mut pending: std::collections::HashMap<String, bitrouter_substrate::up::PendingPermission> =
        std::collections::HashMap::new();
    let mut keys = EventStream::new();

    loop {
        terminal.draw(|f| ui::render(&state, f)).context("draw")?;
        if state.should_quit {
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
                    pending.insert(record_id, *p);
                    Some(ev)
                }
                Some(Incoming::Exited { record_id }) => Some(AppEvent::Exited { record_id }),
                None => Some(AppEvent::Key(quit_key())),
            },
        };

        let Some(app_event) = app_event else { continue };
        let effects = reduce(&mut state, &app_event);
        for effect in effects {
            apply_effect(effect, session, &mut pending).await;
        }
    }
}

/// Apply one reducer effect against the live session.
async fn apply_effect(
    effect: Effect,
    session: &Arc<Session>,
    pending: &mut std::collections::HashMap<String, bitrouter_substrate::up::PendingPermission>,
) {
    match effect {
        Effect::Quit => { /* loop exits via should_quit */ }
        Effect::Prompt { text, .. } => {
            // Fire-and-forget: visible output arrives via the updates stream.
            let session = Arc::clone(session);
            tokio::spawn(async move {
                if let Err(e) = session.prompt(&text).await {
                    tracing::warn!(error = %e, "prompt failed");
                }
            });
        }
        Effect::ResolvePermission { record_id, outcome } => {
            if let Some(p) = pending.remove(&record_id) {
                p.resolve(outcome);
            }
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
