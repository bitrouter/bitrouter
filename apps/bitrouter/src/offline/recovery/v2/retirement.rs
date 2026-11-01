//! Complete ordered retirement evidence and bounded opaque copies.
use super::{
    format::{self, RetirementRecord},
    inputs::Prepared,
    streams::Retirement,
};
use crate::offline::{
    files,
    large::{self, Workspace, sort::Records},
    recovery::plan::Source,
};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
};

/// Scan every selected path before any write, including ownership-lock opens.
/// One bounded line is reused; the complete anchored stream is never retained.
pub(super) fn preflight_paths(
    anchor: &super::super::plan::Anchor,
    source: &Source,
    forbidden: &[PathBuf],
    bytes: usize,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    let capacity = bytes
        .checked_sub(8192)
        .ok_or(super::streams::WorkingCapacity)?
        / 16;
    ensure!(
        capacity > 0,
        "retirement preflight working capacity exceeded"
    );
    let mut input = BufReader::with_capacity(4096, files::private_file(&anchor.path, u64::MAX)?);
    let mut line = Vec::new();
    line.try_reserve_exact(capacity.checked_add(1).context("line capacity overflow")?)
        .map_err(|_| super::streams::WorkingCapacity)?;
    let mut digest = Sha256::new();
    let mut header = false;
    loop {
        ensure!(
            !cancel.is_cancelled(),
            "recovery path qualification cancelled"
        );
        line.clear();
        input
            .by_ref()
            .take(
                u64::try_from(capacity)?
                    .checked_add(1)
                    .context("line capacity overflow")?,
            )
            .read_until(b'\n', &mut line)?;
        if line.is_empty() {
            break;
        }
        if line.len() > capacity {
            return Err(super::streams::WorkingCapacity.into());
        }
        ensure!(
            line.len() > 1 && line.last() == Some(&b'\n'),
            "invalid retirement JSON-lines boundary"
        );
        let required = super::memory::json_required(&line)?
            .checked_add(line.capacity())
            .and_then(|n| n.checked_add(8192))
            .context("retirement working size overflow")?;
        if required > bytes {
            return Err(super::streams::WorkingCapacity.into());
        }
        digest.update(&line);
        if !header {
            serde_json::from_slice::<format::RetirementHeader>(&line)?
                .validate(&source.installation_id, &source.evidence_digest)?;
            header = true;
        } else if let RetirementRecord::File { path, .. } = serde_json::from_slice(&line)? {
            super::super::plan::absolute(&path)?;
            let canonical = path.canonicalize()?;
            ensure!(
                path == canonical && !forbidden.iter().any(|other| canonical.starts_with(other)),
                "selected file overlaps SQL/evidence/output/ownership lock"
            );
            drop(files::private_file(&canonical, u64::MAX)?);
        }
    }
    ensure!(
        header && hex::encode(digest.finalize()) == anchor.sha256,
        "retirement preflight anchor mismatch"
    );
    Ok(())
}

pub(super) fn inspect(
    input: &mut Prepared,
    source: &Source,
    output: &Path,
    forbidden: &[PathBuf],
    key: &[u8; 32],
    workspace: &Workspace,
    copy: bool,
) -> Result<u64> {
    let check = || workspace.check();
    let mut stream = Retirement::new(
        input.reader(workspace)?,
        &source.installation_id,
        &source.evidence_digest,
        &check,
    )?;
    let mut writers = Records::<256>::new(workspace, workspace.available().min(1024 * 1024))?;
    let mut selected_writers =
        Records::<256>::new(workspace, workspace.available().min(1024 * 1024))?;
    let mut count = 0_u64;
    while let Some(record) = stream.next(&check)? {
        match record.value {
            RetirementRecord::Writer { id, .. } => writers.push(format::padded::<256>(&id)?)?,
            RetirementRecord::File {
                path,
                writer,
                keyed_digest,
                bytes,
            } => {
                ensure!(
                    !forbidden
                        .iter()
                        .any(|other| path == *other || path.starts_with(other)),
                    "selected file overlaps SQL/evidence input"
                );
                ensure!(
                    fingerprint(&path, key, workspace)? == (bytes, keyed_digest.clone()),
                    "selected file content changed"
                );
                let destination = output.join(format!("selected-{count}"));
                if copy {
                    ensure!(
                        large::copy_new(&path, &destination, workspace)? == bytes,
                        "selected copy length changed"
                    );
                }
                ensure!(
                    fingerprint(&destination, key, workspace)? == (bytes, keyed_digest),
                    "selected copy changed"
                );
                selected_writers.push(format::padded::<256>(&writer)?)?;
                count = count
                    .checked_add(1)
                    .context("selected file count overflow")?;
            }
        }
    }
    drop(stream);
    let mut writers = writers.finish()?;
    let mut selected_writers = selected_writers.finish()?;
    let mut current = writers.next()?;
    let mut previous = None;
    while let Some(selected) = selected_writers.next()? {
        if previous == Some(selected) {
            continue;
        }
        while current.is_some_and(|writer| writer < selected) {
            current = writers.next()?;
        }
        ensure!(
            current == Some(selected),
            "selected file writer missing from retirement evidence"
        );
        previous = Some(selected);
    }
    Ok(count)
}

fn fingerprint(path: &Path, key: &[u8; 32], workspace: &Workspace) -> Result<(u64, String)> {
    let _charge = workspace.reserve(4096)?;
    let mut file = files::private_file(path, u64::MAX)?;
    let expected = file.metadata()?.len();
    let mut digest = files::Fingerprint::new(key, b"recovery-selected-file.v1")?;
    // This is one length-framed field, regardless of OS read chunk boundaries.
    digest.update(&expected.to_be_bytes());
    let mut buffer = [0; 4096];
    let mut total = 0u64;
    loop {
        workspace.check()?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(u64::try_from(count)?)
            .context("selected bytes overflow")?;
        ensure!(total <= expected, "selected file grew");
        digest.update(&buffer[..count]);
    }
    ensure!(
        total == expected && file.metadata()?.len() == expected,
        "selected file changed"
    );
    Ok((total, hex::encode(digest.finish())))
}
