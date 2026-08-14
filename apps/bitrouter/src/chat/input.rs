//! The one owner of stdin for a chat session.
//!
//! # Why one owner, and why raw mode for the whole session
//!
//! Two readers of a terminal is a race, not a design: whichever future is
//! polled first takes the keystroke, and the other one waits for a key the
//! user already pressed. The previous shape had three — a line reader for
//! prompts and a fresh `EventStream` per modal — and each modal took raw mode
//! on entry and dropped it on exit, so a panic inside a prompt left the shell
//! without echo.
//!
//! So: one [`Stdin`], holding raw mode from the first prompt to teardown, and
//! every consumer reads through it.
//!
//! Raw mode is not free. It means the terminal no longer echoes, no longer
//! assembles a line, and no longer turns Ctrl-C into a signal — all three
//! become ours. [`Stdin::read_line`] is the line editor that pays for the
//! first two; [`is_interrupt`] and [`Prompt::End`] pay for the third.
//!
//! Deliberately absent: history, multi-line entry, and cursor movement within
//! the line. Each is a real feature with real state, and none of them is what
//! this task is for.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt as _;

/// Why the line is being echoed.
pub enum Echo<'a> {
    /// The line changed and the screen should show it.
    Changed(&'a str),
    /// The user asked for the whole screen back (Ctrl-L). The line is
    /// unchanged; what is stale is the terminal.
    Redraw(&'a str),
}

/// How a prompt read ended.
pub enum Prompt {
    /// A submitted line, already known to be non-blank.
    Line(String),
    /// The session is over — Ctrl-C or Ctrl-D at an idle prompt, or stdin
    /// itself ended.
    End,
}

/// The single reader of the terminal, and the holder of raw mode.
///
/// Raw mode is released on drop, which is what makes an early `?` from
/// anywhere in the session safe: the shell comes back even on the paths
/// nobody wrote an exit for.
pub struct Stdin {
    events: tokio::sync::mpsc::UnboundedReceiver<Event>,
    reader: tokio::task::JoinHandle<()>,
}

impl Stdin {
    /// Take raw mode and start reading.
    ///
    /// The reader is a task rather than an inline `EventStream` so that
    /// [`Stdin::next_event`] is a channel receive — cancel-safe, and therefore
    /// usable as a `select!` arm alongside the turn's own futures. A stream
    /// polled from a `select!` arm that loses the race would be dropped
    /// mid-poll, and the keystroke with it.
    pub fn open() -> anyhow::Result<Self> {
        crate::tui::lifecycle::enter_raw()?;
        if let Err(e) =
            crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste)
        {
            crate::tui::lifecycle::restore();
            return Err(e.into());
        }
        let (tx, events) = tokio::sync::mpsc::unbounded_channel();
        let reader = tokio::spawn(async move {
            let mut stream = crossterm::event::EventStream::new();
            // A read error on the terminal ends input rather than being
            // retried: the session's answer to "stdin is gone" is to end, and
            // a loop that swallowed the error would spin on it.
            while let Some(Ok(event)) = stream.next().await {
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
        Ok(Self { events, reader })
    }

    /// The next terminal event, or `None` once input has ended.
    pub async fn next_event(&mut self) -> Option<Event> {
        self.events.recv().await
    }

    /// Read one line, calling `echo` with the buffer after every change.
    ///
    /// The echo is a callback because this type owns the keys and the caller
    /// owns the screen; handing it a renderer would put a second drawer of the
    /// live row in the process.
    ///
    /// A blank submission is swallowed here rather than returned, which keeps
    /// the caller's loop free of a "not really a prompt" case.
    pub async fn read_line<E>(&mut self, mut echo: E) -> anyhow::Result<Prompt>
    where
        E: FnMut(Echo<'_>) -> anyhow::Result<()>,
    {
        let mut buffer = String::new();
        echo(Echo::Changed(&buffer))?;
        while let Some(event) = self.next_event().await {
            match event {
                Event::Key(key) => match edit(&mut buffer, key) {
                    Edit::Ignored => {}
                    Edit::Changed => echo(Echo::Changed(&buffer))?,
                    Edit::Redrawn => echo(Echo::Redraw(&buffer))?,
                    Edit::Submitted => {
                        if buffer.trim().is_empty() {
                            buffer.clear();
                            echo(Echo::Changed(&buffer))?;
                        } else {
                            return Ok(Prompt::Line(buffer));
                        }
                    }
                    Edit::Ended => return Ok(Prompt::End),
                },
                // Bracketed paste arrives whole, which is the point: without
                // it a pasted line is indistinguishable from a fast typist and
                // its newline submits half of it.
                Event::Paste(text) => {
                    buffer.push_str(&flatten(&text));
                    echo(Echo::Changed(&buffer))?;
                }
                _ => {}
            }
        }
        Ok(Prompt::End)
    }
}

impl Drop for Stdin {
    fn drop(&mut self) {
        self.reader.abort();
        crate::tui::lifecycle::restore();
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
/// The renderer paints against its own model of the terminal and never asks
/// the terminal what it holds, so it cannot notice when something else writes
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

/// What one key did to the buffer.
enum Edit {
    Ignored,
    Changed,
    /// The buffer is unchanged; the screen is what needs redoing.
    Redrawn,
    Submitted,
    Ended,
}

fn edit(buffer: &mut String, key: KeyEvent) -> Edit {
    // Key *release* events exist on some platforms; acting on both would
    // double every keystroke.
    if key.kind != KeyEventKind::Press {
        return Edit::Ignored;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        // Ctrl-C and Ctrl-D at an idle prompt both end the session. Ctrl-C
        // does not clear the line first: the session is idle either way, and
        // one meaning per key beats two.
        KeyCode::Char('c' | 'd') if ctrl => Edit::Ended,
        // The screen, not the line: the buffer is untouched.
        KeyCode::Char('l') if ctrl => Edit::Redrawn,
        KeyCode::Char('w') if ctrl => {
            delete_word(buffer);
            Edit::Changed
        }
        KeyCode::Backspace if alt => {
            delete_word(buffer);
            Edit::Changed
        }
        KeyCode::Backspace => {
            buffer.pop();
            Edit::Changed
        }
        KeyCode::Enter => Edit::Submitted,
        KeyCode::Char(c) if !ctrl && !alt => {
            buffer.push(c);
            Edit::Changed
        }
        _ => Edit::Ignored,
    }
}

/// Delete the trailing word, and the whitespace before it.
fn delete_word(buffer: &mut String) {
    let end = buffer.trim_end().len();
    // Indexed by `char_indices` rather than `rfind` + 1: whitespace is not
    // always one byte, and truncating inside a character would panic.
    let start = buffer[..end]
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    buffer.truncate(start);
}

/// A pasted block becomes one line: this editor has no second row to put the
/// rest on, and silently submitting at the first newline would send a
/// fragment.
fn flatten(text: &str) -> String {
    text.replace(['\n', '\r'], " ")
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

    fn typed(buffer: &mut String, text: &str) {
        for c in text.chars() {
            let _ = edit(buffer, press(KeyCode::Char(c)));
        }
    }

    #[test]
    fn typing_and_backspace_build_the_line() {
        let mut buffer = String::new();
        typed(&mut buffer, "hello");
        let _ = edit(&mut buffer, press(KeyCode::Backspace));
        assert_eq!(buffer, "hell");
        assert!(matches!(
            edit(&mut buffer, press(KeyCode::Enter)),
            Edit::Submitted
        ));
    }

    /// Raw mode means these two keys are ours; if they were not recognised
    /// here the session would have no way out at all.
    #[test]
    fn ctrl_c_and_ctrl_d_end_the_session() {
        let mut buffer = String::from("half a thought");
        assert!(matches!(edit(&mut buffer, ctrl('c')), Edit::Ended));
        assert!(matches!(edit(&mut buffer, ctrl('d')), Edit::Ended));
    }

    /// A control chord must never be mistaken for the character it carries.
    #[test]
    fn ctrl_chords_do_not_type_their_letter() {
        let mut buffer = String::new();
        let _ = edit(&mut buffer, ctrl('a'));
        assert_eq!(buffer, "");
    }

    #[test]
    fn word_delete_takes_the_word_and_its_space() {
        let mut buffer = String::new();
        typed(&mut buffer, "route to opus  ");
        let _ = edit(&mut buffer, ctrl('w'));
        assert_eq!(buffer, "route to ");
        let _ = edit(&mut buffer, ctrl('w'));
        assert_eq!(buffer, "route ");
    }

    /// The reason `delete_word` counts characters instead of bytes: a
    /// non-breaking space is whitespace and is two bytes wide, so a byte
    /// index one past it lands inside a character.
    #[test]
    fn word_delete_survives_multibyte_whitespace() {
        let mut buffer = String::from("héllo\u{a0}wörld");
        delete_word(&mut buffer);
        assert_eq!(buffer, "héllo\u{a0}");
        delete_word(&mut buffer);
        assert_eq!(buffer, "");
    }

    #[test]
    fn word_delete_on_an_empty_buffer_is_harmless() {
        let mut buffer = String::new();
        delete_word(&mut buffer);
        assert_eq!(buffer, "");
    }

    /// A pasted paragraph is one prompt, not the first line of one.
    #[test]
    fn paste_is_flattened_to_a_single_line() {
        assert_eq!(flatten("first\nsecond\r\nthird"), "first second  third");
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
        let mut buffer = String::from("half typed");
        assert!(matches!(edit(&mut buffer, ctrl('l')), Edit::Redrawn));
        assert_eq!(buffer, "half typed");
    }
}
