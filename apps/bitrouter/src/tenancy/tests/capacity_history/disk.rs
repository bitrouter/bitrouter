use super::*;
use std::{os::unix::fs::MetadataExt, path::Path};

pub(super) fn require_free(path: &Path, minimum: u64) -> Result<()> {
    let stat = rustix::fs::statvfs(path)?;
    let free = stat
        .f_bavail
        .checked_mul(stat.f_frsize)
        .context("free space overflow")?;
    ensure!(
        free >= minimum,
        "history fixture needs {minimum} free bytes; found {free}"
    );
    Ok(())
}

#[derive(Debug, Clone)]
pub(super) struct Observation {
    pub pages: u64,
    pub free: u64,
    pub page_size: u64,
    pub lengths: [u64; 3],
    pub allocated: [u64; 3],
}

pub(super) async fn checkpoint(h: &Harness) -> Result<Observation> {
    // The drained fixture owns one connection. Check ACK, not just file length.
    // https://www.sqlite.org/pragma.html#pragma_wal_checkpoint
    let row = h
        .fixture
        .db
        .query_one(sql("PRAGMA wal_checkpoint(PASSIVE)"))
        .await?
        .context("checkpoint")?;
    let busy: i64 = row.try_get_by_index(0)?;
    let frames: i64 = row.try_get_by_index(1)?;
    let done: i64 = row.try_get_by_index(2)?;
    ensure!(
        busy == 0 && frames >= 0 && done == frames,
        "incomplete checkpoint {busy}/{frames}/{done}"
    );
    observe(h).await
}

pub(super) async fn observe(h: &Harness) -> Result<Observation> {
    let row = h.fixture.db.query_one(sql("SELECT page_count AS pages,freelist_count AS free,page_size AS size FROM pragma_page_count(),pragma_freelist_count(),pragma_page_size()"))
        .await?.context("page counts")?;
    let mut value = Observation {
        pages: u64::try_from(row.try_get::<i64>("", "pages")?)?,
        free: u64::try_from(row.try_get::<i64>("", "free")?)?,
        page_size: u64::try_from(row.try_get::<i64>("", "size")?)?,
        lengths: [0; 3],
        allocated: [0; 3],
    };
    for (index, suffix) in ["", "-wal", "-shm"].into_iter().enumerate() {
        let mut path = h.path.as_os_str().to_os_string();
        path.push(suffix);
        match std::fs::metadata(path) {
            Ok(metadata) => {
                value.lengths[index] = metadata.len();
                value.allocated[index] = metadata
                    .blocks()
                    .checked_mul(512)
                    .context("allocated bytes")?;
            }
            Err(error) if index > 0 && error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    ensure!(value.page_size > 0 && value.free <= value.pages);
    Ok(value)
}

pub(super) fn reused(before: &Observation, inserted: &Observation, bytes: u64) -> Result<()> {
    // No VACUUM/shrink claim: later real payload writes consume released pages.
    // https://www.sqlite.org/fileformat.html#the_freelist
    ensure!(
        before.free > inserted.free,
        "no freelist consumption: {before:?} -> {inserted:?}"
    );
    let allowance = bytes / 4 + 256 * 1024;
    ensure!(
        inserted.pages.saturating_sub(before.pages) * inserted.page_size < allowance,
        "later batch grew like cold storage: {before:?} -> {inserted:?}"
    );
    ensure!(inserted.lengths[0].saturating_sub(before.lengths[0]) < allowance);
    ensure!(inserted.allocated[0].saturating_sub(before.allocated[0]) < allowance);
    Ok(())
}
