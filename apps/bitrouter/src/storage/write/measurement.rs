//! Small test-only file observations; each caller retains its real fixture/SQL owner.
use anyhow::{Context, Result, ensure};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use std::{os::unix::fs::MetadataExt, path::Path};

use crate::tenancy::storage::{Budget, estimate};

pub(crate) type Files = ([u64; 4], [u64; 4]);

#[derive(Debug)]
pub(crate) struct Sample {
    pub pages: u64,
    pub free_pages: u64,
    pub page_size: u64,
    pub lengths: [u64; 4],
    pub allocated: [u64; 4],
}

pub(crate) fn reservation(budget: &Budget) -> Result<u64> {
    Ok(budget
        .calibration_observation(None)
        .map_err(|e| anyhow::anyhow!("physical observation: {e:?}"))?
        .physical_write_reserved)
}

/// Safe while a COMMIT gate owns the sole SQL connection: filesystem reads only.
pub(crate) fn files(path: &Path) -> Result<Files> {
    let mut lengths = [0; 4];
    let mut allocated = [0; 4];
    for (index, suffix) in ["", "-wal", "-shm", "-journal"].into_iter().enumerate() {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        match std::fs::metadata(name) {
            Ok(metadata) => {
                lengths[index] = metadata.len();
                allocated[index] = metadata.blocks().checked_mul(512).context("block bytes")?;
            }
            Err(error) if index > 0 && error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    ensure!(
        lengths.iter().sum::<u64>() <= 512 * 1024 * 1024
            && allocated.iter().sum::<u64>() <= 512 * 1024 * 1024,
        "calibration scratch limit exceeded"
    );
    Ok((lengths, allocated))
}

fn sql(text: impl Into<String>) -> Statement {
    Statement::from_string(DatabaseBackend::Sqlite, text.into())
}

pub(crate) async fn sample(db: &impl ConnectionTrait, path: &Path) -> Result<Sample> {
    let row = db.query_one(sql("SELECT page_count AS pages, freelist_count AS free, page_size AS size FROM pragma_page_count(), pragma_freelist_count(), pragma_page_size()"))
        .await?.context("page observation")?;
    let (lengths, allocated) = files(path)?;
    let result = Sample {
        pages: u64::try_from(row.try_get::<i64>("", "pages")?)?,
        free_pages: u64::try_from(row.try_get::<i64>("", "free")?)?,
        page_size: u64::try_from(row.try_get::<i64>("", "size")?)?,
        lengths,
        allocated,
    };
    ensure!(result.page_size == 4096 && result.pages <= 16384 && result.free_pages <= result.pages);
    Ok(result)
}

pub(crate) async fn checkpoint(db: &DatabaseConnection, mode: &str) -> Result<(i64, i64, i64)> {
    ensure!(matches!(mode, "PASSIVE" | "TRUNCATE"));
    let row = db
        .query_one(sql(format!("PRAGMA main.wal_checkpoint({mode})")))
        .await?
        .context("checkpoint result")?;
    Ok((
        row.try_get_by_index(0)?,
        row.try_get_by_index(1)?,
        row.try_get_by_index(2)?,
    ))
}

/// Called only after fixture setup/owners settle, never while a transaction is held.
pub(crate) async fn wal_baseline(db: &DatabaseConnection, path: &Path) -> Result<Sample> {
    let mode: String = db
        .query_one(sql("PRAGMA journal_mode=WAL"))
        .await?
        .context("journal mode")?
        .try_get_by_index(0)?;
    ensure!(mode == "wal");
    db.execute_unprepared("PRAGMA wal_autocheckpoint=0; PRAGMA max_page_count=4096;")
        .await?;
    ensure!(checkpoint(db, "TRUNCATE").await? == (0, 0, 0));
    let version: String = db
        .query_one(sql("SELECT sqlite_version() AS version"))
        .await?
        .context("SQLite version")?
        .try_get("", "version")?;
    let metadata = std::fs::metadata(path)?;
    let result = sample(db, path).await?;
    ensure!(result.pages <= 4096);
    eprintln!(
        "caller calibration sqlite={version} journal={mode} os={} arch={} device={} io_block={} baseline={result:?}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        metadata.dev(),
        metadata.blksize()
    );
    Ok(result)
}

/// Infer the isolated caller E from its actual charge and round-trip production
/// estimate(E). This observes what was reserved, not net payload/file growth.
pub(crate) async fn report(
    db: &DatabaseConnection,
    path: &Path,
    label: &str,
    before: &Sample,
    pending: Files,
    charged: u64,
) -> Result<u64> {
    let base = estimate(0).map_err(super::capacity_error)?;
    let step = estimate(1)
        .map_err(super::capacity_error)?
        .checked_sub(base)
        .context("estimate step")?;
    let encoded = charged
        .checked_sub(base)
        .context("missing isolated charge")?
        .checked_div(step)
        .context("zero estimate step")?;
    ensure!(estimate(encoded).map_err(super::capacity_error)? == charged);
    let settled = sample(db, path).await?;
    let (busy, frames, completed) = checkpoint(db, "PASSIVE").await?;
    ensure!(
        busy == 0 && frames >= 0 && completed == frames,
        "incomplete checkpoint {busy}/{frames}/{completed}"
    );
    let after = sample(db, path).await?;
    let growth = |old: [u64; 4], first: [u64; 4], middle: [u64; 4], last: [u64; 4]| -> u64 {
        (0..4)
            .map(|i| first[i].max(middle[i]).max(last[i]).saturating_sub(old[i]))
            .sum()
    };
    let lengths = growth(before.lengths, pending.0, settled.lengths, after.lengths);
    let allocated = growth(
        before.allocated,
        pending.1,
        settled.allocated,
        after.allocated,
    );
    let frame_bytes = u64::try_from(frames)?
        .checked_mul(after.page_size + 24)
        .and_then(|n| n.checked_add(32))
        .and_then(|n| n.checked_add(after.lengths[0].saturating_sub(before.lengths[0])))
        .context("frame bytes")?;
    eprintln!(
        "caller calibration case={label} E={encoded} estimate={charged} lengths={lengths} allocated={allocated} frames_and_main={frame_bytes} before={before:?} pending={pending:?} settled={settled:?} checkpointed={after:?}"
    );
    ensure!(
        lengths.max(allocated).max(frame_bytes) <= charged,
        "caller estimate underestimated sampled writes: {label}"
    );
    Ok(encoded)
}
