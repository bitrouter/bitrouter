//! Optional explicit-path credential storage. This module discovers no account,
//! application directory or environment credential. Locks and retained rotations
//! coordinate only this process; callers coordinate other processes separately.

pub mod backend;
pub mod snapshot;

use std::path::{Path, PathBuf};

// Bind both administrative writes and refresh leases to the same selected file.
// Canonicalize existing ancestors before appending missing parents, preserving
// existing symlink/.. semantics and aliases before the first file is created.
fn selected_path(input: &Path) -> std::io::Result<PathBuf> {
    let absolute = if input.is_absolute() {
        input.to_path_buf()
    } else {
        std::env::current_dir()?.join(input)
    };
    for ancestor in absolute.ancestors() {
        if let Ok(base) = std::fs::canonicalize(ancestor) {
            let suffix = absolute
                .strip_prefix(ancestor)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
            return Ok(if suffix.as_os_str().is_empty() {
                base
            } else {
                base.join(suffix)
            });
        }
    }
    Ok(absolute)
}
