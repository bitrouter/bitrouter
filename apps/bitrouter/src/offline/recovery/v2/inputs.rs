//! Anchored file descriptors established before any target SQLite descriptor.

use super::{format, streams::JsonLines};
use crate::offline::{
    files,
    large::Workspace,
    recovery::plan::{self, Anchor},
};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

pub(crate) fn read_control<T: serde::de::DeserializeOwned>(
    path: &Path,
    workspace: &Workspace,
) -> Result<(T, Vec<u8>, crate::offline::large::Charge)> {
    plan::absolute(path)?;
    let mut file = files::private_file(path, u64::MAX)?;
    let size = usize::try_from(file.metadata()?.len())?;
    let capacity = size.checked_add(1).context("control length overflow")?;
    let initial = control_initial(capacity)?;
    let mut charge = workspace.reserve(initial)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| super::streams::WorkingCapacity)?;
    file.by_ref()
        .take(u64::try_from(capacity)?)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() == size && file.metadata()?.len() == u64::try_from(size)?,
        "control input changed"
    );
    let required = super::memory::json_required(&bytes)?.max(initial);
    charge.grow(required - initial)?;
    let value = serde_json::from_slice(&bytes)?;
    workspace.check()?;
    Ok((value, bytes, charge))
}

/// Bootstrap without creating scratch or ownership files. The returned byte
/// allowance transfers to the command workspace before any later allocation.
pub(crate) fn read_control_before_workspace<T: serde::de::DeserializeOwned>(
    path: &Path,
    limit: usize,
) -> Result<(T, Vec<u8>, usize)> {
    plan::absolute(path)?;
    ensure!(
        limit > 0 && limit <= isize::MAX as usize,
        "invalid control allowance"
    );
    let mut file = files::private_file(path, u64::MAX)?;
    let size = usize::try_from(file.metadata()?.len())?;
    let capacity = size.checked_add(1).context("control length overflow")?;
    let initial = control_initial(capacity)?;
    if initial > limit {
        return Err(super::streams::WorkingCapacity.into());
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(capacity)
        .map_err(|_| super::streams::WorkingCapacity)?;
    file.by_ref()
        .take(u64::try_from(capacity)?)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() == size && file.metadata()?.len() == u64::try_from(size)?,
        "control input changed"
    );
    let required = super::memory::json_required(&bytes)?.max(initial);
    if required > limit {
        return Err(super::streams::WorkingCapacity.into());
    }
    Ok((serde_json::from_slice(&bytes)?, bytes, required))
}

fn control_initial(capacity: usize) -> Result<usize> {
    capacity
        .checked_mul(6)
        .and_then(|n| n.checked_add(8192))
        .context("control working size overflow")
}

pub(crate) fn metadata_charge(
    value: &impl serde::Serialize,
    workspace: &Workspace,
) -> Result<crate::offline::large::Charge> {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .ok_or_else(|| std::io::Error::other("metadata size overflow"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer(&mut count, value)?;
    workspace.reserve(
        count
            .0
            .checked_mul(16)
            .and_then(|n| n.checked_add(8192))
            .context("metadata working size overflow")?,
    )
}

pub(crate) struct Prepared {
    path: PathBuf,
    digest: String,
    bytes: u64,
    file: File,
    _charge: crate::offline::large::Charge,
}

impl Prepared {
    /// Establish all inputs before SQL. `forbidden` contains canonical source
    /// bundle, target file/sidecars and output directory; pass it for each input.
    pub(crate) fn open(
        anchor: &Anchor,
        forbidden: &[PathBuf],
        workspace: &Workspace,
    ) -> Result<Self> {
        workspace.check()?;
        plan::absolute(&anchor.path)?;
        format::digest(&anchor.sha256)?;
        let path = anchor.path.canonicalize()?;
        ensure!(
            !forbidden
                .iter()
                .any(|other| path == *other || path.starts_with(other)),
            "input overlaps SQL or evidence output"
        );
        let charge = workspace.reserve(
            path.as_os_str()
                .len()
                .checked_mul(4)
                .and_then(|n| n.checked_add(4096))
                .context("input metadata overflow")?,
        )?;
        let mut file = files::private_file(&anchor.path, u64::MAX)?;
        let (bytes, digest) = fingerprint(&mut file, workspace)?;
        ensure!(digest == anchor.sha256, "stream evidence anchor mismatch");
        file.seek(SeekFrom::Start(0))?;
        Ok(Self {
            path,
            digest,
            bytes,
            file,
            _charge: charge,
        })
    }

    /// Duplicates the already qualified non-SQL descriptor, never opens an
    /// arbitrary path while SQLite's POSIX inode locks are held.
    pub(crate) fn reader<'a>(&mut self, workspace: &'a Workspace) -> Result<JsonLines<'a>> {
        workspace.check()?;
        self.file.seek(SeekFrom::Start(0))?;
        JsonLines::new(self.file.try_clone()?, workspace)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn verify_copy(&self, path: &Path, workspace: &Workspace) -> Result<()> {
        let mut file = files::private_file(path, u64::MAX)?;
        ensure!(
            fingerprint(&mut file, workspace)? == (self.bytes, self.digest.clone()),
            "published stream copy differs from its anchor"
        );
        Ok(())
    }

    /// Call only after actual SQL close. Recheck retained descriptor and current
    /// path contents; no changed or substituted input may publish a report.
    pub(crate) fn recheck(&mut self, workspace: &Workspace) -> Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        ensure!(
            fingerprint(&mut self.file, workspace)? == (self.bytes, self.digest.clone()),
            "retained stream changed"
        );
        let mut current = files::private_file(&self.path, u64::MAX)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let before = self.file.metadata()?;
            let after = current.metadata()?;
            ensure!(
                before.dev() == after.dev() && before.ino() == after.ino(),
                "stream inode changed"
            );
        }
        ensure!(
            fingerprint(&mut current, workspace)? == (self.bytes, self.digest.clone()),
            "stream path changed"
        );
        Ok(())
    }
}

fn fingerprint(file: &mut File, workspace: &Workspace) -> Result<(u64, String)> {
    let _charge = workspace.reserve(4096)?;
    let expected = file.metadata()?.len();
    let mut digest = Sha256::new();
    let mut buffer = [0; 4096];
    let mut bytes = 0_u64;
    loop {
        workspace.check()?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(u64::try_from(count)?)
            .context("stream length overflow")?;
        ensure!(bytes <= expected, "stream grew during inspection");
        digest.update(&buffer[..count]);
    }
    ensure!(
        bytes == expected && file.metadata()?.len() == expected,
        "stream changed during inspection"
    );
    Ok((bytes, hex::encode(digest.finalize())))
}
