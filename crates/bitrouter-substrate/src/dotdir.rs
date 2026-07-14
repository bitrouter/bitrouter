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
