//! Explicit local path adapter; never resolves an HTTP or host configuration.
use crate::{gateway::error::BitrouterError, output::Output};
use anyhow::Result;
use clap::{Args, Subcommand};
use std::path::PathBuf;

#[derive(Args)]
pub(crate) struct Working {
    /// Absolute path to an existing private scratch directory.
    #[arg(long)]
    scratch_directory: PathBuf,
    /// Aggregate application working bytes, independent of database size.
    #[arg(long, default_value_t = 67_108_864)]
    working_bytes: usize,
    /// Minimum free bytes to preserve on each output/scratch volume.
    #[arg(long, default_value_t = 268_435_456)]
    scratch_free_bytes: u64,
    #[arg(long, default_value_t = 32)]
    validation_page_rows: usize,
}
impl Working {
    fn options(&self) -> crate::offline::large::Options {
        crate::offline::large::Options {
            working_bytes: self.working_bytes,
            scratch_directory: self.scratch_directory.clone(),
            min_free_bytes: self.scratch_free_bytes,
        }
    }
}

#[derive(Subcommand)]
pub(crate) enum Action {
    /// Snapshot one stopped SQLite DB. External state is excluded; not a full backup.
    Snapshot {
        /// Existing literal SQLite file path (not a URL).
        #[arg(long)]
        database: PathBuf,
        /// New secured output directory; existing paths are never reused.
        #[arg(long)]
        bundle: PathBuf,
        #[command(flatten)]
        working: Working,
    },
    /// Verify DB evidence against a digest retained outside the bundle.
    Verify {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        evidence_digest: String,
        #[command(flatten)]
        working: Working,
    },
    /// Restore a complete database to a new path; startup and external recovery remain separate.
    Restore {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        evidence_digest: String,
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        expected_installation_id: String,
        #[arg(long)]
        wrapping_key_env: String,
        #[command(flatten)]
        working: Working,
    },
}
pub(super) async fn run(action: &Action, context: Option<&str>, output: &Output) -> Result<()> {
    if context.is_some() {
        return Err(BitrouterError::bad_request(
            "tenancy offline accepts explicit local paths only; omit --context",
        )
        .into());
    }
    match action {
        Action::Snapshot {
            database,
            bundle,
            working,
        } => output.emit(
            &crate::offline::snapshot_with_options(database, bundle, working.options()).await?,
        )?,
        Action::Verify {
            bundle,
            evidence_digest,
            working,
        } => output.emit(
            &crate::offline::verify_with_options(bundle, evidence_digest, working.options())
                .await?,
        )?,
        Action::Restore {
            bundle,
            evidence_digest,
            database,
            expected_installation_id,
            wrapping_key_env,
            working,
        } => {
            output.emit(
                &crate::offline::restore_with_options(
                    bundle,
                    evidence_digest,
                    database,
                    expected_installation_id,
                    wrapping_key_env,
                    working.validation_page_rows,
                    working.options(),
                )
                .await?,
            )?;
        }
    }
    Ok(())
}
