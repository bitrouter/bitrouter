//! Ctrl+C cancel signaling for the headless agent driver.
//!
//! First Ctrl+C cooperates: a `Notify` is fired so the driver can send
//! ACP `session/cancel` and drain remaining events. Second Ctrl+C
//! escalates to `std::process::exit(130)` — the subprocess agent dies
//! via `kill_on_drop(true)` set on `tokio::process::Command` inside
//! `bitrouter-acp`, so no `unsafe` SIGKILL is needed here.

use std::sync::Arc;

use tokio::sync::Notify;

/// Spawn a background task watching for Ctrl+C.
///
/// Returns a `Notify` the caller can `.notified().await` to learn about
/// the first interrupt. The task self-terminates after the second
/// interrupt by exiting the process; callers do not need to await the
/// task.
pub fn cancel_token() -> Arc<Notify> {
    let notify = Arc::new(Notify::new());
    let token = notify.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            token.notify_waiters();
        }
        if tokio::signal::ctrl_c().await.is_ok() {
            std::process::exit(130);
        }
    });
    notify
}
