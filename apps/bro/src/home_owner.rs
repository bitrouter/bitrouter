//! Claim the checkpoint home before creating its first thread or lock.
use anyhow::{Result, ensure};
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
        "gateway home or invalid product home marker cannot be used as BRO home"
    );
    Ok(true)
}

pub(crate) fn check(root: &Path) -> Result<()> {
    claimed(root).map(|_| ())
}

pub(crate) fn claim(root: &Path) -> Result<()> {
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
