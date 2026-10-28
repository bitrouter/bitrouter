//! Explicit local path adapter; never resolves an HTTP or host configuration.
use anyhow::Result;
use bitrouter::{gateway::error::BitrouterError, output::Output};
use clap::Subcommand;
use std::path::PathBuf;

#[derive(Subcommand)]
pub(crate) enum Action {
    /// Verify retired evidence and fresh provisioning read-only; never release ingress.
    Recover {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        report: PathBuf,
    },
    /// Inspect bounded Legacy Eval partitions, without copying or granting ownership.
    #[cfg(feature = "core")]
    EvalInventory {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        evidence_digest: String,
        #[arg(long)]
        source_kind: String,
        #[arg(long)]
        report: PathBuf,
    },
    /// Apply one anchored four-table Eval graph to a stopped fresh target.
    #[cfg(feature = "core")]
    EvalBackfill {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        plan_digest: String,
        #[arg(long)]
        report: PathBuf,
    },
    /// Validate and record explicit M0 mapping claims; never apply backfill.
    Map {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        evidence_digest: String,
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        report: PathBuf,
    },
    /// Serve existing receipt GET/page from a verified retired SQLite archive.
    Audit {
        #[arg(long)]
        archive: PathBuf,
        /// New private directory outside the immutable archive.
        #[arg(long)]
        projection: PathBuf,
        #[arg(long)]
        evidence_digest: String,
        /// Bounded JSON containing only named credential env references/scopes.
        #[arg(long)]
        control_config: PathBuf,
        #[arg(long)]
        listen: std::net::SocketAddr,
    },
    /// Snapshot one stopped SQLite DB. External state is excluded; not a full backup.
    Snapshot {
        /// Existing literal SQLite file path (not a URL).
        #[arg(long)]
        database: PathBuf,
        /// New secured output directory; existing paths are never reused.
        #[arg(long)]
        bundle: PathBuf,
    },
    /// Recompute a DB-only bundle's bounded, nonsecret inventory.
    Inventory {
        #[arg(long)]
        bundle: PathBuf,
    },
    /// Verify DB evidence against a digest retained outside the bundle.
    Verify {
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        evidence_digest: String,
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
        Action::Recover { plan, report } => {
            output.emit(&bitrouter::offline::recover(plan, report).await?)?
        }
        #[cfg(feature = "core")]
        Action::EvalInventory {
            bundle,
            evidence_digest,
            source_kind,
            report,
        } => output.emit(
            &bitrouter::offline::eval_inventory(bundle, evidence_digest, source_kind, report)
                .await?,
        )?,
        #[cfg(feature = "core")]
        Action::EvalBackfill {
            plan,
            plan_digest,
            report,
        } => output.emit(&bitrouter::offline::eval_backfill(plan, plan_digest, report).await?)?,
        Action::Map {
            bundle,
            evidence_digest,
            manifest,
            report,
        } => output
            .emit(&bitrouter::offline::map(bundle, evidence_digest, manifest, report).await?)?,
        Action::Audit {
            archive,
            projection,
            evidence_digest,
            control_config,
            listen,
        } => {
            bitrouter::offline::audit(
                archive,
                projection,
                evidence_digest,
                control_config,
                *listen,
                output,
            )
            .await?;
        }
        Action::Snapshot { database, bundle } => {
            output.emit(&bitrouter::offline::snapshot(database, bundle).await?)?
        }
        Action::Inventory { bundle } => {
            output.emit(&bitrouter::offline::inventory(bundle).await?)?
        }
        Action::Verify {
            bundle,
            evidence_digest,
        } => output.emit(&bitrouter::offline::verify(bundle, evidence_digest).await?)?,
    }
    Ok(())
}
