//! Terminal custody: enter the state a view draws against, and give it back.
//!
//! Restoration is the whole point of this module. A TUI that leaves the shell
//! in raw mode with no echo is worse than one that never drew, and there are
//! **three** exits — normal teardown, a panic, and a signal — of which only
//! the first two run Rust code the loop controls. Both end at [`restore`],
//! which takes no handle precisely so a panic hook can call it from anywhere.
//! The third is the caller's: registering for signals means naming them, and
//! which signals exist is a property of the host process, not of the terminal.
//!
//! # Why this lives here
//!
//! [`Writer::new`](crate::writer::Writer::new) asks the terminal where the
//! cursor is before it draws a single row, so entering raw mode and opening
//! the view are two halves of one ordering contract. Splitting them across a
//! crate boundary left that contract documented in two places and enforced in
//! neither. Terminal custody is one responsibility; it belongs in one module,
//! and this crate is already the one holding `crossterm`.

use std::io::{self, Write};

use crossterm::execute;
use crossterm::terminal::{LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};

/// Take raw mode, and nothing else.
///
/// The inline chat renderer wants the keys but not the screen switch — its
/// whole design is that finished rows stay in ordinary scrollback. Splitting
/// this out is also what keeps `enable_raw_mode` to a single call site, so
/// there is exactly one thing for [`restore`] to undo.
pub fn enter_raw() -> io::Result<()> {
    enable_raw_mode()
}

/// Best-effort return to the terminal the user had. Needs no handle so the
/// panic hook and a caller's signal branch can both reach it; every step is
/// independently fallible and independently ignored, because a partially
/// restored terminal still beats an unrestored one.
pub fn restore() {
    let _ = disable_raw_mode();
    let mut out = io::stdout();
    // Bracketed paste is the chat session's, not the full-screen view's, and
    // disabling a mode that was never enabled costs nothing — which is why it
    // belongs here, in the one function every exit already reaches.
    let _ = execute!(
        out,
        crossterm::event::DisableBracketedPaste,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
    // XTWINOPS pop: put the user's window title back.
    let _ = write!(out, "\x1b[23;0t");
    let _ = out.flush();
}

/// Chain [`restore`] in front of the current panic hook, so a panic anywhere
/// in the draw path leaves a usable terminal and the message lands on a
/// screen the user can actually read.
pub fn install_panic_restore() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The panic exit is a *chain*: [`restore`] runs, and then whatever hook
    /// was already installed still reports the panic. A hook that replaced its
    /// predecessor would restore the terminal and swallow the message that
    /// says why the session died.
    #[test]
    fn the_panic_hook_restores_and_still_reports() {
        let reported = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&reported);
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |_| {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }));

        install_panic_restore();
        let panicked = std::panic::catch_unwind(|| panic!("a panic mid-draw"));
        std::panic::set_hook(original);

        assert!(panicked.is_err(), "the panic must still propagate");
        assert!(
            reported.load(std::sync::atomic::Ordering::SeqCst),
            "the previous hook must still run, so the panic is still reported"
        );
        assert!(
            !crossterm::terminal::is_raw_mode_enabled().unwrap_or(true),
            "the terminal must not be left in raw mode by a panic"
        );
    }
}
