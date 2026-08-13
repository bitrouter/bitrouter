//! Terminal lifecycle for the live view: enter, restore, and the three ways
//! out.
//!
//! Restoration is the whole point of this module. A TUI that leaves the shell
//! in raw mode with no echo is worse than one that never drew, and there are
//! **three** exits — normal teardown, a panic, and a signal — of which only
//! the first two run Rust code the loop controls. Every one of them ends at
//! [`restore`], which takes no handle precisely so a panic hook or a signal
//! branch can call it from anywhere.

use std::io::Write;

use anyhow::Result;
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

/// Put the terminal into the state the view draws against: raw mode,
/// alternate screen, key reporting, and the user's window title saved so
/// [`restore`] can put it back.
///
/// Any failure here returns `Err` **after** undoing whatever already
/// succeeded — the panic hook is installed by the caller only once this
/// returns `Ok`, so a half-entered terminal must clean up after itself.
pub fn enter() -> Result<()> {
    enable_raw_mode()?;
    let mut out = std::io::stdout();
    if let Err(e) = execute!(out, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(e.into());
    }
    // XTWINOPS push: save the window title so a restored terminal does not
    // keep whatever the view (or a suspended child) set.
    let _ = write!(out, "\x1b[22;0t");
    let _ = out.flush();
    Ok(())
}

/// Best-effort return to the terminal the user had. Needs no handle so the
/// panic hook and the signal branch can both call it; every step is
/// independently fallible and independently ignored, because a partially
/// restored terminal still beats an unrestored one.
pub fn restore() {
    let _ = disable_raw_mode();
    let mut out = std::io::stdout();
    let _ = execute!(out, LeaveAlternateScreen, crossterm::cursor::Show);
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

/// Hand the real terminal to a child (`$EDITOR`, `providers login`), run it,
/// and take the screen back.
///
/// This is what lets the view offer management without owning any of it: the
/// commands that edit config or handle credentials already exist and already
/// know how to prompt, so the view leaves rather than reimplementing them.
///
/// The caller must not draw until this returns, and must force a full redraw
/// afterwards — the child owned the screen and the view's idea of it is stale.
pub async fn suspend<F, Fut>(run: F) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    restore();
    let result = run().await;
    // Re-enter even when the child failed: the alternative is returning to a
    // caller that is about to draw into a cooked terminal.
    let reentered = enter();
    result.and(reentered)
}
