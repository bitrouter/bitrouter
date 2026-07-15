//! The per-repo `.bitrouter/` state directory — created **self-ignoring**.
//!
//! Session records, transcripts, worktrees, and fleet state all live under
//! `<base_repo>/.bitrouter/`. None of it belongs in version control (records
//! carry pids and absolute paths; transcripts carry whole conversations), so
//! the directory is created with a `.gitignore` containing `*` — the same
//! trick cargo uses for `target/` — instead of trusting every repo to ignore
//! it. An existing `.gitignore` is never overwritten.

use std::path::Path;

/// Create `dot_dir` (and parents) if needed and drop a self-ignoring
/// `.gitignore` into it unless one already exists.
pub fn ensure_self_ignored(dot_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dot_dir)?;
    let gitignore = dot_dir.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, "*\n")?;
    }
    Ok(())
}

/// Repoint this process's stderr (fd 2) at `path` (created, append).
///
/// One mechanism catches every stderr writer at once: the process's own
/// tracing subscriber AND any child that inherits stderr — which is how a
/// full-screen TUI keeps a chatty agent child (or its own `tracing::warn!`)
/// from scribbling over the raw-mode alternate screen. Unix only (`dup2`);
/// returns whether the redirect happened. No test exercises this directly:
/// hijacking fd 2 inside a shared test process would swallow every later
/// test's panic output under plain `cargo test`.
#[cfg(unix)]
pub fn redirect_stderr_to(path: &Path) -> bool {
    use std::os::unix::io::AsRawFd;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return false;
    };
    // SAFETY: dup2 atomically repoints fd 2 at the open log file; the
    // original descriptor closes on drop while fd 2 keeps the file
    // description alive.
    unsafe { libc::dup2(file.as_raw_fd(), 2) == 2 }
}

/// Non-Unix stub: no fd-level redirect; callers keep inherited stderr.
#[cfg(not(unix))]
pub fn redirect_stderr_to(_path: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_dir_and_self_ignoring_gitignore() {
        let base = tempfile::tempdir().expect("tempdir");
        let dot = base.path().join(".bitrouter");
        ensure_self_ignored(&dot).expect("ensure");
        assert_eq!(
            std::fs::read_to_string(dot.join(".gitignore")).expect("read"),
            "*\n"
        );
    }

    #[test]
    fn never_overwrites_an_existing_gitignore() {
        let base = tempfile::tempdir().expect("tempdir");
        let dot = base.path().join(".bitrouter");
        std::fs::create_dir_all(&dot).expect("mkdir");
        std::fs::write(dot.join(".gitignore"), "sessions/\n").expect("write");
        ensure_self_ignored(&dot).expect("ensure");
        assert_eq!(
            std::fs::read_to_string(dot.join(".gitignore")).expect("read"),
            "sessions/\n",
            "a user-authored ignore file is preserved"
        );
    }
}
