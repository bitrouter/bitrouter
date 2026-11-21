//! Bounded directory manifests and verified reads; payload files are never executed.

use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Result, ensure};

/// Validate a portable relative material name.
pub fn relative(path: &str) -> Result<PathBuf> {
    ensure!(
        !path.is_empty() && !path.contains(['\\', '\0', ':']),
        "invalid bundle path"
    );
    let path = PathBuf::from(path);
    ensure!(
        path.components()
            .all(|part| matches!(part, Component::Normal(name) if name != ".git")),
        "bundle paths must stay inside the bundle and exclude Git administrative data"
    );
    ensure!(
        path.to_string_lossy()
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".."),
        "invalid bundle path component"
    );
    Ok(path)
}

/// Read a bounded regular file inside its selected directory.
pub fn read_regular(root: &Path, path: &Path, limit: u64) -> Result<Vec<u8>> {
    let before = std::fs::symlink_metadata(path)?;
    ensure!(
        before.is_file() && !before.is_symlink(),
        "bundle entries must be regular files, without symlinks"
    );
    ensure!(before.len() <= limit, "bundle file exceeds its byte limit");
    let canonical_root = root.canonicalize()?;
    ensure!(
        path.canonicalize()?.starts_with(&canonical_root),
        "bundle path escapes its source"
    );
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A concurrent replacement with a FIFO must not block before fd validation.
        options.custom_flags(
            (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
        );
    }
    let file = options.open(path)?;
    let opened = file.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            before.dev() == opened.dev() && before.ino() == opened.ino(),
            "bundle source changed while opening a file"
        );
    }
    ensure!(opened.is_file(), "bundle entries must be regular files");
    let mut data = Vec::new();
    file.take(limit + 1).read_to_end(&mut data)?;
    let after = std::fs::symlink_metadata(path)?;
    ensure!(
        data.len() as u64 <= limit
            && before.len() == after.len()
            && before.modified()? == after.modified()?
            && path.canonicalize()?.starts_with(canonical_root),
        "bundle source changed while reading"
    );
    Ok(data)
}
