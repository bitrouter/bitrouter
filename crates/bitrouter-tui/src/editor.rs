//! The single-line editor, and what the keys of a raw terminal mean.
//!
//! Raw mode is not free. It means the terminal no longer echoes, no longer
//! assembles a line, and no longer turns Ctrl-C into a signal — all three
//! become the client's. [`Editor`] pays for the first two; [`is_cancel`] pays
//! for the third.
//!
//! # No I/O, no runtime
//!
//! Nothing here reads a terminal. [`Editor::apply`] is a state machine over
//! `crossterm`'s already-decoded [`KeyEvent`], so *who owns stdin* and *how
//! events are delivered* stay the caller's — and they have to, because the
//! answer depends on what else that process is selecting over. An ACP client
//! typically pumps events into a channel so its reader is cancel-safe beside
//! the session's own futures; that pump needs an async runtime, and keeping it
//! out is what lets this crate stay a synchronous library.
//!
//! Deliberately absent: history, multi-line entry, and cursor movement within
//! the line. Each is a real feature with real state, and a renderer that grew
//! them would be a text editor.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// What one key did to the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    /// Nothing — a key this editor does not bind, or a key *release*.
    Ignored,
    /// The line changed and the screen should show it.
    Changed,
    /// The line is unchanged; what is stale is the terminal (Ctrl-L).
    Redrawn,
    /// Enter. Whether an empty line counts as a submission is the caller's
    /// call, because only the caller knows what an empty prompt would do.
    Submitted,
    /// Ctrl-C or Ctrl-D. The session is over.
    Ended,
}

/// One line being typed.
#[derive(Debug, Default, Clone)]
pub struct Editor {
    line: String,
}

impl Editor {
    /// The line so far, for echoing.
    pub fn line(&self) -> &str {
        &self.line
    }

    /// Discard the line, keeping the editor.
    pub fn clear(&mut self) {
        self.line.clear();
    }

    /// Take the line and leave an empty editor behind.
    pub fn take(&mut self) -> String {
        std::mem::take(&mut self.line)
    }

    /// Apply one key.
    pub fn apply(&mut self, key: KeyEvent) -> Edit {
        // Key *release* events exist on some platforms; acting on both would
        // double every keystroke.
        if key.kind != KeyEventKind::Press {
            return Edit::Ignored;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            // Ctrl-C and Ctrl-D at an idle prompt both end the session. Ctrl-C
            // does not clear the line first: the session is idle either way,
            // and one meaning per key beats two.
            KeyCode::Char('c' | 'd') if ctrl => Edit::Ended,
            // The screen, not the line: the buffer is untouched.
            KeyCode::Char('l') if ctrl => Edit::Redrawn,
            KeyCode::Char('w') if ctrl => {
                self.delete_word();
                Edit::Changed
            }
            KeyCode::Backspace if alt => {
                self.delete_word();
                Edit::Changed
            }
            KeyCode::Backspace => {
                self.line.pop();
                Edit::Changed
            }
            KeyCode::Enter => Edit::Submitted,
            KeyCode::Char(c) if !ctrl && !alt => {
                self.line.push(c);
                Edit::Changed
            }
            _ => Edit::Ignored,
        }
    }

    /// Append a bracketed paste, flattened to one line.
    ///
    /// Bracketed paste arrives whole, which is the point: without it a pasted
    /// line is indistinguishable from a fast typist and its newline submits
    /// half of it. This editor has no second row to put the rest on, so the
    /// block becomes one line rather than a silently truncated fragment.
    pub fn paste(&mut self, text: &str) {
        self.line.push_str(&text.replace(['\n', '\r'], " "));
    }

    /// Delete the trailing word, and the whitespace before it.
    fn delete_word(&mut self) {
        let end = self.line.trim_end().len();
        // Indexed by `char_indices` rather than `rfind` + 1: whitespace is not
        // always one byte, and truncating inside a character would panic.
        let start = self.line[..end]
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map_or(0, |(i, c)| i + c.len_utf8());
        self.line.truncate(start);
    }
}

/// Does this event cancel a running turn — Ctrl-C, or `Esc`?
///
/// In raw mode the terminal no longer sends SIGINT, so every consumer that
/// wants to be interruptible has to recognise the key itself. One predicate,
/// so they all agree on what it looks like.
///
/// `Esc` belongs here only when no modal is open: a modal owns the key stream
/// while it runs, so by the time an event reaches the turn loop there is
/// nothing else for `Esc` to close.
pub fn is_cancel(event: &Event) -> bool {
    press(event).is_some_and(|key| {
        key.code == KeyCode::Esc
            || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c'))
    })
}

/// Is this Ctrl-L — redraw the screen?
///
/// The writer paints against its own model of the terminal and never asks the
/// terminal what it holds, so it cannot notice when something else writes
/// there. This is how a person tells it.
pub fn is_redraw(event: &Event) -> bool {
    press(event).is_some_and(|key| {
        key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('l')
    })
}

/// The key event behind a press, if this event is one.
fn press(event: &Event) -> Option<&KeyEvent> {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => Some(key),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn typed(editor: &mut Editor, text: &str) {
        for c in text.chars() {
            let _ = editor.apply(press(KeyCode::Char(c)));
        }
    }

    #[test]
    fn typing_and_backspace_build_the_line() {
        let mut editor = Editor::default();
        typed(&mut editor, "hello");
        let _ = editor.apply(press(KeyCode::Backspace));
        assert_eq!(editor.line(), "hell");
        assert_eq!(editor.apply(press(KeyCode::Enter)), Edit::Submitted);
    }

    /// Taking the line leaves the editor reusable rather than spent.
    #[test]
    fn taking_the_line_empties_the_editor() {
        let mut editor = Editor::default();
        typed(&mut editor, "route to opus");
        assert_eq!(editor.take(), "route to opus");
        assert_eq!(editor.line(), "");
    }

    /// Raw mode means these two keys are ours; if they were not recognised
    /// here the session would have no way out at all.
    #[test]
    fn ctrl_c_and_ctrl_d_end_the_session() {
        let mut editor = Editor::default();
        typed(&mut editor, "half a thought");
        assert_eq!(editor.apply(ctrl('c')), Edit::Ended);
        assert_eq!(editor.apply(ctrl('d')), Edit::Ended);
    }

    /// A control chord must never be mistaken for the character it carries.
    #[test]
    fn ctrl_chords_do_not_type_their_letter() {
        let mut editor = Editor::default();
        let _ = editor.apply(ctrl('a'));
        assert_eq!(editor.line(), "");
    }

    #[test]
    fn word_delete_takes_the_word_and_its_space() {
        let mut editor = Editor::default();
        typed(&mut editor, "route to opus  ");
        let _ = editor.apply(ctrl('w'));
        assert_eq!(editor.line(), "route to ");
        let _ = editor.apply(ctrl('w'));
        assert_eq!(editor.line(), "route ");
    }

    /// The reason `delete_word` counts characters instead of bytes: a
    /// non-breaking space is whitespace and is two bytes wide, so a byte
    /// index one past it lands inside a character.
    #[test]
    fn word_delete_survives_multibyte_whitespace() {
        let mut editor = Editor::default();
        typed(&mut editor, "héllo\u{a0}wörld");
        editor.delete_word();
        assert_eq!(editor.line(), "héllo\u{a0}");
        editor.delete_word();
        assert_eq!(editor.line(), "");
    }

    #[test]
    fn word_delete_on_an_empty_buffer_is_harmless() {
        let mut editor = Editor::default();
        editor.delete_word();
        assert_eq!(editor.line(), "");
    }

    /// A pasted paragraph is one prompt, not the first line of one.
    #[test]
    fn paste_is_flattened_to_a_single_line() {
        let mut editor = Editor::default();
        editor.paste("first\nsecond\r\nthird");
        assert_eq!(editor.line(), "first second  third");
    }

    /// A key release must not double the keystroke.
    #[test]
    fn a_key_release_changes_nothing() {
        let mut editor = Editor::default();
        let mut release = press(KeyCode::Char('x'));
        release.kind = KeyEventKind::Release;
        assert_eq!(editor.apply(release), Edit::Ignored);
        assert_eq!(editor.line(), "");
    }

    /// Both keys the binding table gives to turn-cancel, and nothing else.
    #[test]
    fn cancel_is_ctrl_c_or_escape() {
        assert!(is_cancel(&Event::Key(ctrl('c'))));
        assert!(is_cancel(&Event::Key(press(KeyCode::Esc))));
        assert!(
            !is_cancel(&Event::Key(ctrl('d'))),
            "Ctrl-D ends the session"
        );
        assert!(!is_cancel(&Event::Key(press(KeyCode::Char('c')))));
        assert!(!is_cancel(&Event::Paste("c".to_string())));
    }

    #[test]
    fn redraw_is_ctrl_l_and_nothing_else() {
        assert!(is_redraw(&Event::Key(ctrl('l'))));
        assert!(!is_redraw(&Event::Key(press(KeyCode::Char('l')))));
        assert!(!is_redraw(&Event::Key(ctrl('c'))));
    }

    /// Ctrl-L asks for the screen, not for a change to the line.
    #[test]
    fn ctrl_l_leaves_the_line_alone() {
        let mut editor = Editor::default();
        typed(&mut editor, "half typed");
        assert_eq!(editor.apply(ctrl('l')), Edit::Redrawn);
        assert_eq!(editor.line(), "half typed");
    }
}
