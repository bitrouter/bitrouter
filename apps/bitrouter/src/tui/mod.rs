//! `bitrouter status --requests` — what the router has actually done.
//!
//! A table of settled requests read straight from the metering store, plus a
//! spend rollup. Printed once and exited, never drawn: this module holds a
//! data layer and a text formatter, and nothing here takes the terminal.
//!
//! # Why there is no live view any more
//!
//! There was one — a ratatui table refreshing once a second, with `r` (reload)
//! and `e` (`$EDITOR`) bound to real `DaemonCommand`s. It was removed because
//! it was the last thing in this binary that drew widgets, and it is the one
//! surface whose data — daemon-wide request rows across every caller, most of
//! which never speak ACP — cannot move to `bitrouter-tui` without importing a
//! daemon-wide model into a session-scoped crate. Deleting it is what lets
//! `apps/bitrouter` drop `ratatui` entirely, so that **only the renderer crate
//! can draw**, checked by the build rather than by review.
//!
//! What it did that this does not: refresh on its own, and scroll. `watch -n1
//! bitrouter status --requests` covers the first, and a pager the second. The
//! two mutating keys were already `bitrouter reload` and `$EDITOR` on
//! `bitrouter.yaml`, which is what the help pane said they were.
//!
//! See `docs/OBSERVABILITY_TUI_SPEC.md`, whose live-view half this supersedes.

pub mod lifecycle;
pub mod render;
pub mod snapshot;

use std::path::Path;

use crate::metering::store::TimeWindow;

/// One plain-text snapshot of the request stream and the spend rollup.
///
/// Portable, and identical whether stdout is a terminal or a pipe — which is
/// what makes `bitrouter status --requests | …` and an agent reading the same
/// bytes the same thing.
pub async fn oneshot_text(
    source: &crate::paths::ConfigSource,
    socket: &Path,
    window: TimeWindow,
) -> String {
    render::oneshot(&snapshot::poll(source, socket, window, None).await)
}
