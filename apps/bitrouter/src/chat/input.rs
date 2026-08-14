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
        E: FnMut(&str) -> anyhow::Result<()>,
    {
        let mut buffer = String::new();
        echo(&buffer)?;
        while let Some(event) = self.next_event().await {
            match event {
                Event::Key(key) => match edit(&mut buffer, key) {
                    Edit::Ignored => {}
                    Edit::Changed => echo(&buffer)?,
                    Edit::Submitted => {
                        if buffer.trim().is_empty() {
                            buffer.clear();
                            echo(&buffer)?;
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
                    echo(&buffer)?;
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

/// Is this event the interrupt — Ctrl-C?
///
/// In raw mode the terminal no longer sends SIGINT, so every consumer that
/// wants to be interruptible has to recognise the key itself. One predicate,
/// so they all agree on what it looks like.
pub fn is_interrupt(event: &Event) -> bool {
    match event {
        Event::Key(key) => {
            key.kind == KeyEventKind::Press
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && key.code == KeyCode::Char('c')
        }
        _ => false,
    }
}

/// What one key did to the buffer.
enum Edit {
    Ignored,
    Changed,
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

    #[test]
    fn interrupt_is_ctrl_c_and_nothing_else() {
        assert!(is_interrupt(&Event::Key(ctrl('c'))));
        assert!(!is_interrupt(&Event::Key(ctrl('d'))));
        assert!(!is_interrupt(&Event::Key(press(KeyCode::Char('c')))));
        assert!(!is_interrupt(&Event::Paste("c".to_string())));
    }
}
