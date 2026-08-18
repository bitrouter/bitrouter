//! The one exit that is this process's rather than the terminal's.
//!
//! Entering and restoring the terminal live in [`bitrouter_tui::lifecycle`],
//! because they are properties of a terminal and that crate is what draws to
//! one. Signals are not: which of them exist, and what a process wants to do
//! about them, is a fact about *this* binary. So the registration stays here.

/// The signals a terminal session has to survive, registered once.
///
/// SIGINT is registered too: `kill -2` is a common supervisor default, and
/// leaving it on the default disposition means the one signal users reach for
/// most is the one that breaks their shell.
#[cfg(unix)]
struct ShutdownSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ShutdownSignals {
    /// Register once, before the loop.
    ///
    /// Constructing these per iteration — the obvious `select!` spelling —
    /// silently drops signals: a `Signal` only observes what arrives after it
    /// exists, and the first registration has already replaced the default
    /// disposition. A signal delivered while the loop was awaiting some other
    /// branch would then neither kill the process nor end the loop.
    fn install() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
            hangup: signal(SignalKind::hangup())?,
        })
    }

    /// Resolve when any of them fires.
    async fn recv(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
            _ = self.hangup.recv() => {}
        }
    }
}

/// The third exit, as a `select!` arm.
///
/// Normal teardown and a panic both run Rust code that can reach
/// [`bitrouter_tui::lifecycle::restore`]. A signal runs none: `kill`, a
/// supervisor stopping the process, or a closed terminal window sending SIGHUP
/// would otherwise end it with the terminal still in raw mode.
///
/// Registration that fails, and every platform without these signals, resolve
/// **never** rather than immediately: an arm that fired at once would end the
/// session on its first poll, which is worse than the exit it is guarding.
pub struct Shutdown {
    #[cfg(unix)]
    signals: Option<ShutdownSignals>,
}

impl Shutdown {
    /// Register once, before the loop.
    pub fn install() -> Self {
        Self {
            #[cfg(unix)]
            signals: ShutdownSignals::install().ok(),
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
}
