//! The [`Terminal`] entity: a PTY child plus an `alacritty_terminal::Term`
//! emulator, driven by alacritty's own event loop thread.

use std::sync::Arc;
use std::time::Duration;

use alacritty_terminal::event::{Event as AlacEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, Options as PtyOptions, Shell};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Rgb};
use anyhow::Context as _;
use futures::channel::mpsc::{unbounded, UnboundedSender};
use futures::StreamExt;
use gpui::Context;

/// Default scrollback. Kept modest; this is not a full-featured terminal.
const SCROLLBACK_LINES: usize = 10_000;

/// How long to batch grid mutations before asking gpui to repaint. Bulk output
/// (e.g. `cat` of a large file) emits a flood of `Wakeup` events; coalescing
/// them onto a short timer keeps the UI responsive instead of repainting per
/// chunk.
const COALESCE_INTERVAL: Duration = Duration::from_millis(4);

/// A simple RGB triple for the snapshot. Decoupled from any gpui color type so
/// the element layer owns the conversion to `Hsla`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// A single rendered grid cell: the character plus the minimal styling the
/// element layer needs to paint it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
}

/// An owned snapshot of the currently visible grid, safe to hand to the render
/// thread without holding the terminal lock.
#[derive(Clone, Debug)]
pub struct TerminalSnapshot {
    pub rows: Vec<Vec<Cell>>,
    pub cols: usize,
}

impl TerminalSnapshot {
    /// First row as a `String`, trimmed of trailing spaces. Handy for tests.
    pub fn first_row_text(&self) -> String {
        self.rows
            .first()
            .map(|row| row.iter().map(|cell| cell.ch).collect::<String>())
            .map(|line| line.trim_end().to_string())
            .unwrap_or_default()
    }
}

/// The default foreground/background used when alacritty reports the terminal's
/// own default colors (which we don't theme here).
const DEFAULT_FG: Color = Color::rgb(0xCC, 0xCC, 0xCC);
const DEFAULT_BG: Color = Color::rgb(0x1E, 0x1E, 0x1E);

/// `EventListener` newtype forwarding alacritty events onto an unbounded channel
/// drained by a gpui background task.
#[derive(Clone)]
struct Listener(UnboundedSender<AlacEvent>);

impl EventListener for Listener {
    fn send_event(&self, event: AlacEvent) {
        // If the receiver is gone the terminal is shutting down; dropping the
        // event is correct.
        let _ = self.0.unbounded_send(event);
    }
}

/// Dimensions wrapper implementing alacritty's [`Dimensions`] for `Term::new`
/// and `Term::resize`.
#[derive(Clone, Copy)]
struct TermDimensions {
    lines: usize,
    columns: usize,
}

impl Dimensions for TermDimensions {
    fn total_lines(&self) -> usize {
        self.lines
    }

    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// A PTY-backed terminal. Construct with [`Terminal::spawn`] inside a
/// `cx.new(|cx| Terminal::spawn(...))` closure so the receiver task can hold a
/// weak handle to the entity.
pub struct Terminal {
    term: Arc<FairMutex<Term<Listener>>>,
    /// Sender into alacritty's event loop for input and resize messages.
    ///
    /// `None` for a [`Terminal::placeholder`] whose PTY never started; in that
    /// state [`Terminal::input`] and [`Terminal::resize`] are no-ops.
    loop_tx: Option<EventLoopSender>,
    rows: u16,
    cols: u16,
}

impl Terminal {
    /// Spawn `program` (with `args`) in a fresh PTY sized `rows`x`cols`.
    ///
    /// Must be called from within an entity-build closure so that grid mutations
    /// can notify this entity. Returns an error if the PTY or the event loop
    /// cannot be created.
    pub fn spawn(
        program: &str,
        args: &[String],
        cwd: Option<&std::path::Path>,
        rows: u16,
        cols: u16,
        cx: &mut Context<Self>,
    ) -> anyhow::Result<Self> {
        let rows = rows.max(1);
        let cols = cols.max(1);

        let (event_tx, mut event_rx) = unbounded::<AlacEvent>();
        let listener = Listener(event_tx);

        let dimensions = TermDimensions {
            lines: rows as usize,
            columns: cols as usize,
        };

        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            ..Default::default()
        };

        let term = Term::new(config, &dimensions, listener.clone());
        let term = Arc::new(FairMutex::new(term));

        let window_size = WindowSize {
            num_lines: rows,
            num_cols: cols,
            // Cell pixel size is only used for apps that query it; a sane default
            // is fine for a minimal terminal.
            cell_width: 8,
            cell_height: 16,
        };

        let pty_options = PtyOptions {
            shell: Some(Shell::new(program.to_string(), args.to_vec())),
            working_directory: cwd.map(|p| p.to_path_buf()),
            drain_on_exit: true,
            env: Default::default(),
            #[cfg(target_os = "windows")]
            escape_args: true,
        };

        tty::setup_env();

        let pty =
            tty::new(&pty_options, window_size, 0).context("failed to open pseudoterminal")?;

        let event_loop = EventLoop::new(term.clone(), listener, pty, false, false)
            .context("failed to create terminal event loop")?;
        let loop_tx = event_loop.channel();
        // The PTY reader thread owns the I/O; we keep only the sender side.
        let _join = event_loop.spawn();

        // Drain alacritty events on a gpui background task, coalescing repaints.
        cx.spawn(async move |this, cx| {
            while let Some(first) = event_rx.next().await {
                let mut should_notify = matches!(first, AlacEvent::Wakeup);
                let mut exited = matches!(first, AlacEvent::Exit | AlacEvent::ChildExit(_));

                // Coalesce: let any events buffered during the interval pile up,
                // then notify once.
                cx.background_executor().timer(COALESCE_INTERVAL).await;
                while let Ok(event) = event_rx.try_recv() {
                    match event {
                        AlacEvent::Wakeup => should_notify = true,
                        AlacEvent::Exit | AlacEvent::ChildExit(_) => {
                            exited = true;
                            should_notify = true;
                        }
                        _ => {}
                    }
                }

                if should_notify {
                    let updated = this.update(cx, |_, cx| cx.notify());
                    if updated.is_err() {
                        break;
                    }
                }

                if exited {
                    break;
                }
            }
        })
        .detach();

        Ok(Self {
            term,
            loop_tx: Some(loop_tx),
            rows,
            cols,
        })
    }

    /// An inert terminal with no live PTY, used by callers that build the entity
    /// with `cx.new` and need a value for the failure branch (see the crate
    /// binary). Renders an empty grid; input and resize are no-ops.
    pub fn placeholder() -> Self {
        let dimensions = TermDimensions {
            lines: 1,
            columns: 1,
        };
        let (event_tx, _event_rx) = unbounded::<AlacEvent>();
        let term = Term::new(Config::default(), &dimensions, Listener(event_tx));
        Self {
            term: Arc::new(FairMutex::new(term)),
            loop_tx: None,
            rows: 1,
            cols: 1,
        }
    }

    /// Write raw bytes to the PTY (already encoded for the terminal).
    pub fn input(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let Some(loop_tx) = self.loop_tx.as_ref() else {
            return;
        };
        let owned: Vec<u8> = bytes.to_vec();
        let _ = loop_tx.send(Msg::Input(owned.into()));
    }

    /// Resize both the emulator grid and the PTY.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;

        let dimensions = TermDimensions {
            lines: rows as usize,
            columns: cols as usize,
        };
        self.term.lock().resize(dimensions);

        let Some(loop_tx) = self.loop_tx.as_ref() else {
            return;
        };
        let window_size = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: 8,
            cell_height: 16,
        };
        let _ = loop_tx.send(Msg::Resize(window_size));
    }

    /// Visible grid rows/cols.
    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    /// Build an owned snapshot of the visible grid.
    pub fn snapshot(&self) -> TerminalSnapshot {
        let term = self.term.lock();
        let cols = term.columns();
        let screen_lines = term.screen_lines();

        let mut rows: Vec<Vec<Cell>> = (0..screen_lines)
            .map(|_| {
                vec![
                    Cell {
                        ch: ' ',
                        fg: DEFAULT_FG,
                        bg: DEFAULT_BG,
                        bold: false,
                    };
                    cols
                ]
            })
            .collect();

        let content = term.renderable_content();
        let display_offset = content.display_offset as i32;
        for indexed in content.display_iter {
            // `point.line` is relative to the top of history; convert to a
            // viewport row index.
            let row = indexed.point.line.0 + display_offset;
            if row < 0 || row as usize >= screen_lines {
                continue;
            }
            let col = indexed.point.column.0;
            if col >= cols {
                continue;
            }

            let cell = indexed.cell;
            let flags = cell.flags;
            // Skip the trailing half of wide chars; the leading cell carries it.
            if flags.contains(Flags::WIDE_CHAR_SPACER)
                || flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }

            let bold = flags.contains(Flags::BOLD) || flags.contains(Flags::BOLD_ITALIC);
            let inverse = flags.contains(Flags::INVERSE);
            let hidden = flags.contains(Flags::HIDDEN);

            let mut fg = ansi_to_color(cell.fg, true);
            let mut bg = ansi_to_color(cell.bg, false);
            if inverse {
                std::mem::swap(&mut fg, &mut bg);
            }
            let ch = if hidden { ' ' } else { cell.c };

            rows[row as usize][col] = Cell { ch, fg, bg, bold };
        }

        TerminalSnapshot { rows, cols }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(tx) = self.loop_tx.as_ref() {
            let _ = tx.send(Msg::Shutdown);
        }
    }
}

/// Resolve an alacritty [`AnsiColor`] to a concrete RGB triple using a small
/// built-in palette. We don't theme the terminal, so named/default colors map
/// to fixed values and indexed colors use the xterm 256-color cube.
fn ansi_to_color(color: AnsiColor, is_fg: bool) -> Color {
    match color {
        AnsiColor::Spec(Rgb { r, g, b }) => Color::rgb(r, g, b),
        AnsiColor::Named(named) => named_color(named, is_fg),
        AnsiColor::Indexed(idx) => indexed_color(idx),
    }
}

/// 16-color ANSI palette (a common dark scheme).
const ANSI_16: [Color; 16] = [
    Color::rgb(0x00, 0x00, 0x00), // black
    Color::rgb(0xCD, 0x31, 0x31), // red
    Color::rgb(0x0D, 0xBC, 0x79), // green
    Color::rgb(0xE5, 0xE5, 0x10), // yellow
    Color::rgb(0x24, 0x72, 0xC8), // blue
    Color::rgb(0xBC, 0x3F, 0xBC), // magenta
    Color::rgb(0x11, 0xA8, 0xCD), // cyan
    Color::rgb(0xE5, 0xE5, 0xE5), // white
    Color::rgb(0x66, 0x66, 0x66), // bright black
    Color::rgb(0xF1, 0x4C, 0x4C), // bright red
    Color::rgb(0x23, 0xD1, 0x8B), // bright green
    Color::rgb(0xF5, 0xF5, 0x43), // bright yellow
    Color::rgb(0x3B, 0x8E, 0xEA), // bright blue
    Color::rgb(0xD6, 0x70, 0xD6), // bright magenta
    Color::rgb(0x29, 0xB8, 0xDB), // bright cyan
    Color::rgb(0xFF, 0xFF, 0xFF), // bright white
];

fn named_color(named: NamedColor, is_fg: bool) -> Color {
    match named {
        NamedColor::Foreground => DEFAULT_FG,
        NamedColor::Background => DEFAULT_BG,
        NamedColor::Black => ANSI_16[0],
        NamedColor::Red => ANSI_16[1],
        NamedColor::Green => ANSI_16[2],
        NamedColor::Yellow => ANSI_16[3],
        NamedColor::Blue => ANSI_16[4],
        NamedColor::Magenta => ANSI_16[5],
        NamedColor::Cyan => ANSI_16[6],
        NamedColor::White => ANSI_16[7],
        NamedColor::BrightBlack => ANSI_16[8],
        NamedColor::BrightRed => ANSI_16[9],
        NamedColor::BrightGreen => ANSI_16[10],
        NamedColor::BrightYellow => ANSI_16[11],
        NamedColor::BrightBlue => ANSI_16[12],
        NamedColor::BrightMagenta => ANSI_16[13],
        NamedColor::BrightCyan => ANSI_16[14],
        NamedColor::BrightWhite => ANSI_16[15],
        NamedColor::BrightForeground => DEFAULT_FG,
        // Dim variants and cursor colors: fall back to a sensible default.
        _ => {
            if is_fg {
                DEFAULT_FG
            } else {
                DEFAULT_BG
            }
        }
    }
}

/// Map an xterm 256-color index to RGB.
fn indexed_color(idx: u8) -> Color {
    match idx {
        0..=15 => ANSI_16[idx as usize],
        16..=231 => {
            // 6x6x6 color cube.
            let i = idx - 16;
            let r = i / 36;
            let g = (i % 36) / 6;
            let b = i % 6;
            let level = |v: u8| -> u8 {
                if v == 0 {
                    0
                } else {
                    55 + v * 40
                }
            };
            Color::rgb(level(r), level(g), level(b))
        }
        232..=255 => {
            // Grayscale ramp.
            let v = 8 + (idx - 232) * 10;
            Color::rgb(v, v, v)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, Entity, TestAppContext};
    use std::time::{Duration, Instant};

    // Plain `#[test]` (not `#[gpui::test]`) is used deliberately: the std test
    // harness honors a returned `Result`, failing on `Err`, whereas the
    // `#[gpui::test]` macro discards the inner function's return value. This
    // lets the tests surface failures without any `unwrap`/`expect`/`panic!`.

    /// Build a `Terminal` entity inside a test app, capturing the spawn result.
    fn spawn_in_test(
        cx: &mut TestAppContext,
        program: &str,
        args: &[String],
        rows: u16,
        cols: u16,
    ) -> anyhow::Result<Entity<Terminal>> {
        let mut spawn_err: Option<anyhow::Error> = None;
        let entity = cx.update(|cx| {
            cx.new(
                |cx| match Terminal::spawn(program, args, None, rows, cols, cx) {
                    Ok(terminal) => terminal,
                    Err(err) => {
                        spawn_err = Some(err);
                        Terminal::placeholder()
                    }
                },
            )
        });
        match spawn_err {
            Some(err) => Err(err),
            None => Ok(entity),
        }
    }

    #[test]
    fn terminal_echo() -> anyhow::Result<()> {
        let mut cx = TestAppContext::single();
        let args = vec!["-n".to_string(), "hi".to_string()];
        let terminal = spawn_in_test(&mut cx, "/bin/echo", &args, 24, 80)?;

        // The PTY reader runs on its own OS thread, parsing bytes into the
        // shared `Term`. Poll the snapshot until "hi" shows up, with a wall
        // clock timeout so a broken PTY fails the test instead of hanging.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut first_row = String::new();
        while Instant::now() < deadline {
            cx.run_until_parked();
            first_row = terminal.read_with(&cx, |t, _| t.snapshot().first_row_text());
            if first_row.contains("hi") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        anyhow::ensure!(
            first_row.contains("hi"),
            "expected first row to contain 'hi', got {first_row:?}"
        );
        Ok(())
    }

    #[test]
    fn placeholder_is_inert() -> anyhow::Result<()> {
        let mut cx = TestAppContext::single();
        let terminal = cx.update(|cx| cx.new(|_| Terminal::placeholder()));
        // input/resize must not panic and the snapshot must be empty-ish.
        terminal.update(&mut cx, |t, _| {
            t.input(b"hello");
            t.resize(10, 40);
        });
        let text = terminal.read_with(&cx, |t, _| t.snapshot().first_row_text());
        anyhow::ensure!(
            text.is_empty(),
            "placeholder row should be empty, got {text:?}"
        );
        Ok(())
    }
}
