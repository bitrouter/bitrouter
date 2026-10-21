//! Process session-log diagnostics shared by launchers and non-terminal clients.

use anyhow::{Context, Result};

/// This process's session log, once the subscriber has opened one.
///
/// A global because the path is decided during subscriber init — before any
/// command is dispatched, and the only moment it exists. A session that ends
/// badly needs it much later, and threading it through every launch path to
/// serve the failure case would cost more than it is worth.
static SESSION_LOG: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Record where this process's session log lives. Called once at startup.
pub fn remember_session_log(path: std::path::PathBuf) {
    let _ = SESSION_LOG.set(path);
}

/// Write the end of the session log, naming the file.
///
/// Called only when something went wrong: a permanent pane would cost rows on
/// every session to serve the rare one that fails. An unreadable log is
/// reported as unreadable rather than skipped — "there is no log" is the kind
/// of thing a user needs told, not hidden.
///
/// The path is this process's, so it is read here; the tail is *rendered* by
/// the crate, which is why nothing below names a widget.
pub(crate) fn write_session_log_tail(out: &mut impl std::io::Write) -> Result<()> {
    let Some(path) = SESSION_LOG.get() else {
        return Ok(());
    };
    let log = match std::fs::read_to_string(path) {
        Ok(log) => log,
        Err(e) => format!("(could not read the session log: {e})"),
    };
    writeln!(out, "session log: {}", path.display()).context("writing the session log path")?;
    let lines: Vec<_> = log.lines().collect();
    let skipped = lines.len().saturating_sub(20);
    if skipped > 0 {
        writeln!(out, "  … {skipped} earlier lines in the file above")
            .context("writing the log omission")?;
    }
    for line in lines.into_iter().skip(skipped) {
        writeln!(out, "  {line}").context("writing the session log tail")?;
    }
    Ok(())
}

/// Show the end of the session log on **stderr**, for a failure that happened
/// before there was a view to draw it in.
///
/// A piped launch that dies during the handshake returns an error before a
/// transcript exists. Its harness child's stderr is in the session file, so
/// writing the tail beside the failure lets the caller see the actionable
/// diagnostic without knowing the file path.
///
/// The launch error outranks this, so a tail that cannot be written is
/// dropped rather than replacing the reason the launch failed.
pub(crate) fn report_failed_launch() {
    let _ = write_session_log_tail(&mut std::io::stderr());
}
