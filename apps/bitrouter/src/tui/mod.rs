//! `bitrouter status --watch` — the live view of what the router is doing.
//!
//! Read-only by construction, with one deliberate exception (`r` → reload).
//! The previous TUI in this codebase died by accreting mutation verbs until it
//! became an agent orchestrator, so the rule here is external: **if there is
//! no CLI subcommand for it, there is no keystroke for it**, and every
//! mutating key echoes the command it ran. Growing the surface then requires
//! adding a CLI command first, which gets normal review.
//!
//! See `docs/OBSERVABILITY_TUI_SPEC.md`.

#[cfg(unix)]
pub mod host;
pub mod lifecycle;
pub mod pty;
pub mod render;
pub mod snapshot;
pub mod term;
/// The interactive view. Unix-only, gated here and nowhere else.
#[cfg(unix)]
pub mod watch;

use std::path::Path;

use anyhow::Result;

use crate::metering::store::TimeWindow;

/// Run `bitrouter status --watch`.
///
/// A non-terminal stdout prints one snapshot and exits instead of refusing:
/// `bitrouter status --watch --json | jq` is how people will script against
/// this, and a view that only works interactively is a worse view.
pub async fn run_watch(
    source: &crate::paths::ConfigSource,
    socket: &Path,
    window: TimeWindow,
) -> Result<()> {
    // Unix only, stated rather than half-working. The rest of the binary does
    // support Windows, but this view's exits depend on SIGTERM/SIGHUP, which
    // have no equivalent here — and a view that cannot guarantee it restores
    // the terminal is the one thing worse than no view. Piping still works
    // everywhere: that path never touches the terminal.
    #[cfg(not(unix))]
    {
        let _ = (source, socket, window);
        anyhow::bail!(
            "`status --watch` is unix-only today. Pipe it for a one-shot snapshot \
             (`bitrouter status --watch | more`), or use `bitrouter status`."
        );
    }
    #[cfg(unix)]
    {
        run_watch_unix(source, socket, window).await
    }
}

#[cfg(unix)]
async fn run_watch_unix(
    source: &crate::paths::ConfigSource,
    socket: &Path,
    window: TimeWindow,
) -> Result<()> {
    lifecycle::enter(lifecycle::Input::Keys)?;
    lifecycle::install_panic_restore();
    let result = watch::event_loop(source, socket, window).await;
    lifecycle::restore();
    result
}

/// One plain-text snapshot, for a redirected stdout.
pub async fn oneshot_text(
    source: &crate::paths::ConfigSource,
    socket: &Path,
    window: TimeWindow,
) -> String {
    render::oneshot(&snapshot::poll(source, socket, window).await)
}
