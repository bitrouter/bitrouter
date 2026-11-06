use super::{CapacityExceeded, Workspace};
use crate::offline::files::{self, Fingerprint};
use anyhow::{Context, Result, ensure};
use std::{
    fs::{File, Metadata, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

/// Metadata witness only: no database descriptor can interfere with SQLite locks.
pub(crate) struct Source {
    path: PathBuf,
    metadata: Metadata,
}
impl Source {
    pub(crate) fn inspect(path: &Path) -> Result<Self> {
        ensure!(path.is_absolute(), "absolute source path required");
        ensure!(cfg!(unix), "offline source qualification requires Unix");
        let metadata = metadata(path)?;
        sidecars(path)?;
        Ok(Self {
            path: path.into(),
            metadata,
        })
    }

    pub(crate) fn check(&self) -> Result<()> {
        let current = metadata(&self.path)?;
        ensure!(same(&self.metadata, &current), "offline source changed");
        sidecars(&self.path)
    }
}

fn metadata(path: &Path) -> Result<Metadata> {
    let value = std::fs::symlink_metadata(path)?;
    ensure!(
        value.is_file() && !value.file_type().is_symlink(),
        "invalid offline source"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        ensure!(
            value.nlink() == 1 && value.permissions().mode() & 0o077 == 0,
            "offline source is not private and single-link"
        );
    }
    Ok(value)
}

fn same(left: &Metadata, right: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        left.dev() == right.dev()
            && left.ino() == right.ino()
            && left.len() == right.len()
            && left.mode() == right.mode()
            && left.mtime() == right.mtime()
            && left.mtime_nsec() == right.mtime_nsec()
            && left.ctime() == right.ctime()
            && left.ctime_nsec() == right.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        let _ = (left, right);
        false
    }
}

fn sidecars(path: &Path) -> Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        match std::fs::symlink_metadata(Path::new(&name)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
            Ok(_) => anyhow::bail!("offline source has SQLite sidecars"),
        }
    }
    Ok(())
}

pub(super) fn create_new(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

const BUFFER: usize = 64 * 1024;

pub(super) fn copy_new(source: &Path, destination: &Path, workspace: &Workspace) -> Result<u64> {
    // Caller owns incomplete-destination cleanup, including every failure here.
    // Selected opaque files may have adjacent names ending in SQLite suffixes.
    // Database callers separately enforce Source qualification and snapshot hashing.
    let _charge = workspace.reserve(
        BUFFER
            .checked_add(source.as_os_str().len())
            .and_then(|bytes| bytes.checked_add(destination.as_os_str().len()))
            .and_then(|bytes| bytes.checked_add(4096))
            .ok_or(CapacityExceeded)?,
    )?;
    ensure!(source.is_absolute(), "absolute source path required");
    ensure!(cfg!(unix), "offline source qualification requires Unix");
    let witness = metadata(source)?;
    let parent = destination.parent().context("destination parent missing")?;
    files::private_directory(parent)?;
    workspace.probe(parent, witness.len())?;
    let mut input = files::private_file(source, u64::MAX)?;
    ensure!(same(&witness, &input.metadata()?), "offline source changed");
    let mut output = create_new(destination)?;
    let mut buffer = [0_u8; BUFFER];
    let mut total = 0_u64;
    loop {
        workspace.check()?;
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .context("offline file size overflow")?;
        ensure!(total <= witness.len(), "offline source grew");
        workspace.probe(parent, count as u64)?;
        output.write_all(&buffer[..count])?;
    }
    ensure!(total == witness.len(), "offline source shrank");
    output.sync_all()?;
    drop(output);
    drop(input);
    ensure!(same(&witness, &metadata(source)?), "offline source changed");
    files::sync_directory(parent)?;
    workspace.probe(parent, 0)?;
    Ok(total)
}

pub(super) fn snapshot_digest(
    path: &Path,
    key: &[u8; 32],
    workspace: &Workspace,
) -> Result<(u64, String)> {
    let _charge = workspace.reserve(
        BUFFER
            .checked_add(path.as_os_str().len())
            .and_then(|bytes| bytes.checked_add(4096))
            .ok_or(CapacityExceeded)?,
    )?;
    let witness = Source::inspect(path)?;
    let mut file = files::private_file(path, u64::MAX)?;
    ensure!(
        same(&witness.metadata, &file.metadata()?),
        "offline source changed"
    );
    let length = witness.metadata.len();
    let mut digest = Fingerprint::new(key, b"snapshot-file")?;
    digest.field(&length.to_be_bytes());
    let mut buffer = [0_u8; BUFFER];
    let mut total = 0_u64;
    loop {
        workspace.check()?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .context("offline file size overflow")?;
        ensure!(total <= length, "offline source grew");
        digest.update(&buffer[..count]);
    }
    ensure!(total == length, "offline source shrank");
    drop(file);
    witness.check()?;
    Ok((length, hex::encode(digest.finish())))
}
