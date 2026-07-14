//! Outer-terminal escape encoding for notifications and the title badge.
//!
//! The tower's attention model only works if it can reach the human when the
//! terminal is *not* focused, so `Effect::Notify` is delivered as the host
//! terminal's native notification escape — picked per terminal, one sequence
//! each (emitting several would double-notify on terminals that support more
//! than one). Terminals decide themselves whether to suppress a notification
//! for a focused window. Everything here is pure encoding; the loop writes
//! the bytes to stdout.

/// Which notification escape the host terminal understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermKind {
    /// kitty: OSC 99 (its desktop-notifications protocol).
    Kitty,
    /// iTerm2: OSC 9 (the original growl-style escape).
    Iterm2,
    /// Terminal.app: no notification escape — the bell is all it has.
    AppleTerminal,
    /// Everything else (WezTerm, Ghostty, foot, urxvt, unknown): OSC 777
    /// `notify`, the most widely parsed title+body form.
    Other,
}

/// The notification path, detected once at startup: the terminal's escape
/// dialect plus whether sequences must be wrapped in a tmux DCS passthrough.
#[derive(Debug, Clone, Copy)]
pub struct NotifyPath {
    pub kind: TermKind,
    /// Running under tmux: wrap escapes in `DCS tmux; … ST` so they reach
    /// the outer terminal (needs tmux's `allow-passthrough`; best-effort).
    pub tmux: bool,
}

impl NotifyPath {
    /// Detect from the environment (`TERM_PROGRAM`, `TERM`, `KITTY_WINDOW_ID`,
    /// `TMUX`).
    pub fn detect() -> Self {
        Self::from_env(
            std::env::var("TERM_PROGRAM").ok().as_deref(),
            std::env::var("TERM").ok().as_deref(),
            std::env::var_os("KITTY_WINDOW_ID").is_some(),
            std::env::var_os("TMUX").is_some(),
        )
    }

    fn from_env(
        term_program: Option<&str>,
        term: Option<&str>,
        kitty_window: bool,
        tmux: bool,
    ) -> Self {
        let kind = if kitty_window || term.is_some_and(|t| t.contains("kitty")) {
            TermKind::Kitty
        } else {
            match term_program {
                Some("iTerm.app") => TermKind::Iterm2,
                Some("Apple_Terminal") => TermKind::AppleTerminal,
                _ => TermKind::Other,
            }
        };
        Self { kind, tmux }
    }

    /// Encode one notification, or `None` when the terminal has no
    /// notification escape (the bell effect already covers it).
    pub fn notification(&self, title: &str, body: &str) -> Option<Vec<u8>> {
        let title = sanitize(title);
        let body = sanitize(body);
        let seq = match self.kind {
            TermKind::AppleTerminal => return None,
            // kitty's protocol: with no payload-type metadata the payload is
            // the notification title, which is all a one-shot ping needs.
            TermKind::Kitty => format!("\x1b]99;;{title}: {body}\x1b\\"),
            TermKind::Iterm2 => format!("\x1b]9;{title}: {body}\x07"),
            // OSC 777 carries title and body as separate fields; the title
            // field cannot contain the `;` separator.
            TermKind::Other => {
                format!("\x1b]777;notify;{};{body}\x07", title.replace(';', ","))
            }
        };
        Some(self.passthrough(seq.into_bytes()))
    }

    /// Encode a terminal-title update (OSC 2) — the tab/window badge.
    pub fn title(&self, text: &str) -> Vec<u8> {
        self.passthrough(format!("\x1b]2;{}\x07", sanitize(text)).into_bytes())
    }

    /// XTWINOPS: push the current title so restore can pop it back.
    pub fn title_push(&self) -> Vec<u8> {
        self.passthrough(b"\x1b[22;0t".to_vec())
    }

    /// XTWINOPS: restore the pushed title on exit.
    pub fn title_pop(&self) -> Vec<u8> {
        self.passthrough(b"\x1b[23;0t".to_vec())
    }

    /// Wrap `seq` for tmux passthrough when needed (ESC doubled inside a
    /// `DCS tmux;` envelope), verbatim otherwise.
    fn passthrough(&self, seq: Vec<u8>) -> Vec<u8> {
        if !self.tmux {
            return seq;
        }
        let mut out = b"\x1bPtmux;".to_vec();
        for byte in seq {
            if byte == 0x1b {
                out.push(0x1b);
            }
            out.push(byte);
        }
        out.extend_from_slice(b"\x1b\\");
        out
    }
}

/// Strip control bytes (they would terminate or corrupt the escape) and cap
/// the length so a pathological title stays a notification, not a payload.
fn sanitize(text: &str) -> String {
    let mut out: String = text.chars().filter(|c| !c.is_control()).take(120).collect();
    if text.chars().count() > 120 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_picks_one_dialect_per_terminal() {
        let kitty = NotifyPath::from_env(None, Some("xterm-kitty"), false, false);
        assert_eq!(kitty.kind, TermKind::Kitty);
        // KITTY_WINDOW_ID wins even when TERM was rewritten (e.g. by a shell).
        let kitty_env = NotifyPath::from_env(Some("WezTerm"), Some("xterm-256color"), true, false);
        assert_eq!(kitty_env.kind, TermKind::Kitty);
        let iterm = NotifyPath::from_env(Some("iTerm.app"), Some("xterm-256color"), false, false);
        assert_eq!(iterm.kind, TermKind::Iterm2);
        let apple =
            NotifyPath::from_env(Some("Apple_Terminal"), Some("xterm-256color"), false, false);
        assert_eq!(apple.kind, TermKind::AppleTerminal);
        let wez = NotifyPath::from_env(Some("WezTerm"), Some("xterm-256color"), false, false);
        assert_eq!(wez.kind, TermKind::Other);
    }

    #[test]
    fn each_dialect_encodes_exactly_its_escape() {
        let enc = |kind| NotifyPath { kind, tmux: false }.notification("api-1 finished", "done");
        assert_eq!(
            enc(TermKind::Iterm2),
            Some(b"\x1b]9;api-1 finished: done\x07".to_vec())
        );
        assert_eq!(
            enc(TermKind::Kitty),
            Some(b"\x1b]99;;api-1 finished: done\x1b\\".to_vec())
        );
        assert_eq!(
            enc(TermKind::Other),
            Some(b"\x1b]777;notify;api-1 finished;done\x07".to_vec())
        );
        assert_eq!(enc(TermKind::AppleTerminal), None, "bell only");
    }

    #[test]
    fn osc_777_title_field_cannot_smuggle_a_separator() {
        let path = NotifyPath {
            kind: TermKind::Other,
            tmux: false,
        };
        let seq = path.notification("a;b", "body;with;semis").expect("some");
        let text = String::from_utf8(seq).expect("utf8");
        assert!(text.starts_with("\x1b]777;notify;a,b;"), "{text:?}");
        assert!(text.contains("body;with;semis"), "body keeps its semis");
    }

    #[test]
    fn tmux_passthrough_wraps_and_doubles_escapes() {
        let path = NotifyPath {
            kind: TermKind::Iterm2,
            tmux: true,
        };
        let seq = path.notification("t", "b").expect("some");
        assert!(seq.starts_with(b"\x1bPtmux;\x1b\x1b]9;"), "{seq:?}");
        assert!(seq.ends_with(b"\x1b\\"), "{seq:?}");
    }

    #[test]
    fn sanitize_strips_controls_and_caps_length() {
        assert_eq!(sanitize("a\x1b]2;evil\x07b"), "a]2;evilb");
        let long = "x".repeat(200);
        let cleaned = sanitize(&long);
        assert_eq!(cleaned.chars().count(), 121, "120 chars + ellipsis");
        assert!(cleaned.ends_with('…'));
    }

    #[test]
    fn title_badge_sequence_is_an_osc_2() {
        let path = NotifyPath {
            kind: TermKind::Other,
            tmux: false,
        };
        assert_eq!(
            path.title("bitrouter ⚠1"),
            "\x1b]2;bitrouter ⚠1\x07".as_bytes()
        );
        assert_eq!(path.title_push(), b"\x1b[22;0t".to_vec());
        assert_eq!(path.title_pop(), b"\x1b[23;0t".to_vec());
    }
}
