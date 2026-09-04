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
}

impl Drop for Stdin {
    fn drop(&mut self) {
        self.reader.abort();
        bitrouter_tui::lifecycle::restore();
    }
}
