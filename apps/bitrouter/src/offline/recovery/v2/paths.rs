//! Read-only path qualification before scratch or the canonical lock is created.
use super::{format::Plan, retirement};
use crate::{
    offline::{files, large::Options},
    tenancy::ownership,
};
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

// At most 64 simultaneous path copies, each subject to the existing 4096-byte
// individual path contract. This is transient accounting, not a record limit.
pub(super) const WORKING_BYTES: usize = 64 * 4096;

pub(super) struct Paths {
    pub target: PathBuf,
    pub source: PathBuf,
    pub output: PathBuf,
    pub forbidden: Vec<PathBuf>,
}

pub(super) fn qualify(
    path: &Path,
    plan: &Plan,
    output: &Path,
    options: &Options,
    remaining: usize,
    cancel: &CancellationToken,
) -> Result<Paths> {
    super::super::plan::absolute(output)?;
    super::super::plan::absolute(&options.scratch_directory)?;
    let record_bytes = remaining
        .checked_sub(WORKING_BYTES)
        .ok_or(super::streams::WorkingCapacity)?;
    files::private_directory(&plan.source.bundle)?;
    files::private_directory(&options.scratch_directory)?;
    let source = plan.source.bundle.canonicalize()?;
    let target = ownership::resolve_path(&plan.target.database, false)?;
    let scratch = options.scratch_directory.canonicalize()?;
    let parent = output.parent().context("report parent")?.canonicalize()?;
    files::private_directory(&parent)?;
    let output = parent.join(output.file_name().context("report filename")?);
    match output.symlink_metadata() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
        Ok(_) => anyhow::bail!("report destination already exists"),
    }
    let mut name = std::ffi::OsString::from(".");
    name.push(target.file_name().context("target filename")?);
    name.push(".tenancy-owner.lock");
    let lock = target.with_file_name(name);
    // Resolve an existing alias too: ownership's open follows the lock path.
    let lock_alias = match lock.symlink_metadata() {
        Ok(_) => Some(lock.canonicalize()?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    ensure!(
        !scratch.starts_with(&source)
            && !output.starts_with(&source)
            && !target.starts_with(&source)
            && !lock.starts_with(&source)
            && !target.starts_with(&output)
            && lock_alias
                .as_ref()
                .is_none_or(|path| !path.starts_with(&source) && path != &target),
        "recovery output/scratch/lock overlaps immutable input"
    );
    let mut forbidden = vec![target.clone(), source.clone(), output.clone(), lock.clone()];
    if let Some(alias) = lock_alias {
        forbidden.push(alias);
    }
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = target.as_os_str().to_os_string();
        name.push(suffix);
        forbidden.push(PathBuf::from(name));
    }
    for input in [
        path,
        &plan.target.provisioning.path,
        &plan.target.static_manifest.path,
        &plan.retirement.path,
        &plan.source.producer.evidence.path,
    ] {
        let input = input.canonicalize()?;
        super::super::plan::absolute(&input)?;
        ensure!(
            !forbidden.iter().any(|other| input.starts_with(other)),
            "recovery input aliases SQL/output/ownership lock or another input"
        );
        // No opaque input descriptor survives into a later SQL phase.
        drop(files::private_file(&input, u64::MAX)?);
        forbidden.push(input);
    }
    for path in [&target, &source, &output, &scratch, &lock] {
        super::super::plan::absolute(path)?;
    }
    retirement::preflight_paths(
        &plan.retirement,
        &plan.source,
        &forbidden,
        record_bytes,
        cancel,
    )?;
    Ok(Paths {
        target,
        source,
        output,
        forbidden,
    })
}
