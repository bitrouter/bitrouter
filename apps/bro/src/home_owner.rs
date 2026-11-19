//! Claim the checkpoint home before creating its first thread or lock.
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    io::{ErrorKind, Write},
    path::Path,
};

const MARKER: &str = ".bitrouter-home-role";

fn claimed(root: &Path) -> Result<bool> {
    let marker = root.join(MARKER);
    let meta = match marker.symlink_metadata() {
        Ok(meta) => meta,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    ensure!(
        meta.is_file() && meta.len() == 4 && fs::read(marker)? == b"bro\n",
        "gateway home or invalid product home marker cannot be used as BRO home: {}",
        root.display()
    );
    Ok(true)
}

fn check_directory(root: &Path) -> Result<()> {
    claimed(root)?;
    for name in [
        "bitrouter.db",
        "bitrouter.db-wal",
        "bitrouter.db-journal",
        "bitrouter.db-shm",
        "bitrouter.yaml",
        "bitrouter.start.lock",
        ".installation.lock",
        "correlation.key",
        "continuation.key",
    ] {
        match root.join(name).symlink_metadata() {
            Ok(_) => bail!(
                "gateway home cannot be used as BRO home: {} ({name})",
                root.display()
            ),
            Err(error) if error.kind() == ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(crate) fn check(root: &Path) -> Result<()> {
    let path = std::path::absolute(root)
        .with_context(|| format!("resolving BRO home {}", root.display()))?;
    for candidate in path.ancestors() {
        match candidate.canonicalize() {
            Ok(existing) => {
                for ancestor in existing.ancestors() {
                    check_directory(ancestor)?;
                }
                return Ok(());
            }
            Err(error) if error.kind() == ErrorKind::NotFound => (),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("resolving BRO home ancestor {}", candidate.display())
                });
            }
        }
    }
    Ok(())
}

pub(crate) fn claim(root: &Path) -> Result<()> {
    check(root)?;
    if claimed(root)? {
        return Ok(());
    }
    crate::durable::create_dirs(root, true)?;
    let marker = root.join(MARKER);
    match crate::durable::write_file(&marker, atomicwrites::DisallowOverwrite, |file| {
        file.write_all(b"bro\n")
    }) {
        Err(e) if e.kind() == ErrorKind::AlreadyExists => (),
        result => result?,
    }
    check(root)
}
