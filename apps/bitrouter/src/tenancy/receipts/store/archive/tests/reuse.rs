//! File-backed reuse and sampled allocation calibration, not an ENOSPC guarantee.
use super::{NO_CHARGE, candidate, commit, lookup_candidate};
use crate::tenancy::receipts::{
    store::{
        Candidate, Disposition,
        archive::{Prepared, archive_one, sqlite},
        same_completion,
    },
    usage_evidence::Snapshot,
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    execution::{Attempt, Outcome, Provenance, ReceiptState, Target, Usage},
    operation::DispatchState,
    usage_evidence::{Protocol, Source},
};
use sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseConnection, TransactionTrait};
use std::{os::unix::fs::MetadataExt, path::Path};

const BATCH: usize = 12;
const CYCLES: usize = 3;

fn sizable(id: &str) -> Result<Candidate> {
    let mut receipt = candidate("installation", "tenant", id, "creator")?.receipt;
    let usage = |n| Usage {
        input_tokens: Some(n),
        output_tokens: Some(n),
        cache_read_tokens: Some(n),
        cache_write_tokens: Some(n),
        reasoning_tokens: Some(n),
        provenance: Provenance::Provider,
        complete: true,
    };
    receipt.state = ReceiptState::Committed;
    receipt.outcome = Outcome::Completed;
    receipt.usage = usage(8);
    let mut evidence = Snapshot::default();
    for index in 0..8 {
        let attempt = Attempt {
            attempt_id: format!("attempt-{index}-{}", "a".repeat(112)).try_into()?,
            target: Some(Target {
                provider_id: "p".repeat(120).try_into()?,
                model_id: "m".repeat(256),
                account_id: "a".repeat(120).try_into()?,
                credential_id: "c".repeat(120).try_into()?,
                credential_version: "r".repeat(120).try_into()?,
            }),
            outcome: Outcome::Completed,
            usage: usage(1),
            native_usage: Vec::new(),
        };
        for frame in 0..32 {
            evidence.observe(&attempt.attempt_id, Protocol::ChatCompletions, Source::ChatChunk,
                &serde_json::json!({
                    "prompt_tokens": frame, "completion_tokens": frame, "total_tokens": 2 * frame,
                    "prompt_tokens_details": {"cached_tokens": 1, "audio_tokens": 1, "image_tokens": 1},
                    "completion_tokens_details": {"reasoning_tokens": 1, "audio_tokens": 1, "image_tokens": 1}
                }));
        }
        evidence.terminals.push(attempt.attempt_id.clone());
        receipt.attempts.push(attempt);
    }
    let result = Candidate::new(
        receipt,
        Disposition {
            dispatch: DispatchState::Started,
            statuses: vec![Some(200); 8],
            auxiliary_digest: None,
        },
    )?
    .with_usage_evidence(evidence)?;
    ensure!(
        result.json.len() >= 8 * 1024,
        "fixture needs sizable receipt bytes"
    );
    ensure!(
        result
            .usage_evidence
            .as_ref()
            .context("fixture usage")?
            .json
            .len()
            >= 128 * 1024,
        "fixture needs overflow pages, not just small logical rows"
    );
    Ok(result)
}

#[derive(Clone, Debug)]
struct Observation {
    pages: u64,
    free: u64,
    page_size: u64,
    // Main DB, WAL, SHM, including filesystem allocation rather than only lengths.
    lengths: [u64; 3],
    allocated: [u64; 3],
}

async fn observe(db: &DatabaseConnection, path: &Path) -> Result<Observation> {
    let row = db.query_one(sqlite(
        "SELECT page_count AS pages, freelist_count AS free, page_size AS size FROM pragma_page_count(), pragma_freelist_count(), pragma_page_size()", []
    )).await?.context("SQLite page observation")?;
    let mut result = Observation {
        pages: u64::try_from(row.try_get::<i64>("", "pages")?)?,
        free: u64::try_from(row.try_get::<i64>("", "free")?)?,
        page_size: u64::try_from(row.try_get::<i64>("", "size")?)?,
        lengths: [0; 3],
        allocated: [0; 3],
    };
    for (index, suffix) in ["", "-wal", "-shm"].into_iter().enumerate() {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        match std::fs::metadata(name) {
            Ok(metadata) => {
                result.lengths[index] = metadata.len();
                result.allocated[index] = metadata
                    .blocks()
                    .checked_mul(512)
                    .context("allocated bytes")?;
            }
            Err(error) if index > 0 && error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    ensure!(result.page_size > 0 && result.free <= result.pages);
    Ok(result)
}

async fn checkpoint(db: &DatabaseConnection) -> Result<u64> {
    // PASSIVE does not promise completeness with readers. This fixture owns one
    // connection, has no readers, and checks all three results rather than busy alone.
    // https://www.sqlite.org/pragma.html#pragma_wal_checkpoint
    let rows = db
        .query_all(sqlite("PRAGMA main.wal_checkpoint(PASSIVE)", []))
        .await
        .context("checkpoint committed main-database writes")?;
    ensure!(rows.len() == 1, "expected one checkpoint result");
    let row = rows.first().context("checkpoint result")?;
    let busy: i64 = row.try_get_by_index(0)?;
    let frames: i64 = row.try_get_by_index(1)?;
    let completed: i64 = row.try_get_by_index(2)?;
    ensure!(
        busy == 0 && frames >= 0 && completed == frames,
        "incomplete fixture checkpoint: {busy}/{frames}/{completed}"
    );
    Ok(u64::try_from(frames)?)
}

#[derive(Debug, Default)]
struct Calibration {
    allocated_growth: u64,
    length_growth: u64,
    wal_and_main_growth: u64,
    margin_per_mille: u64,
}

impl Calibration {
    async fn finish(
        &mut self,
        db: &DatabaseConnection,
        path: &Path,
        before: &Observation,
        encoded: u64,
    ) -> Result<Observation> {
        let committed = observe(db, path).await?;
        let frames = checkpoint(db).await?;
        let after = observe(db, path).await?;
        let growth = |old: [u64; 3], first: [u64; 3], last: [u64; 3]| -> u64 {
            (0..3)
                .map(|i| first[i].max(last[i]).saturating_sub(old[i]))
                .sum()
        };
        let allocated = growth(before.allocated, committed.allocated, after.allocated);
        let lengths = growth(before.lengths, committed.lengths, after.lengths);
        // A reused WAL's file length can hide write activity. Include this
        // transaction's checkpointed frames and any main-file extension too.
        // WAL format: https://www.sqlite.org/fileformat.html#wal_file_format
        let wal_and_main = frames * (after.page_size + 24)
            + 32
            + after.lengths[0].saturating_sub(before.lengths[0]);
        // Mirrors the current transaction estimate for this write family. These
        // samples exclude transient peaks, unrelated writers and full-volume faults.
        let margin = encoded
            .checked_mul(8)
            .and_then(|n| n.checked_add(64 * 1024))
            .context("fixture estimate")?;
        let observed = allocated.max(lengths).max(wal_and_main);
        ensure!(
            observed <= margin,
            "fixture exceeds 8x+64KiB estimate: observed={observed}, estimate={margin}, before={before:?}, after={after:?}"
        );
        ensure!(
            observed <= 32 * 1024 * 1024,
            "fixture exceeds sampled 32MiB allowance: {observed}"
        );
        self.allocated_growth = self.allocated_growth.max(allocated);
        self.length_growth = self.length_growth.max(lengths);
        self.wal_and_main_growth = self.wal_and_main_growth.max(wal_and_main);
        self.margin_per_mille = self
            .margin_per_mille
            .max((observed * 1000).div_ceil(margin));
        Ok(after)
    }
}

#[tokio::test]
async fn terminal_archive_pages_are_reused_by_later_receipt_and_usage_writes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("archive-reuse.sqlite");
    let mut options = ConnectOptions::new(format!("sqlite://{}?mode=rwc", path.display()));
    options
        .min_connections(1)
        .max_connections(1)
        .sqlx_logging(false);
    let db = Database::connect(options.clone()).await?;
    db.execute_unprepared("PRAGMA page_size=4096; PRAGMA auto_vacuum=NONE;")
        .await?;
    let mode: String = db
        .query_one(sqlite("PRAGMA journal_mode=WAL", []))
        .await?
        .context("journal mode")?
        .try_get_by_index(0)?;
    ensure!(mode == "wal");
    // Only this fixture disables automatic checkpoints so each write's allocation
    // can be sampled before and after an explicit PASSIVE checkpoint.
    db.execute_unprepared("PRAGMA wal_autocheckpoint=0").await?;
    // Write and commit the schema in WAL mode before its first checkpoint.
    // A checkpoint immediately after switching journal mode can initialize the
    // WAL through SQLite's internal table_list read; checkpointing every schema
    // then also encounters that read's temporary-database transaction.
    // https://sqlite.org/forum/forumpost/fd0f19d229156939
    crate::db::run_migrations(&db).await?;
    checkpoint(&db).await?;
    let version: String = db
        .query_one(sqlite("SELECT sqlite_version() AS version", []))
        .await?
        .context("SQLite version")?
        .try_get("", "version")?;
    let mut previous = observe(&db, &path).await?;
    ensure!(previous.page_size == 4096);
    let metadata = std::fs::metadata(&path)?;
    eprintln!(
        "archive reuse filesystem device={} preferred_io_block={} baseline={previous:?}",
        metadata.dev(),
        metadata.blksize()
    );
    let mut cold_pages = 0;
    let mut cold_allocated = 0;
    let mut writes = Calibration::default();
    let mut archives = Calibration::default();
    for cycle in 0..CYCLES {
        let before = previous.clone();
        for index in 0..BATCH {
            let original = sizable(&format!("execution-{cycle:02}-{index:02}"))?;
            ensure!(commit(&db, &original).await?);
            previous = writes
                .finish(
                    &db,
                    &path,
                    &previous,
                    original.bytes() + original.auxiliary_bytes(),
                )
                .await?;
        }
        let growth = previous.pages.saturating_sub(before.pages);
        if cycle == 0 {
            cold_pages = growth;
            cold_allocated = previous.allocated[0].saturating_sub(before.allocated[0]);
            ensure!(cold_pages * previous.page_size >= 1024 * 1024 && cold_allocated > 0);
        } else {
            // Freed overflow pages must fund real subsequent writes. Neither a
            // logical live-byte debit nor a smaller compressed blob proves this.
            // https://www.sqlite.org/fileformat.html#the_freelist
            ensure!(
                before.free.saturating_sub(previous.free) >= cold_pages / 2,
                "freelist not consumed: before={before:?}, after={previous:?}"
            );
            ensure!(
                growth < cold_pages / 4,
                "later writes grew like cold writes"
            );
            ensure!(
                previous.lengths[0].saturating_sub(before.lengths[0])
                    < cold_pages * previous.page_size / 4,
                "main file did not reuse pages"
            );
            ensure!(
                previous.allocated[0].saturating_sub(before.allocated[0])
                    < cold_allocated / 2 + 64 * 1024,
                "allocated main-file growth did not improve"
            );
        }
        let inline = previous.clone();
        for index in 0..BATCH {
            let original = sizable(&format!("execution-{cycle:02}-{index:02}"))?;
            let prepared = Prepared::new(&original, NO_CHARGE)?;
            ensure!(prepared.stored_bytes() * 4 < prepared.relocated_bytes());
            let tx = db.begin().await?;
            ensure!(archive_one(&tx, &prepared, NO_CHARGE).await?);
            tx.commit().await?;
            previous = archives
                .finish(
                    &db,
                    &path,
                    &previous,
                    u64::try_from(
                        prepared.stored_bytes() + prepared.relocated_bytes() + 16 * 1024,
                    )?,
                )
                .await?;
        }
        ensure!(
            previous.free.saturating_sub(inline.free) >= cold_pages / 2,
            "archive did not return overflow pages: inline={inline:?}, archived={previous:?}"
        );
        eprintln!(
            "archive reuse sqlite={version} os={} mode={mode} cycle={cycle} inline={inline:?} archived={previous:?}",
            std::env::consts::OS
        );
    }
    eprintln!(
        "sampled per-transaction calibration: writes={writes:?}, archives={archives:?}; not a universal physical bound"
    );
    db.close().await?;
    let db = Database::connect(options).await?;
    for cycle in 0..CYCLES {
        for index in 0..BATCH {
            let original = sizable(&format!("execution-{cycle:02}-{index:02}"))?;
            let r = &original.receipt;
            let loaded = lookup_candidate(&db, &r.installation_id, &r.tenant_id, &r.execution_id)
                .await?
                .context("retained archived receipt")?;
            same_completion(&loaded, &original)?;
        }
    }
    let rows: i64 = db
        .query_one(sqlite(
            "SELECT COUNT(*) AS count FROM tenancy_receipt_archive",
            [],
        ))
        .await?
        .context("archive count")?
        .try_get("", "count")?;
    ensure!(rows == i64::try_from(BATCH * CYCLES)?);
    let integrity: String = db
        .query_one(sqlite("PRAGMA integrity_check", []))
        .await?
        .context("integrity result")?
        .try_get_by_index(0)?;
    ensure!(integrity == "ok");
    db.close().await?;
    Ok(())
}
