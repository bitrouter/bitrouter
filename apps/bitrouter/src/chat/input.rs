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
//! # What is here, and what is in the crate
//!
//! This module is the **transport**: owning stdin, pumping events, and being
//! cancel-safe in a `select!`. All of that is async, and all of it is a fact
//! about this process rather than about terminals in general.
//!
//! What the keys *mean* — the line editor, and which chords cancel or redraw —
//! is [`bitrouter_tui::editor`], which is synchronous and has no I/O. That
//! split is why the renderer crate still needs no runtime: the pump is the
//! only part that did.

use crossterm::event::Event;
use futures::StreamExt as _;

use bitrouter_tui::editor::{Edit, Editor};

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
        bitrouter_tui::lifecycle::enter_raw()?;
        if let Err(e) =
            crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste)
        {
            bitrouter_tui::lifecycle::restore();
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
    /// the caller's loop free of a "not really a prompt" case — and is why
    /// [`Edit::Submitted`] does not decide it for us.
    pub async fn read_line<E>(&mut self, mut echo: E) -> anyhow::Result<Prompt>
    where
        E: FnMut(Echo<'_>) -> anyhow::Result<()>,
    {
        let mut editor = Editor::default();
        echo(Echo::Changed(editor.line()))?;
        while let Some(event) = self.next_event().await {
            match event {
                Event::Key(key) => match editor.apply(key) {
                    Edit::Ignored => {}
                    Edit::Changed => echo(Echo::Changed(editor.line()))?,
                    Edit::Redrawn => echo(Echo::Redraw(editor.line()))?,
                    Edit::Submitted => {
                        if editor.line().trim().is_empty() {
                            editor.clear();
                            echo(Echo::Changed(editor.line()))?;
                        } else {
                            return Ok(Prompt::Line(editor.take()));
                        }
                    }
                    Edit::Ended => return Ok(Prompt::End),
                },
                // Bracketed paste arrives whole, which is the point: without
                // it a pasted line is indistinguishable from a fast typist and
                // its newline submits half of it.
                Event::Paste(text) => {
                    editor.paste(&text);
                    echo(Echo::Changed(editor.line()))?;
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
        bitrouter_tui::lifecycle::restore();
    }
}
