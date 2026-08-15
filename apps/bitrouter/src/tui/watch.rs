//! The interactive half of `bitrouter status --watch`.
//!
//! **Unix only, and gated in exactly one place** — `#[cfg(unix)] mod watch;`
//! in [`super`]. An earlier revision scattered `#[cfg(unix)]` across eight
//! individual items here; two were missed and the Windows build broke on a
//! type that existed only under the gate. One module boundary is the whole
//! fix: everything in this file is unix-only by construction, so there is
//! nothing left to forget.
//!
//! What is *not* here, and stays portable: the snapshot poll, the renderers,
//! and the piped one-shot. Redirecting `status --watch` works everywhere.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};

use super::snapshot::{self, Snapshot};
use super::{lifecycle, render};
use crate::metering::store::TimeWindow;

/// How often the store is re-read. Fast enough that a request appears while
/// you are still looking at the agent that made it; slow enough that a second
/// process polling SQLite is not a load source.
const TICK: Duration = Duration::from_secs(1);

/// Cursor state that survives a re-sorting list.
///
/// The naive version — a row index into a vector that changes every second —
/// silently re-aims the cursor at a different request on every tick. Both
/// fields exist to prevent that:
///
/// - `anchor` remembers the *request*, not the row, so a re-sort moves the
///   highlight with the row it was on.
/// - `following` pins auto-scroll off the moment the user navigates away from
///   the live edge, which is the same rule a terminal applies to scrollback.
#[derive(Debug, Default)]
struct Cursor {
    anchor: Option<String>,
    following: bool,
}

impl Cursor {
    fn new() -> Self {
        Self {
            anchor: None,
            following: true,
        }
    }

    /// Row index of the anchored request in the current rows, clamping to the
    /// nearest surviving position when it has aged out of the window.
    fn index(&self, rows: &[crate::metering::store::RequestRow]) -> usize {
        if rows.is_empty() {
            return 0;
        }
        match &self.anchor {
            None => 0,
            Some(id) => rows
                .iter()
                .position(|r| &r.request_id == id)
                .unwrap_or(rows.len() - 1),
        }
    }

    fn move_by(&mut self, delta: isize, rows: &[crate::metering::store::RequestRow]) {
        if rows.is_empty() {
            return;
        }
        let current = self.index(rows) as isize;
        let next = (current + delta).clamp(0, rows.len() as isize - 1) as usize;
        self.anchor = Some(rows[next].request_id.clone());
        // Leaving the newest row means the user is reading history; stop
        // yanking them back to the top on every tick.
        self.following = next == 0;
    }

    /// Jump to the live edge and re-arm auto-follow.
    fn jump_to_live_edge(&mut self) {
        self.anchor = None;
        self.following = true;
    }

    /// Called after every poll: while following, stay pinned to the newest row
    /// rather than drifting down as new requests push the anchored one along.
    fn settle(&mut self, rows: &[crate::metering::store::RequestRow]) {
        if self.following {
            self.anchor = rows.first().map(|r| r.request_id.clone());
        }
    }
}

/// What the last keypress did, echoed in the footer so the view teaches the
/// CLI rather than hiding it.
#[derive(Debug, Default)]
struct Echo(Option<String>);

pub(super) async fn event_loop(
    source: &crate::paths::ConfigSource,
    socket: &Path,
    window: TimeWindow,
) -> Result<()> {
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut events = crossterm::event::EventStream::new();
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut snap = snapshot::poll(source, socket, window, None).await;
    let mut cursor = Cursor::new();
    cursor.settle(&snap.rows);
    let mut echo = Echo::default();
    let mut help = false;
    let mut signals = ShutdownSignals::install()?;

    loop {
        terminal.draw(|frame| draw(frame, &snap, &cursor, &echo, help))?;

        tokio::select! {
            _ = ticker.tick() => {
                snap = snapshot::poll(source, socket, window, None).await;
                cursor.settle(&snap.rows);
            }
            // A signal here would bypass the panic hook, so the loop owns its
            // own exit: SIGTERM/SIGINT end it the same way `q` does, and
            // `run_watch` restores the terminal on the way out.
            _ = signals.recv() => return Ok(()),
            event = futures::StreamExt::next(&mut events) => {
                match event {
                    Some(Ok(Event::Key(key))) => {
                        match handle_key(key, &mut cursor, &snap, &mut help) {
                            Action::Quit => return Ok(()),
                            Action::Reload => {
                                echo.0 = Some(reload(socket).await);
                            }
                            Action::Edit => {
                                // Zero-config installs have no file to edit;
                                // `edit_config` says so rather than opening an
                                // editor on a path that does not exist.
                                let path = match source {
                                    crate::paths::ConfigSource::File(p) => Some(p.clone()),
                                    crate::paths::ConfigSource::Default { .. } => None,
                                };
                                echo.0 = Some(edit_config(path, socket).await);
                                // The child owned the screen; our idea of it is
                                // stale until a full redraw.
                                terminal.clear()?;
                            }
                            Action::None => {}
                        }
                    }
                    Some(Ok(Event::Resize(_, _))) => { terminal.autoresize()?; }
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(()),
                    _ => {}
                }
            }
        }
    }
}

/// Signals that must end the view rather than kill the process outright.
///
/// Raw mode turns Ctrl-C into a key event, but an *external* signal — `kill`,
/// a supervisor stopping the daemon, a closed terminal window sending SIGHUP —
/// bypasses both that and the panic hook. Without this the process dies with
/// the terminal still in raw mode inside the alternate screen, which the
/// module doc rightly calls worse than never drawing at all.
///
/// SIGINT is registered too: `kill -2` is a common supervisor default, and
/// leaving it on the default disposition means the one signal users reach for
/// most is the one that breaks their shell.
pub(crate) struct ShutdownSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}

impl ShutdownSignals {
    /// Register once, before the loop.
    ///
    /// Constructing these per iteration — the obvious `select!` spelling —
    /// silently drops signals: a `Signal` only observes what arrives after it
    /// exists, and the first registration has already replaced the default
    /// disposition. A signal delivered while the loop was awaiting some other
    /// branch (a one-second store poll, an `$EDITOR` that ran for minutes)
    /// would then neither kill the process nor end the loop.
    pub(crate) fn install() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
            hangup: signal(SignalKind::hangup())?,
        })
    }

    /// Resolve when any of them fires.
    pub(crate) async fn recv(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
            _ = self.hangup.recv() => {}
        }
    }
}

enum Action {
    None,
    Quit,
    Reload,
    Edit,
}

fn handle_key(key: KeyEvent, cursor: &mut Cursor, snap: &Snapshot, help: &mut bool) -> Action {
    // Key *release* events exist on some platforms; acting on both would
    // double every keystroke.
    if key.kind != KeyEventKind::Press {
        return Action::None;
    }
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
        KeyCode::Char('q') | KeyCode::Esc if !*help => Action::Quit,
        KeyCode::Esc => {
            *help = false;
            Action::None
        }
        KeyCode::Char('?') => {
            *help = !*help;
            Action::None
        }
        KeyCode::Down | KeyCode::Char('j') => {
            cursor.move_by(1, &snap.rows);
            Action::None
        }
        KeyCode::Up | KeyCode::Char('k') => {
            cursor.move_by(-1, &snap.rows);
            Action::None
        }
        KeyCode::Char('g') | KeyCode::Home => {
            cursor.jump_to_live_edge();
            Action::None
        }
        KeyCode::Char('G') | KeyCode::End => {
            cursor.move_by(snap.rows.len() as isize, &snap.rows);
            Action::None
        }
        KeyCode::Char('r') => Action::Reload,
        KeyCode::Char('e') => Action::Edit,
        _ => Action::None,
    }
}

/// `r` — re-read config in the running daemon. Reversible, already a CLI
/// verb, and the only mutation the view offers. `stop` is deliberately absent:
/// it is the one keypress whose mis-press cannot be undone, since it kills the
/// gateway behind every agent currently running.
async fn reload(socket: &Path) -> String {
    match crate::daemon::send_command(
        socket,
        &crate::daemon::DaemonCommand::Reload { env: vec![] },
    )
    .await
    {
        Ok(crate::daemon::DaemonResponse::Ok) => "ran: bitrouter reload".to_string(),
        Ok(crate::daemon::DaemonResponse::Error { message }) => format!("reload failed: {message}"),
        Ok(other) => format!("reload: unexpected response: {other:?}"),
        Err(e) => format!("reload failed: {e}"),
    }
}

/// `e` — hand the terminal to `$EDITOR` on `bitrouter.yaml`, then reload.
///
/// The view never writes the config itself. That file is hand-maintained and
/// commented, and round-tripping it through a serializer would silently delete
/// every comment in it — so the editor the user already has is the right tool,
/// and this only arranges the handover.
async fn edit_config(path: Option<PathBuf>, socket: &Path) -> String {
    let Some(path) = path else {
        return "no bitrouter.yaml to edit (running on zero-config defaults)".to_string();
    };
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_string());
    let display = path.display().to_string();
    let outcome = lifecycle::suspend(|| async {
        let status = tokio::process::Command::new(&editor)
            .arg(&path)
            .status()
            .await?;
        if status.success() {
            Ok(())
        } else {
            Err(anyhow::anyhow!("{editor} exited with {status}"))
        }
    })
    .await;
    match outcome {
        Ok(()) => format!("ran: {editor} {display} · {}", reload(socket).await),
        Err(e) => format!("edit failed: {e}"),
    }
}

fn draw(frame: &mut ratatui::Frame<'_>, snap: &Snapshot, cursor: &Cursor, echo: &Echo, help: bool) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("bitrouter ", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(snap.state_line()),
        ])),
        header,
    );

    if help {
        frame.render_widget(
            Paragraph::new(vec![
                Line::raw(""),
                Line::raw("  ↑/↓ or j/k   move          g  live edge      G  oldest"),
                Line::raw("  r            reload        e  edit config"),
                Line::raw("  ?            close help    q  quit"),
                Line::raw(""),
                Line::raw("  Every mutating key runs an existing bitrouter command"),
                Line::raw("  and shows it in the footer."),
            ])
            .block(Block::default().borders(Borders::ALL).title(" keys ")),
            body,
        );
    } else if snap.rows.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::raw(match snap.mode() {
                snapshot::Mode::Live => "  waiting for the first request…",
                _ => "  nothing recorded in this window",
            })),
            body,
        );
    } else {
        let rows: Vec<Row> = snap
            .rows
            .iter()
            .map(|r| {
                let cells = render::stream_row(r);
                let style = if r.error.is_some() {
                    Style::new().add_modifier(Modifier::DIM)
                } else {
                    Style::new()
                };
                Row::new(cells.into_iter().map(Cell::from).collect::<Vec<_>>()).style(style)
            })
            .collect();
        let widths = [
            Constraint::Length(8),
            Constraint::Min(16),
            Constraint::Min(10),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Length(9),
            Constraint::Length(8),
            Constraint::Min(6),
        ];
        let table = Table::new(rows, widths)
            .header(
                Row::new(render::STREAM_HEADERS).style(Style::new().add_modifier(Modifier::DIM)),
            )
            .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED));
        let mut state = TableState::default().with_selected(Some(cursor.index(&snap.rows)));
        frame.render_stateful_widget(table, body, &mut state);
    }

    let mut footer_line = render::footer(snap);
    if !cursor.following {
        footer_line.push_str("  ·  paused (g to follow)");
    }
    if let Some(message) = &echo.0 {
        footer_line = format!("↩ {message}");
    }
    frame.render_widget(
        Paragraph::new(Line::raw(footer_line).style(Style::new().add_modifier(Modifier::DIM))),
        footer,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metering::store::RequestRow;

    fn rows(ids: &[&str]) -> Vec<RequestRow> {
        ids.iter()
            .map(|id| RequestRow {
                request_id: (*id).to_string(),
                created_at: "2026-08-10T12:00:00Z".into(),
                model_id: "gpt-5".into(),
                provider_id: "openai".into(),
                prompt_tokens: 1,
                completion_tokens: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                estimated_charge_micro_usd: 1,
                latency_ms: 1,
                error: None,
            })
            .collect()
    }

    #[test]
    fn the_cursor_follows_the_request_not_the_row_index() {
        // The bug this prevents: at 1 Hz the list re-sorts under the cursor,
        // so a row index silently re-aims at a different request each tick.
        let mut cursor = Cursor::new();
        let before = rows(&["c", "b", "a"]);
        cursor.move_by(1, &before); // aim at "b"
        assert_eq!(cursor.index(&before), 1);

        // Two newer requests arrive; "b" is now row 3.
        let after = rows(&["e", "d", "c", "b", "a"]);
        assert_eq!(cursor.index(&after), 3, "the highlight stayed on 'b'");
    }

    #[test]
    fn navigating_away_from_the_live_edge_pauses_auto_follow() {
        let mut cursor = Cursor::new();
        let list = rows(&["c", "b", "a"]);
        assert!(cursor.following, "a fresh view follows");

        cursor.move_by(1, &list);
        assert!(!cursor.following, "reading history must not be interrupted");

        // A tick arrives: following is off, so the anchor must not jump.
        let newer = rows(&["d", "c", "b", "a"]);
        cursor.settle(&newer);
        assert_eq!(cursor.anchor.as_deref(), Some("b"));

        cursor.jump_to_live_edge();
        assert!(cursor.following);
        cursor.settle(&newer);
        assert_eq!(cursor.anchor.as_deref(), Some("d"), "g re-arms following");
    }

    #[test]
    fn an_aged_out_anchor_clamps_instead_of_pointing_nowhere() {
        let mut cursor = Cursor::new();
        let list = rows(&["c", "b", "a"]);
        cursor.move_by(2, &list); // aim at "a"
        // "a" falls out of the window entirely.
        let rolled = rows(&["e", "d", "c"]);
        assert_eq!(cursor.index(&rolled), 2, "clamped to the nearest survivor");
        assert_eq!(cursor.index(&[]), 0, "an empty list is not an index panic");
    }

    #[test]
    fn cursor_movement_is_bounded_at_both_ends() {
        let mut cursor = Cursor::new();
        let list = rows(&["c", "b", "a"]);
        cursor.move_by(-5, &list);
        assert_eq!(cursor.index(&list), 0);
        cursor.move_by(99, &list);
        assert_eq!(cursor.index(&list), 2);
    }

    #[test]
    fn key_releases_do_not_double_every_keystroke() {
        let mut cursor = Cursor::new();
        let mut help = false;
        let snap = Snapshot {
            rows: rows(&["c", "b", "a"]),
            ..Default::default()
        };
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('j'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        handle_key(release, &mut cursor, &snap, &mut help);
        assert_eq!(cursor.index(&snap.rows), 0, "release events are ignored");
    }

    #[test]
    fn esc_closes_help_before_it_quits() {
        // Otherwise the first Esc a user presses to dismiss help exits the
        // view instead.
        let mut cursor = Cursor::new();
        let snap = Snapshot::default();
        let mut help = true;
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert!(matches!(
            handle_key(esc, &mut cursor, &snap, &mut help),
            Action::None
        ));
        assert!(!help);
        assert!(matches!(
            handle_key(esc, &mut cursor, &snap, &mut help),
            Action::Quit
        ));
    }
}
