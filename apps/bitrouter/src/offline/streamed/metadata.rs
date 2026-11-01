//! Evidence metadata uses its actual bounded JSON shape, not a history quota.
use crate::offline::{
    files,
    format::{Evidence, FORMAT, FORMAT_V2, MAX_BYTES, MAX_EVIDENCE, MAX_ROWS},
    large::{CapacityExceeded, Charge, Workspace},
    recovery::v2::memory::json_required,
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::Path,
};

struct Count {
    bytes: usize,
    limit: usize,
}
impl Write for Count {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .filter(|bytes| *bytes <= self.limit)
            .ok_or_else(|| std::io::Error::other(CapacityExceeded))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn encode(evidence: &Evidence, workspace: &Workspace) -> Result<(Vec<u8>, Charge)> {
    workspace.check()?;
    let mut count = Count {
        bytes: 0,
        limit: workspace.available() / 2,
    };
    serde_json::to_writer_pretty(&mut count, evidence)?;
    if evidence.format == FORMAT {
        ensure!(
            count.bytes as u64 <= MAX_EVIDENCE,
            "v1 evidence exceeds profile"
        );
    } else {
        ensure!(evidence.format == FORMAT_V2, "unsupported evidence format");
    }
    let charge = workspace.reserve(
        count
            .bytes
            .checked_mul(2)
            .context("evidence output size overflow")?,
    )?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(count.bytes)
        .map_err(|_| CapacityExceeded)?;
    serde_json::to_writer_pretty(&mut bytes, evidence)?;
    ensure!(
        bytes.len() == count.bytes
            && bytes.capacity()
                <= count
                    .bytes
                    .checked_mul(2)
                    .context("evidence capacity overflow")?,
        "evidence output changed"
    );
    Ok((bytes, charge))
}

#[derive(Deserialize)]
struct Version {
    format: String,
}

pub(super) fn read(path: &Path, workspace: &Workspace) -> Result<(Evidence, String, Charge)> {
    workspace.check()?;
    let mut file = files::private_file(path, u64::MAX)?;
    let length = usize::try_from(file.metadata()?.len())?;
    // Reserve raw bytes and the JSON decoder's escaped-string scratch before
    // either allocation. The shape pass borrows strings and never owns arrays.
    let initial = length
        .checked_mul(6)
        .and_then(|bytes| bytes.checked_add(8192))
        .ok_or(CapacityExceeded)?;
    let mut charge = workspace.reserve(initial)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| CapacityExceeded)?;
    ensure!(bytes.capacity() <= initial, CapacityExceeded);
    bytes.resize(length, 0);
    file.read_exact(&mut bytes)?;
    let mut extra = [0_u8; 1];
    ensure!(file.read(&mut extra)? == 0, "evidence grew during read");
    drop(file);
    workspace.check()?;
    let required = json_required(&bytes)?
        .checked_add(8192)
        .ok_or(CapacityExceeded)?;
    charge.grow(required.saturating_sub(initial))?;
    let version: Version = serde_json::from_slice(&bytes)?;
    if version.format == FORMAT {
        ensure!(length as u64 <= MAX_EVIDENCE, "v1 evidence exceeds profile");
    } else {
        ensure!(version.format == FORMAT_V2, "unsupported evidence format");
    }
    let evidence: Evidence = serde_json::from_slice(&bytes)?;
    if evidence.format == FORMAT {
        evidence.validate_profile()?;
        ensure!(
            evidence.inventory.rows <= MAX_ROWS && evidence.inventory.value_bytes <= MAX_BYTES,
            "v1 inventory exceeds profile"
        );
    } else {
        evidence.validate_large_profile()?;
    }
    let digest = files::sha(&bytes);
    Ok((evidence, digest, charge))
}

pub(super) fn digest(path: &Path, workspace: &Workspace) -> Result<String> {
    let _charge = workspace.reserve(8192)?;
    let mut file = files::private_file(path, u64::MAX)?;
    let expected = file.metadata()?.len();
    let mut actual = 0_u64;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        workspace.check()?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        actual = actual
            .checked_add(count as u64)
            .context("evidence size overflow")?;
        ensure!(actual <= expected, "evidence grew during verification");
        hash.update(&buffer[..count]);
    }
    ensure!(actual == expected, "evidence shrank during verification");
    Ok(hex::encode(hash.finalize()))
}

#[cfg(all(test, unix))]
mod tests;
