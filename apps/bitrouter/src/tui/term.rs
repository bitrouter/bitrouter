//! PTY terminal backend for native harness panes (TUI_SPEC §8a/§9/§11).
//!
//! [`TerminalBackend`] abstracts the VT emulation core: byte feeding, the
//! cell grid (as ratatui lines), input encoding, and the emulator's write-back
//! responses (DA/DSR answers). The default implementation rides
//! `alacritty_terminal` — TUI_SPEC §11 picked wezterm-term for its input
//! encoder, but wezterm-term is not published to crates.io (a git dependency
//! would make this binary unpublishable), so the trait is exactly the escape
//! hatch §11 keeps: the input encoder lives here (mode-aware, the §11 spike's
//! probe surface) and a wezterm-term/libghostty backend can slot in later
//! without touching the TUI.
//!
//! Fidelity mechanics (§9) implemented at this layer:
//! - **Input encoding** is mode-aware (application cursor keys, bracketed
//!   paste); `Ctrl-C` encodes to `0x03` — it interrupts the inner agent, it
//!   does not quit the manager.
//! - **OSC passthrough**: [`Osc52Scanner`] peels OSC-52 clipboard writes out
//!   of the PTY stream so the host loop can re-emit them verbatim to the
//!   outer terminal (the tmux `allow-passthrough` pattern), capped so a
//!   malicious child can't balloon memory.
//! - **Resize recovery**: [`PtyPane::resize`] resizes the emulator and the
//!   PTY (delivering `SIGWINCH`) whenever the pane rect changes.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Line as TermLine;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color as VtColor, NamedColor, Processor, Rgb};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TuiLine, Span};

/// The VT emulation core under a PTY pane. Object-safe so the pane host can
/// swap cores (§11's validation-gated backend decision).
pub trait TerminalBackend: Send {
    /// Feed PTY output bytes into the emulator.
    fn feed(&mut self, bytes: &[u8]);
    /// Resize the emulator grid.
    fn resize(&mut self, cols: u16, rows: u16);
    /// Snapshot the visible grid as styled lines (one per row).
    fn lines(&self, no_color: bool) -> Vec<TuiLine<'static>>;
    /// Encode one key press into the byte sequence the inner app expects,
    /// honoring the emulator's current keyboard modes.
    fn encode_key(&self, key: &KeyEvent) -> Option<Vec<u8>>;
    /// Drain bytes the emulator wants written back to the PTY (device
    /// attribute / status responses) — capability scoping happens here: the
    /// emulator answers with what it actually renders.
    fn drain_responses(&mut self) -> Vec<u8>;
}

/// `alacritty_terminal`-backed [`TerminalBackend`].
pub struct AlacrittyBackend {
    term: Term<Proxy>,
    parser: Processor,
    proxy: Proxy,
}

/// Event listener collecting the emulator's PTY write-backs.
#[derive(Clone)]
struct Proxy {
    responses: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

impl EventListener for Proxy {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event
            && let Ok(mut buf) = self.responses.lock()
        {
            buf.extend_from_slice(text.as_bytes());
        }
    }
}

/// Grid dimensions for the emulator (alacritty's `Dimensions` is in
/// lines/columns with no window pixels — fine for a cell renderer).
struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

impl AlacrittyBackend {
    pub fn new(cols: u16, rows: u16) -> Self {
        let proxy = Proxy {
            responses: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        };
        let size = Size {
            cols: cols.max(2) as usize,
            rows: rows.max(1) as usize,
        };
        Self {
            term: Term::new(Config::default(), &size, proxy.clone()),
            parser: Processor::new(),
            proxy,
        }
    }
}

impl TerminalBackend for AlacrittyBackend {
    fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.term.resize(Size {
            cols: cols.max(2) as usize,
            rows: rows.max(1) as usize,
        });
    }

    fn lines(&self, no_color: bool) -> Vec<TuiLine<'static>> {
        let cols = self.term.columns();
        let rows = self.term.screen_lines();
        let cursor = self.term.grid().cursor.point;
        let cursor_visible = self.term.mode().contains(TermMode::SHOW_CURSOR);
        // Cell matrix defaulted to spaces; the display iter fills what exists.
        let mut cells: Vec<Vec<Span<'static>>> = (0..rows)
            .map(|_| vec![Span::raw(" ".to_string()); cols])
            .collect();
        for indexed in self.term.grid().display_iter() {
            let point = indexed.point;
            let row = point.line.0;
            if row < 0 {
                continue; // scrollback above the viewport
            }
            let (row, col) = (row as usize, point.column.0);
            if row >= rows || col >= cols {
                continue;
            }
            let cell = &indexed.cell;
            let mut style = if no_color {
                Style::default()
            } else {
                Style::default().fg(vt_color(cell.fg)).bg(vt_color(cell.bg))
            };
            use alacritty_terminal::term::cell::Flags;
            if cell.flags.contains(Flags::BOLD) {
                style = style.add_modifier(Modifier::BOLD);
            }
            if cell.flags.contains(Flags::ITALIC) {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if cell.flags.contains(Flags::UNDERLINE) {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            if cell.flags.contains(Flags::INVERSE) {
                style = style.add_modifier(Modifier::REVERSED);
            }
            if cell.flags.contains(Flags::DIM) {
                style = style.add_modifier(Modifier::DIM);
            }
            // The inner app's cursor renders as a reversed cell — the host
            // terminal's real cursor stays with the manager.
            if cursor_visible
                && point.line == TermLine(cursor.line.0)
                && point.column == cursor.column
            {
                style = style.add_modifier(Modifier::REVERSED);
            }
            cells[row][col] = Span::styled(cell.c.to_string(), style);
        }
        cells.into_iter().map(TuiLine::from).collect()
    }

    fn encode_key(&self, key: &KeyEvent) -> Option<Vec<u8>> {
        encode_key(key, self.term.mode().contains(TermMode::APP_CURSOR))
    }

    fn drain_responses(&mut self) -> Vec<u8> {
        match self.proxy.responses.lock() {
            Ok(mut buf) => std::mem::take(&mut *buf),
            Err(_) => Vec::new(),
        }
    }
}

/// Map a VT color to ratatui's, preserving truecolor and the 16/256 palettes.
fn vt_color(c: VtColor) -> Color {
    match c {
        VtColor::Spec(Rgb { r, g, b }) => Color::Rgb(r, g, b),
        VtColor::Indexed(i) => Color::Indexed(i),
        VtColor::Named(n) => match n {
            NamedColor::Black => Color::Black,
            NamedColor::Red => Color::Red,
            NamedColor::Green => Color::Green,
            NamedColor::Yellow => Color::Yellow,
            NamedColor::Blue => Color::Blue,
            NamedColor::Magenta => Color::Magenta,
            NamedColor::Cyan => Color::Cyan,
            NamedColor::White => Color::Gray,
            NamedColor::BrightBlack => Color::DarkGray,
            NamedColor::BrightRed => Color::LightRed,
            NamedColor::BrightGreen => Color::LightGreen,
            NamedColor::BrightYellow => Color::LightYellow,
            NamedColor::BrightBlue => Color::LightBlue,
            NamedColor::BrightMagenta => Color::LightMagenta,
            NamedColor::BrightCyan => Color::LightCyan,
            NamedColor::BrightWhite => Color::White,
            NamedColor::Foreground | NamedColor::BrightForeground => Color::Reset,
            NamedColor::Background => Color::Reset,
            _ => Color::Reset,
        },
    }
}

/// Encode a crossterm key event to the inner PTY's expected bytes. Legacy
/// xterm encoding, mode-aware for DECCKM (application cursor keys). `Ctrl-C`
/// → `0x03` (interrupt the agent, never quit the manager — §9/§12).
pub fn encode_key(key: &KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // Arrow/nav keys: CSI (normal) or SS3 (application mode); modifiers use
    // the xterm `CSI 1;<mod>` form.
    let csi_mod = |base: &str, final_ch: char| -> Vec<u8> {
        let m = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
        if m == 1 {
            let intro: &str = if app_cursor && base == "1" {
                "\x1bO"
            } else {
                "\x1b["
            };
            if base == "1" {
                return format!("{intro}{final_ch}").into_bytes();
            }
            return format!("\x1b[{base}{final_ch}").into_bytes();
        }
        format!("\x1b[{base};{m}{final_ch}").into_bytes()
    };

    let bytes = match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Ctrl-A..Z → 0x01..0x1A (Ctrl-C interrupts the inner agent).
                let lower = c.to_ascii_lowercase();
                if lower.is_ascii_lowercase() {
                    let byte = (lower as u8 - b'a') + 1;
                    if alt { vec![0x1b, byte] } else { vec![byte] }
                } else {
                    return None;
                }
            } else {
                let mut out = Vec::new();
                if alt {
                    out.push(0x1b);
                }
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                out
            }
        }
        KeyCode::Enter => {
            if alt {
                vec![0x1b, b'\r']
            } else {
                vec![b'\r']
            }
        }
        KeyCode::Tab => {
            if shift {
                b"\x1b[Z".to_vec()
            } else {
                vec![b'\t']
            }
        }
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => {
            if alt {
                vec![0x1b, 0x7f]
            } else {
                vec![0x7f]
            }
        }
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => csi_mod("1", 'A'),
        KeyCode::Down => csi_mod("1", 'B'),
        KeyCode::Right => csi_mod("1", 'C'),
        KeyCode::Left => csi_mod("1", 'D'),
        KeyCode::Home => csi_mod("1", 'H'),
        KeyCode::End => csi_mod("1", 'F'),
        KeyCode::PageUp => csi_mod("5", '~'),
        KeyCode::PageDown => csi_mod("6", '~'),
        KeyCode::Insert => csi_mod("2", '~'),
        KeyCode::Delete => csi_mod("3", '~'),
        KeyCode::F(n @ 1..=4) => {
            // F1–F4 are SS3 P/Q/R/S unmodified, CSI 1;<mod> P… modified.
            let final_ch = b"PQRS"[(n - 1) as usize] as char;
            let m = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
            if m == 1 {
                format!("\x1bO{final_ch}").into_bytes()
            } else {
                format!("\x1b[1;{m}{final_ch}").into_bytes()
            }
        }
        KeyCode::F(n @ 5..=12) => {
            let base = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                _ => 24,
            };
            csi_mod(&base.to_string(), '~')
        }
        _ => return None,
    };
    Some(bytes)
}

/// OSC-52 sequences beyond this are dropped, not forwarded (herdr's cap —
/// keeps a malicious child from ballooning the splitter).
const OSC52_CAP: usize = 256 * 1024;

/// Byte-level splitter that recognizes OSC-52 (clipboard) sequences in the
/// PTY output stream so the host can re-emit them verbatim to the outer
/// terminal — the tmux `allow-passthrough` pattern. Stateful across chunk
/// boundaries.
#[derive(Default)]
pub struct Osc52Scanner {
    /// In-progress sequence bytes (from `ESC ]` on), when inside a candidate.
    pending: Vec<u8>,
    state: ScanState,
}

#[derive(Default, PartialEq)]
enum ScanState {
    #[default]
    Ground,
    /// Saw ESC; deciding.
    Esc,
    /// Inside `ESC ] …` collecting until BEL or ST.
    Osc,
    /// Inside OSC and saw ESC (potential ST terminator).
    OscEsc,
}

impl Osc52Scanner {
    /// Scan one output chunk; returns any complete OSC-52 sequences found
    /// (each verbatim, terminator included) for re-emission.
    pub fn scan(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut found = Vec::new();
        for &b in bytes {
            match self.state {
                ScanState::Ground => {
                    if b == 0x1b {
                        self.state = ScanState::Esc;
                    }
                }
                ScanState::Esc => {
                    if b == b']' {
                        self.state = ScanState::Osc;
                        self.pending.clear();
                        self.pending.extend_from_slice(&[0x1b, b']']);
                    } else {
                        self.state = ScanState::Ground;
                    }
                }
                ScanState::Osc => {
                    if self.pending.len() > OSC52_CAP {
                        // Too big: abandon (never forward unbounded data).
                        self.pending.clear();
                        self.state = ScanState::Ground;
                    } else if b == 0x07 {
                        self.pending.push(b);
                        self.finish(&mut found);
                    } else if b == 0x1b {
                        self.state = ScanState::OscEsc;
                    } else {
                        self.pending.push(b);
                    }
                }
                ScanState::OscEsc => {
                    if b == b'\\' {
                        self.pending.extend_from_slice(&[0x1b, b'\\']);
                        self.finish(&mut found);
                    } else {
                        // Not ST — the OSC was aborted.
                        self.pending.clear();
                        self.state = ScanState::Ground;
                        if b == b']' {
                            // …and a new OSC starts.
                            self.state = ScanState::Osc;
                            self.pending.extend_from_slice(&[0x1b, b']']);
                        }
                    }
                }
            }
        }
        found
    }

    fn finish(&mut self, found: &mut Vec<Vec<u8>>) {
        let seq = std::mem::take(&mut self.pending);
        self.state = ScanState::Ground;
        // Only clipboard (OSC 52) is forwarded; titles/hyperlinks render
        // inside the pane and must not leak to the outer terminal.
        if seq.get(2..5) == Some(b"52;") {
            found.push(seq);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(backend: &AlacrittyBackend) -> String {
        backend
            .lines(true)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn feed_renders_text_into_the_grid() {
        let mut b = AlacrittyBackend::new(20, 4);
        b.feed(b"hello\r\nworld");
        let t = text(&b);
        assert!(t.contains("hello"), "{t:?}");
        assert!(t.contains("world"), "{t:?}");
    }

    #[test]
    fn sgr_colors_map_to_ratatui() {
        let mut b = AlacrittyBackend::new(20, 2);
        b.feed(b"\x1b[31mred\x1b[0m plain");
        let lines = b.lines(false);
        let red = &lines[0].spans[0];
        assert_eq!(red.content.as_ref(), "r");
        assert_eq!(red.style.fg, Some(Color::Red));
    }

    #[test]
    fn resize_reflows_the_grid() {
        let mut b = AlacrittyBackend::new(10, 3);
        b.feed(b"0123456789");
        b.resize(20, 5);
        assert_eq!(b.lines(true).len(), 5, "grid tracks the new size");
        b.feed(b"\x1b[1;1Hafter-resize");
        assert!(text(&b).contains("after-resize"));
    }

    #[test]
    fn device_attribute_queries_are_answered() {
        let mut b = AlacrittyBackend::new(10, 3);
        b.feed(b"\x1b[c"); // primary DA query
        let resp = b.drain_responses();
        assert!(
            resp.starts_with(b"\x1b[?"),
            "emulator answers DA so the inner app never probes the void: {resp:?}"
        );
        assert!(b.drain_responses().is_empty(), "drained");
    }

    #[test]
    fn ctrl_c_encodes_to_interrupt_not_quit() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(encode_key(&key, false), Some(vec![0x03]));
    }

    #[test]
    fn arrows_honor_application_cursor_mode() {
        let up = KeyEvent::from(KeyCode::Up);
        assert_eq!(encode_key(&up, false), Some(b"\x1b[A".to_vec()));
        assert_eq!(encode_key(&up, true), Some(b"\x1bOA".to_vec()));
        // Modified arrows always use the CSI 1;<mod> form.
        let ctrl_up = KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL);
        assert_eq!(encode_key(&ctrl_up, true), Some(b"\x1b[1;5A".to_vec()));
    }

    #[test]
    fn plain_and_alt_keys_encode() {
        let a = KeyEvent::from(KeyCode::Char('a'));
        assert_eq!(encode_key(&a, false), Some(b"a".to_vec()));
        let alt_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT);
        assert_eq!(encode_key(&alt_a, false), Some(vec![0x1b, b'a']));
        let enter = KeyEvent::from(KeyCode::Enter);
        assert_eq!(encode_key(&enter, false), Some(vec![b'\r']));
        let del = KeyEvent::from(KeyCode::Delete);
        assert_eq!(encode_key(&del, false), Some(b"\x1b[3~".to_vec()));
        let f5 = KeyEvent::from(KeyCode::F(5));
        assert_eq!(encode_key(&f5, false), Some(b"\x1b[15~".to_vec()));
        // The emulator's own cursor mode never leaks into typed characters.
        let utf8 = KeyEvent::from(KeyCode::Char('é'));
        assert_eq!(encode_key(&utf8, false), Some("é".as_bytes().to_vec()));
    }

    #[test]
    fn osc52_scanner_extracts_clipboard_sequences_across_chunks() {
        let mut sc = Osc52Scanner::default();
        // Split mid-sequence to prove statefulness.
        let seq = b"\x1b]52;c;aGVsbG8=\x07";
        let found1 = sc.scan(&seq[..7]);
        assert!(found1.is_empty(), "incomplete — nothing forwarded yet");
        let found2 = sc.scan(&seq[7..]);
        assert_eq!(found2, vec![seq.to_vec()], "forwarded verbatim");
    }

    #[test]
    fn osc52_scanner_ignores_titles_and_respects_st_terminator() {
        let mut sc = Osc52Scanner::default();
        assert!(
            sc.scan(b"\x1b]0;window title\x07").is_empty(),
            "titles stay inside the pane"
        );
        let st = b"\x1b]52;c;YQ==\x1b\\";
        assert_eq!(sc.scan(st), vec![st.to_vec()], "ST-terminated form");
    }

    #[test]
    fn osc52_scanner_drops_oversized_sequences() {
        let mut sc = Osc52Scanner::default();
        let mut big = b"\x1b]52;c;".to_vec();
        big.extend(std::iter::repeat_n(b'A', OSC52_CAP + 10));
        big.push(0x07);
        assert!(sc.scan(&big).is_empty(), "capped, never ballooned");
        // Scanner recovers for the next sequence.
        let ok = b"\x1b]52;c;YQ==\x07";
        assert_eq!(sc.scan(ok), vec![ok.to_vec()]);
    }
}
