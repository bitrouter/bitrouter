//! Snapshot, verify, and restore the current SQLite gateway database.

pub(crate) mod engine;
mod files;
pub(crate) mod format;
mod inventory;
pub mod large;
pub(crate) mod memory;
mod schema;
mod streamed;

use crate::output::CliReport;
use anyhow::Result;
use std::path::Path;
use tokio_util::sync::CancellationToken;

/// Temporary memory for complete offline authority validation, not history size.
#[derive(Clone, Copy)]
pub struct ValidationBudget {
    /// Number of small authority facts or keys in one page.
    pub rows: usize,
    /// Aggregate validation working bytes, including sequential object decoding.
    pub bytes: usize,
}
impl Default for ValidationBudget {
    fn default() -> Self {
        Self {
            rows: 32,
            bytes: 64 * 1024 * 1024,
        }
    }
}

/// Create v2 DB-only evidence using finite working memory and disk scratch.
/// The source must be cleanly stopped and all external writers fenced.
///
/// # Errors
/// Refuses live/changed sources, insufficient working resources, unsupported
/// schema, reused output or unconfirmed worker cleanup. Retain INCOMPLETE output.
pub async fn snapshot_with_options(
    database: &Path,
    bundle: &Path,
    options: large::Options,
) -> Result<impl CliReport> {
    snapshot_with_token(database, bundle, options, CancellationToken::new()).await
}

// Keep the public error mapping and the actual snapshot owner in one path.
async fn snapshot_with_token(
    database: &Path,
    bundle: &Path,
    options: large::Options,
    cancel: CancellationToken,
) -> Result<impl CliReport> {
    streamed::run_with_token(streamed::Action::Snapshot { database: database.into(), bundle: bundle.into() }, options, cancel)
        .await.map_err(|_| failure("offline v2 snapshot failed: require a cleanly stopped, externally fenced private SQLite source, private scratch and a new bundle; retain INCOMPLETE output"))
}

/// Link test cancellation without abandoning the real snapshot owner waiter.
#[cfg(all(test, unix))]
pub(crate) async fn snapshot_with_options_for_test(
    database: &Path,
    bundle: &Path,
    options: large::Options,
    parent: &CancellationToken,
) -> Result<impl CliReport> {
    snapshot_with_token(database, bundle, options, parent.child_token()).await
}

/// Verify the database bundle against an independently retained digest.
/// # Errors
/// Refuses invalid evidence, invalid resource limits, or unconfirmed cleanup.
pub async fn verify_with_options(
    bundle: &Path,
    evidence_digest: &str,
    options: large::Options,
) -> Result<impl CliReport> {
    files::validate_digest(evidence_digest)?;
    streamed::run(
        streamed::Action::Read {
            bundle: bundle.into(),
            expected: Some(evidence_digest.into()),
            action: "verify",
        },
        options,
    )
    .await
    .map_err(|_| failure("offline verification failed: evidence anchor or database validation failed, or working resources were insufficient"))
}

/// Restore an anchored database into a new path, preserving its original identity.
/// This does not migrate, initialize, start authority, or authorize serving. Fence
/// the original installation; normal startup and external-state recovery remain
/// mandatory before releasing ingress.
///
/// # Errors
/// Refuses existing destinations, mismatched identity/key, invalid evidence or
/// unconfirmed cleanup. Preserve incomplete staging for reconciliation.
pub async fn restore_with_options(
    bundle: &Path,
    evidence_digest: &str,
    database: &Path,
    expected_installation_id: &str,
    wrapping_key_env: &str,
    validation_rows: usize,
    options: large::Options,
) -> Result<impl CliReport> {
    let installation = expected_installation_id.try_into()?;
    streamed::run(streamed::Action::Restore {
        bundle: bundle.into(), expected: evidence_digest.into(), database: database.into(),
        installation, wrapping_key_env: wrapping_key_env.into(), validation_rows,
    }, options).await.map_err(|_| failure("offline database restore failed or is incomplete: preserve staging and the stopped destination; do not release ingress or run the original installation concurrently"))
}
pub(super) fn failure(message: &'static str) -> anyhow::Error {
    // Do not forward SQL/JSON/OS errors: their details can contain source payload
    // or schema defaults. The CLI emits exactly one standard nonsecret error.
    crate::gateway::error::BitrouterError::bad_request(message).into()
}
