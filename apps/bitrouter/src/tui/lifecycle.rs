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

/// Take raw mode, and nothing else.
///
/// The inline chat renderer wants the keys but not the screen switch — its
/// whole design is that finished rows stay in ordinary scrollback. Splitting
/// this out is also what keeps `enable_raw_mode` to a single call site in the
/// binary, so there is exactly one thing for [`restore`] to undo.
pub fn enter_raw() -> Result<()> {
    enable_raw_mode()?;
    Ok(())
}

/// Put the terminal into the state the view draws against: raw mode,
/// alternate screen, key reporting, and the user's window title saved so
/// [`restore`] can put it back.
///
/// Any failure here returns `Err` **after** undoing whatever already
/// succeeded — the panic hook is installed by the caller only once this
/// returns `Ok`, so a half-entered terminal must clean up after itself.
pub fn enter() -> Result<()> {
    enter_raw()?;
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
    // Bracketed paste is the chat session's, not this view's, and disabling a
    // mode that was never enabled costs nothing — which is why it belongs
    // here, in the one function every exit already reaches.
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

/// The third exit, as a `select!` arm.
///
/// Normal teardown and a panic both run Rust code that can reach [`restore`].
/// A signal runs none: `kill`, a supervisor stopping the process, or a closed
/// terminal window sending SIGHUP would otherwise end it with the terminal
/// still in raw mode. This wraps [`crate::tui::watch::ShutdownSignals`] — the
/// registration is the same, only the caller is new — so an inline session can
/// await it alongside its own futures and leave by the front door.
///
/// Registration that fails, and every platform without these signals, resolve
/// **never** rather than immediately: an arm that fired at once would end the
/// session on its first poll, which is worse than the exit it is guarding.
pub struct Shutdown {
    #[cfg(unix)]
    signals: Option<crate::tui::watch::ShutdownSignals>,
}

impl Shutdown {
    /// Register once, before the loop — see `ShutdownSignals::install`.
    pub fn install() -> Self {
        Self {
            #[cfg(unix)]
            signals: crate::tui::watch::ShutdownSignals::install().ok(),
        }
    }

    /// Resolve when one of them fires.
    pub async fn recv(&mut self) {
        #[cfg(unix)]
        if let Some(signals) = self.signals.as_mut() {
            signals.recv().await;
            return;
        }
        std::future::pending().await
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The signal arm must be *quiet*. It sits in a `select!` next to the
    /// futures that do the session's work, and one that resolved on its first
    /// poll would end every session the instant it started — including on the
    /// platforms and sandboxes where registration is not available at all.
    #[tokio::test]
    async fn the_shutdown_arm_does_not_fire_on_its_own() {
        let mut shutdown = Shutdown::install();
        let fired =
            tokio::time::timeout(std::time::Duration::from_millis(50), shutdown.recv()).await;
        assert!(
            fired.is_err(),
            "the shutdown arm resolved with no signal delivered"
        );
    }

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
