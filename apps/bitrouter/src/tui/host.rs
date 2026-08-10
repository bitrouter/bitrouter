//! `bitrouter launch --tui` — host a harness inside BitRouter's emulator so a
//! status row can survive underneath it.
//!
//! ## Why an emulator rather than a reserved line
//!
//! The cheap design is `DECSTBM`: reserve the bottom row and let the harness
//! write straight to the real tty — full fidelity, native scrollback, no
//! emulator. It does not survive the requirement. [`super::term`] tracks
//! alt-screen state (`\x1b[?1049h`) because some harnesses render inline on the
//! main screen and others take the alternate screen, and an alt-screen app owns
//! the whole display and clobbers a reserved line. Uniformity across all eight
//! catalog harnesses is the product requirement, so BitRouter owns the screen
//! and composites. That cost buys uniformity and nothing else.
//!
//! ## What the user gives up
//!
//! Scrollback moves here from their terminal: `Cmd-F` stops finding agent
//! output, and copy routes through the OSC-52 relay. That is daily friction,
//! which is why plain `launch` stays the default and this is opt-in — and why
//! every error out of this module names `launch` without `--tui`.
//!
//! ## Zero keybindings
//!
//! Every keystroke goes to the child. Any prefix key would collide with
//! something across eight harnesses — it is why tmux users remap `C-b` — and
//! none is needed: `Ctrl-C` reaches the child, and the child exiting ends the
//! session. The one gesture that branches is the wheel, and [`super::term`]
//! already owns that rule (forward when the inner app enabled mouse reporting,
//! else page our own scrollback).

use std::io::Write;

use anyhow::{Context, Result};
use crossterm::event::{Event, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use tokio::sync::mpsc::unbounded_channel;

use super::pty::{HostEvent, PtyLaunch, PtyPane};
use super::snapshot::{self, Snapshot};
use super::{lifecycle, render};
use crate::metering::store::TimeWindow;

/// How often the status row re-reads the store. Matches `status --watch`: the
/// two surfaces share a renderer, so they should not disagree on freshness.
const TICK: std::time::Duration = std::time::Duration::from_secs(1);

/// Everything the host needs that is not the child process itself.
pub struct HostContext<'a> {
    /// Config source, for the metering read behind the status row.
    pub source: &'a crate::paths::ConfigSource,
    /// Daemon control socket, for the live/absent dot.
    pub socket: std::path::PathBuf,
    /// The harness being hosted — named in the status row.
    pub harness: &'static crate::harness::Harness,
    /// This launch's attribution token, when BitRouter minted the credential.
    /// `None` scopes the row to the daemon and says so (#795).
    pub launch_id: Option<String>,
    /// The model pinned for this launch, if any.
    pub model: Option<String>,
}

/// Host `command args…` with the status row pinned to the bottom line.
///
/// Returns the child's exit code so the caller can propagate it — a launcher
/// must be transparent: the shell sees the agent's status, not BitRouter's.
pub async fn run(
    command: &str,
    args: &[String],
    env: &[(String, String)],
    ctx: HostContext<'_>,
) -> Result<i32> {
    let (cols, rows) = crossterm::terminal::size().context("reading terminal size")?;
    // A pty with no window size set (CI, `script`, some multiplexer edge
    // cases) reports 0x0. That is "unknown", not "small", and the two need
    // different advice — sizing a hosted child from a bogus 0x0 would render
    // garbage rather than fail.
    if cols == 0 || rows == 0 {
        anyhow::bail!(
            "this terminal did not report a size, so a harness cannot be hosted in it. \
             Run `bitrouter launch` without `--tui`."
        );
    }
    if rows < 3 || cols < 20 {
        anyhow::bail!(
            "terminal is too small to host a harness ({cols}x{rows}); needs at least 20x3. \
             Run `bitrouter launch` without `--tui`."
        );
    }

    lifecycle::enter(lifecycle::Input::Full)?;
    lifecycle::install_panic_restore();
    let outcome = hosted_loop(command, args, env, &ctx, cols, rows).await;
    lifecycle::restore();
    outcome
}

/// The pane keeps every row but the last; the status row owns the last.
fn pane_rows(rows: u16) -> u16 {
    rows.saturating_sub(1).max(1)
}

async fn hosted_loop(
    command: &str,
    args: &[String],
    env: &[(String, String)],
    ctx: &HostContext<'_>,
    cols: u16,
    rows: u16,
) -> Result<i32> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let (tx, mut rx) = unbounded_channel();
    let mut pane = PtyPane::spawn(
        &PtyLaunch {
            command,
            args,
            env,
            cwd: &cwd,
        },
        cols,
        pane_rows(rows),
        tx,
    )
    .with_context(|| {
        format!("hosting '{command}' in the BitRouter terminal — run `bitrouter launch` without `--tui` to launch it directly")
    })?;

    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut events = crossterm::event::EventStream::new();
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let window = TimeWindow::Today;
    let mut snap = snapshot::poll(ctx.source, &ctx.socket, window).await;
    let mut exit_code: Option<i32> = None;

    loop {
        draw(&mut terminal, &mut pane, &snap, ctx)?;

        tokio::select! {
            _ = ticker.tick() => {
                snap = snapshot::poll(ctx.source, &ctx.socket, window).await;
            }
            // The child owns Ctrl-C; these are the signals that would
            // otherwise leave a raw-mode shell behind, since a panic hook
            // never runs for them.
            _ = super::shutdown_signal() => {
                pane.kill();
                return Ok(exit_code.unwrap_or(130));
            }
            event = rx.recv() => match event {
                Some(HostEvent::Output(bytes)) => {
                    // OSC-52 is peeled out and re-emitted verbatim so the
                    // outer terminal — not our grid — performs the copy.
                    for sequence in pane.feed(&bytes) {
                        let mut out = std::io::stdout();
                        let _ = out.write_all(&sequence);
                        let _ = out.flush();
                    }
                }
                Some(HostEvent::Exited(code)) => {
                    exit_code = Some(code.unwrap_or(0));
                    // Drain whatever the child wrote just before exiting;
                    // otherwise its last frame is lost.
                    while let Ok(HostEvent::Output(bytes)) = rx.try_recv() {
                        pane.feed(&bytes);
                    }
                    draw(&mut terminal, &mut pane, &snap, ctx)?;
                    return Ok(exit_code.unwrap_or(0));
                }
                None => return Ok(exit_code.unwrap_or(0)),
            },
            input = futures::StreamExt::next(&mut events) => match input {
                Some(Ok(event)) => forward(&mut pane, event, cols, rows),
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(exit_code.unwrap_or(0)),
            },
        }
    }
}

/// Send one outer-terminal event to the child. Every key goes through
/// unmodified; the host claims none of them.
fn forward(pane: &mut PtyPane, event: Event, cols: u16, rows: u16) {
    match event {
        Event::Key(key) => {
            if let Some(bytes) = pane.backend.encode_key(&key) {
                // Typing implies you want to see what you are typing at.
                pane.backend.scroll_to_bottom();
                pane.write_input(&bytes);
            }
        }
        Event::Paste(text) => {
            let bytes = pane.backend.encode_paste(&text);
            pane.write_input(&bytes);
        }
        Event::Resize(new_cols, new_rows) => {
            pane.resize(new_cols, pane_rows(new_rows));
        }
        Event::Mouse(mouse) => {
            let wheel = matches!(
                mouse.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            );
            // The inner app gets pointer events only when it asked for them.
            // When it did not, the wheel pages our scrollback instead — the
            // host owns history here, since the real terminal no longer does.
            if pane.backend.mouse_enabled() {
                if let Some(bytes) = pane.backend.encode_mouse(
                    mouse.kind,
                    mouse.column.saturating_add(1),
                    mouse.row.saturating_add(1),
                    mouse.modifiers,
                ) {
                    pane.write_input(&bytes);
                }
            } else if wheel {
                pane.backend
                    .scroll(matches!(mouse.kind, MouseEventKind::ScrollUp), false);
            }
            let _ = (cols, rows);
        }
        _ => {}
    }
}

fn draw(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    pane: &mut PtyPane,
    snap: &Snapshot,
    ctx: &HostContext<'_>,
) -> Result<()> {
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let lines = pane.backend.lines(no_color);
    let scrolled = pane.backend.is_scrolled();
    let bar = render::status_bar(
        snap,
        ctx.harness,
        ctx.model.as_deref(),
        ctx.launch_id.as_deref(),
    );
    terminal.draw(|frame| {
        let [body, status] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
        frame.render_widget(Paragraph::new(lines), body);
        let mut text = bar;
        if scrolled {
            text.push_str("  ·  scrolled (type to return)");
        }
        frame.render_widget(
            Paragraph::new(Line::raw(text).style(Style::new().add_modifier(Modifier::REVERSED))),
            status,
        );
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_status_row_never_takes_the_last_line_from_the_child() {
        // Off-by-one here means the harness draws under the bar or loses a
        // row on every resize.
        assert_eq!(pane_rows(24), 23);
        assert_eq!(pane_rows(3), 2);
        // Degenerate sizes must still leave the child something to draw on
        // rather than underflowing to a zero-row PTY.
        assert_eq!(pane_rows(1), 1);
        assert_eq!(pane_rows(0), 1);
    }
}
