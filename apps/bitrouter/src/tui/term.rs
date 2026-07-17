//! PTY terminal backend for native harness panes (TUI_SPEC §8a/§9/§11).
//!
//! [`TerminalBackend`] abstracts the VT emulation core: byte feeding, the
//! cell grid (as ratatui lines), input encoding, and the emulator's write-back
//! responses (DA/DSR answers). The default implementation rides
//! `alacritty_terminal` — TUI_SPEC §11 picked wezterm-term for its input
//! encoder, but wezterm-term is not published to crates.io (a git dependency
//! would make this binary unpublishable). The encoders themselves, however,
//! ARE published, and [`encode_key`] delegates to them: legacy/DECCKM via
//! `termwiz::input::KeyCode::encode`, and — when the inner app pushed kitty
//! keyboard flags (tracked by the emulator, `kitty_keyboard: true`) —
//! `wezterm_input_types::KeyEvent::encode_kitty`, the same encoder wezterm's
//! GUI feeds its own panes. The trait remains §11's escape hatch: a full
//! wezterm-term/libghostty grid can slot in later without touching the TUI.
//!
//! Fidelity mechanics (§9) implemented at this layer:
//! - **Input encoding** follows what the inner app negotiated (kitty flags,
//!   application cursor keys, newline mode); `Ctrl-C` encodes to an
//!   interrupt (`0x03` legacy, `CSI 99;5u` kitty) — it interrupts the inner
//!   agent, it does not quit the manager.
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
use termwiz::escape::csi::KittyKeyboardFlags;
use termwiz::input::{KeyCodeEncodeModes, KeyboardEncoding};

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
    /// Encode pasted text for the inner app: wrapped in `ESC[200~ … ESC[201~`
    /// when it enabled bracketed paste (DEC 2004), raw bytes otherwise.
    fn encode_paste(&self, text: &str) -> Vec<u8>;
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
        let config = Config {
            // Let the inner app negotiate the kitty keyboard protocol — the
            // tracked flags drive the wezterm kitty encoder in `encode_key`.
            kitty_keyboard: true,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &size, proxy.clone()),
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
        encode_key(key, self.term.mode())
    }

    fn encode_paste(&self, text: &str) -> Vec<u8> {
        // Normalize CRLF; inner readline-style apps expect \r for Enter-ish
        // behavior only inside bracketed mode, raw \n otherwise is fine.
        let text = text.replace("\r\n", "\n");
        if self.term.mode().contains(TermMode::BRACKETED_PASTE) {
            let mut bytes = b"\x1b[200~".to_vec();
            bytes.extend_from_slice(text.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
            bytes
        } else {
            text.into_bytes()
        }
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

/// Encode a crossterm key event to the inner PTY's expected bytes via
/// **wezterm's encoders** — the piece TUI_SPEC §11 wanted from wezterm-term,
/// available from the published crates: legacy/DECCKM encoding is
/// `termwiz::input::KeyCode::encode`, and when the inner app pushed kitty
/// keyboard flags (tracked by the emulator) the key goes through
/// `wezterm_input_types::KeyEvent::encode_kitty` — exactly what wezterm's
/// GUI feeds its own panes. `Ctrl-C` still encodes to an interrupt (`0x03`
/// legacy / `CSI 99;5u` under kitty) — never a manager quit (§9/§12).
pub fn encode_key(key: &KeyEvent, mode: &TermMode) -> Option<Vec<u8>> {
    let mut mods = map_mods(key.modifiers);
    if key.code == KeyCode::BackTab {
        // crossterm's BackTab is Tab+SHIFT.
        mods |= termwiz::input::Modifiers::SHIFT;
    }

    // Kitty path: the inner app negotiated the protocol — encode with
    // wezterm's kitty encoder, honoring the exact flag set it pushed.
    if mode.intersects(TermMode::KITTY_KEYBOARD_PROTOCOL) {
        let event = wezterm_input_types::KeyEvent {
            key: map_key_kitty(key.code)?,
            modifiers: mods,
            leds: wezterm_input_types::KeyboardLedStatus::empty(),
            repeat_count: 1,
            key_is_down: true,
            raw: None,
            // wezterm-input-types only has this field on Windows.
            #[cfg(windows)]
            win32_uni_char: None,
        };
        let encoded = event.encode_kitty(kitty_flags(mode));
        if !encoded.is_empty() {
            return Some(encoded.into_bytes());
        }
        // Inexpressible without a raw event (rare) — fall through to legacy.
    }

    let modes = KeyCodeEncodeModes {
        encoding: KeyboardEncoding::Xterm,
        application_cursor_keys: mode.contains(TermMode::APP_CURSOR),
        newline_mode: mode.contains(TermMode::LINE_FEED_NEW_LINE),
        // XTMODKEYS (modifyOtherKeys) is not tracked by the emulator; the
        // kitty path supersedes it for the harnesses we host.
        modify_other_keys: None,
    };
    map_key(key.code)?
        .encode(mods, modes, true)
        .ok()
        .filter(|s| !s.is_empty())
        .map(String::into_bytes)
}

/// crossterm key → wezterm-input-types key for the kitty encoder, which
/// (per its own docs) wants Enter/Tab/Escape/Backspace as their `Char`
/// control forms.
fn map_key_kitty(code: KeyCode) -> Option<wezterm_input_types::KeyCode> {
    use wezterm_input_types::KeyCode as Wz;
    Some(match code {
        KeyCode::Char(c) => Wz::Char(c),
        KeyCode::Enter => Wz::Char('\r'),
        KeyCode::Tab | KeyCode::BackTab => Wz::Char('\t'),
        KeyCode::Esc => Wz::Char('\u{1b}'),
        KeyCode::Backspace => Wz::Char('\u{8}'),
        KeyCode::Delete => Wz::Char('\u{7f}'),
        KeyCode::Up => Wz::UpArrow,
        KeyCode::Down => Wz::DownArrow,
        KeyCode::Left => Wz::LeftArrow,
        KeyCode::Right => Wz::RightArrow,
        KeyCode::Home => Wz::Home,
        KeyCode::End => Wz::End,
        KeyCode::PageUp => Wz::PageUp,
        KeyCode::PageDown => Wz::PageDown,
        KeyCode::Insert => Wz::Insert,
        KeyCode::F(n) => Wz::Function(n),
        _ => return None,
    })
}

/// The emulator's kitty keyboard state → termwiz's flag set (1:1 bits).
fn kitty_flags(mode: &TermMode) -> KittyKeyboardFlags {
    let mut flags = KittyKeyboardFlags::NONE;
    flags.set(
        KittyKeyboardFlags::DISAMBIGUATE_ESCAPE_CODES,
        mode.contains(TermMode::DISAMBIGUATE_ESC_CODES),
    );
    flags.set(
        KittyKeyboardFlags::REPORT_EVENT_TYPES,
        mode.contains(TermMode::REPORT_EVENT_TYPES),
    );
    flags.set(
        KittyKeyboardFlags::REPORT_ALTERNATE_KEYS,
        mode.contains(TermMode::REPORT_ALTERNATE_KEYS),
    );
    flags.set(
        KittyKeyboardFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES,
        mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC),
    );
    flags.set(
        KittyKeyboardFlags::REPORT_ASSOCIATED_TEXT,
        mode.contains(TermMode::REPORT_ASSOCIATED_TEXT),
    );
    flags
}

/// crossterm key → termwiz key. `None` = a key we deliberately don't forward
/// (media keys, bare modifiers).
fn map_key(code: KeyCode) -> Option<termwiz::input::KeyCode> {
    use termwiz::input::KeyCode as Tw;
    Some(match code {
        KeyCode::Char(c) => Tw::Char(c),
        KeyCode::Enter => Tw::Enter,
        KeyCode::Tab | KeyCode::BackTab => Tw::Tab,
        KeyCode::Backspace => Tw::Backspace,
        KeyCode::Esc => Tw::Escape,
        KeyCode::Up => Tw::UpArrow,
        KeyCode::Down => Tw::DownArrow,
        KeyCode::Left => Tw::LeftArrow,
        KeyCode::Right => Tw::RightArrow,
        KeyCode::Home => Tw::Home,
        KeyCode::End => Tw::End,
        KeyCode::PageUp => Tw::PageUp,
        KeyCode::PageDown => Tw::PageDown,
        KeyCode::Insert => Tw::Insert,
        KeyCode::Delete => Tw::Delete,
        KeyCode::F(n) => Tw::Function(n),
        _ => return None,
    })
}

/// crossterm modifiers → termwiz modifiers.
fn map_mods(m: KeyModifiers) -> termwiz::input::Modifiers {
    use termwiz::input::Modifiers as Tw;
    let mut out = Tw::NONE;
    if m.contains(KeyModifiers::SHIFT) {
        out |= Tw::SHIFT;
    }
    if m.contains(KeyModifiers::ALT) {
        out |= Tw::ALT;
    }
    if m.contains(KeyModifiers::CONTROL) {
        out |= Tw::CTRL;
    }
    if m.contains(KeyModifiers::SUPER) {
        out |= Tw::SUPER;
    }
    out
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
    fn paste_brackets_only_when_the_inner_app_asked() {
        let mut backend = AlacrittyBackend::new(20, 4);
        assert_eq!(
            backend.encode_paste("a\r\nb"),
            b"a\nb".to_vec(),
            "raw (CRLF-normalized) without DEC 2004"
        );
        backend.feed(b"\x1b[?2004h");
        assert_eq!(
            backend.encode_paste("hi"),
            b"\x1b[200~hi\x1b[201~".to_vec(),
            "wrapped once the inner app enables bracketed paste"
        );
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
        assert_eq!(encode_key(&key, &TermMode::default()), Some(vec![0x03]));
    }

    #[test]
    fn arrows_honor_application_cursor_mode() {
        let up = KeyEvent::from(KeyCode::Up);
        assert_eq!(
            encode_key(&up, &TermMode::default()),
            Some(b"\x1b[A".to_vec())
        );
        // Drive DECCKM through the emulator, not a hand-set flag: the inner
        // app enables application cursor keys and the encoder follows.
        let mut b = AlacrittyBackend::new(20, 4);
        b.feed(b"\x1b[?1h");
        assert_eq!(b.encode_key(&up), Some(b"\x1bOA".to_vec()));
        // Modified arrows always use the CSI 1;<mod> form.
        let ctrl_up = KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL);
        assert_eq!(b.encode_key(&ctrl_up), Some(b"\x1b[1;5A".to_vec()));
    }

    #[test]
    fn plain_and_alt_keys_encode() {
        let m = TermMode::default();
        let a = KeyEvent::from(KeyCode::Char('a'));
        assert_eq!(encode_key(&a, &m), Some(b"a".to_vec()));
        let alt_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT);
        assert_eq!(encode_key(&alt_a, &m), Some(vec![0x1b, b'a']));
        let enter = KeyEvent::from(KeyCode::Enter);
        assert_eq!(encode_key(&enter, &m), Some(vec![b'\r']));
        let del = KeyEvent::from(KeyCode::Delete);
        assert_eq!(encode_key(&del, &m), Some(b"\x1b[3~".to_vec()));
        let f5 = KeyEvent::from(KeyCode::F(5));
        assert_eq!(encode_key(&f5, &m), Some(b"\x1b[15~".to_vec()));
        let utf8 = KeyEvent::from(KeyCode::Char('é'));
        assert_eq!(encode_key(&utf8, &m), Some("é".as_bytes().to_vec()));
        let shift_tab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE);
        assert_eq!(encode_key(&shift_tab, &m), Some(b"\x1b[Z".to_vec()));
    }

    #[test]
    fn kitty_negotiation_switches_the_encoder() {
        // The herdr-#106 class of bug this swap retires: the inner app pushes
        // kitty keyboard flags; the encoder must follow what was negotiated.
        let mut b = AlacrittyBackend::new(20, 4);
        let shift_enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

        // Legacy: Shift-Enter is indistinguishable from Enter; Ctrl-C is 0x03.
        assert_eq!(b.encode_key(&shift_enter), Some(vec![b'\r']));
        assert_eq!(b.encode_key(&ctrl_c), Some(vec![0x03]));

        // The inner app pushes DISAMBIGUATE_ESCAPE_CODES (kitty flag 1).
        b.feed(b"\x1b[>1u");
        assert_eq!(
            b.encode_key(&shift_enter),
            Some(b"\x1b[13;2u".to_vec()),
            "Shift-Enter becomes expressible under kitty"
        );
        assert_eq!(
            b.encode_key(&ctrl_c),
            Some(b"\x1b[99;5u".to_vec()),
            "Ctrl-C is CSI-u encoded — still an interrupt to the app"
        );
        // Plain text stays plain under DISAMBIGUATE alone.
        let a = KeyEvent::from(KeyCode::Char('a'));
        assert_eq!(b.encode_key(&a), Some(b"a".to_vec()));

        // The app pops its flags on exit: back to legacy encoding.
        b.feed(b"\x1b[<u");
        assert_eq!(b.encode_key(&shift_enter), Some(vec![b'\r']));
        assert_eq!(b.encode_key(&ctrl_c), Some(vec![0x03]));
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
