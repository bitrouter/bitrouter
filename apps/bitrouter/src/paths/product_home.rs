//! Recognize BRO state without opening a checkpoint database or acquiring a lock.
use anyhow::{Result, bail, ensure};
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

fn present(path: &Path) -> std::io::Result<bool> {
    match path.symlink_metadata() {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

fn legacy_threads(home: &Path) -> Result<bool> {
    let dir = home.join("threads");
    if !dir.is_dir() {
        return Ok(false);
    }
    if present(&dir.join("admission.lock"))? {
        return Ok(true);
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "json")
            && path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| uuid::Uuid::parse_str(s).is_ok())
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn staged_store(home: &Path) -> Result<bool> {
    let path = home.join("store.sqlite");
    if !path.is_file() {
        return Ok(false);
    }
    // The staged importer sets the SQLite application_id prior to publication.
    let mut header = [0; 72];
    match fs::File::open(path)?.read_exact(&mut header) {
        Ok(()) => Ok(header.starts_with(b"SQLite format 3\0") && &header[68..72] == b"BRST"),
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn check_directory(home: &Path) -> Result<()> {
    let marker = home.join(".bitrouter-home-role");
    if present(&marker)? {
        let meta = marker.symlink_metadata()?;
        ensure!(
            meta.is_file() && meta.len() == 10 && fs::read(&marker)? == b"bitrouter\n",
            "BRO home or invalid product home marker cannot be used by the gateway: {}",
            home.display()
        );
    }
    if (present(&home.join("ACTIVATED"))? && present(&home.join("store.sqlite"))?)
        || legacy_threads(home)?
        || staged_store(home)?
    {
        bail!(
            "BRO home cannot be used as a BitRouter gateway home: {}",
            home.display()
        );
    }
    Ok(())
}

pub(super) fn check(home: &Path) -> Result<()> {
    // Resolve the deepest existing parent first. A new config path or an alias
    // to a BRO checkpoint subdirectory must respect the same ownership.
    let path = absolute_home(home)?;
    for candidate in path.ancestors() {
        match candidate.canonicalize() {
            Ok(existing) => {
                for parent in existing.ancestors() {
                    check_directory(parent)?;
                }
                return Ok(());
            }
            Err(e) if e.kind() == ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

fn absolute_home(home: &Path) -> std::io::Result<PathBuf> {
    if home.as_os_str().is_empty() {
        std::env::current_dir()
    } else {
        std::path::absolute(home)
    }
}

/// Only claim a directory selected as the gateway runtime. An explicit config
/// or SQLite filename in a shared parent does not claim that entire parent.
pub(super) fn claim_if_runtime(home: &Path) -> Result<()> {
    check(home)?;
    let Ok(runtime) = super::runtime_home() else {
        return Ok(());
    };
    let home = absolute_home(home)?;
    if absolute_home(&runtime)? != home
        && !(home.exists() && runtime.exists() && home.canonicalize()? == runtime.canonicalize()?)
    {
        return Ok(());
    }
    let marker = home.join(".bitrouter-home-role");
    if present(&marker)? {
        return check(&home);
    }
    let mut directories = fs::DirBuilder::new();
    directories.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut directories, 0o700);
    directories.create(&home)?;
    let mut pending = tempfile::NamedTempFile::new_in(&home)?;
    pending.write_all(b"bitrouter\n")?;
    pending.as_file().sync_all()?;
    match pending.persist_noclobber(marker) {
        Ok(_) => {
            #[cfg(unix)]
            fs::File::open(&home)?.sync_all()?;
        }
        Err(e) if e.error.kind() == ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.error.into()),
    }
    check(&home)
}
