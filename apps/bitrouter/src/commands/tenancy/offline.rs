//! Explicit local path adapter; never resolves an HTTP or host configuration.
use anyhow::Result;
use bitrouter::{gateway::error::BitrouterError, output::Output};
use clap::{Args, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum EvidenceFormat {
    V1,
    V2,
}

#[derive(Args)]
pub(crate) struct Working {
    /// Existing private scratch parent; required for complete v2 processing.
    #[arg(long)]
    scratch_directory: Option<PathBuf>,
    /// Aggregate application working bytes, independent of database size.
    #[arg(long, alias = "validation-bytes", default_value_t = 67_108_864)]
    working_bytes: usize,
    /// Minimum free bytes to preserve on each output/scratch volume.
    #[arg(long, default_value_t = 268_435_456)]
    scratch_free_bytes: u64,
    #[arg(long, default_value_t = 32)]
    validation_page_rows: usize,
}
impl Working {
    fn options(&self) -> Option<bitrouter::offline::large::Options> {
        self.scratch_directory
            .as_ref()
            .map(|path| bitrouter::offline::large::Options {
                working_bytes: self.working_bytes,
                scratch_directory: path.clone(),
                min_free_bytes: self.scratch_free_bytes,
            })
    }
    fn required(&self) -> Result<bitrouter::offline::large::Options> {
        self.options().ok_or_else(|| {
            BitrouterError::bad_request("v2 offline processing requires --scratch-directory").into()
        })
    }
    fn validation(&self) -> bitrouter::offline::ValidationBudget {
        bitrouter::offline::ValidationBudget {
            rows: self.validation_page_rows,
            bytes: self.working_bytes,
        }
    }
}

#[derive(Subcommand)]
pub(crate) enum Action {
    /// Upgrade a stopped legacy count-saturated authority with explicit acknowledgment.
    UpgradeCapacity {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        expected_installation_id: String,
        #[arg(long)]
        expected_authority_revision: u64,
        #[arg(long)]
        operation_id: String,
        #[arg(long)]
        acknowledge_legacy_count_saturation: bool,
        #[arg(long, default_value_t = 32)]
        validation_page_rows: usize,
        #[arg(long, default_value_t = 67_108_864)]
        validation_bytes: usize,
    },
    /// Verify retired evidence and fresh provisioning read-only; never release ingress.
    Recover {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        report: PathBuf,
        #[command(flatten)]
        working: Working,
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
        #[command(flatten)]
        working: Working,
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
        #[command(flatten)]
        working: Working,
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
        #[command(flatten)]
        working: Working,
    },
    /// Snapshot one stopped SQLite DB. External state is excluded; not a full backup.
    Snapshot {
        /// Existing literal SQLite file path (not a URL).
        #[arg(long)]
        database: PathBuf,
        /// New secured output directory; existing paths are never reused.
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long, value_enum, default_value = "v1")]
        format: EvidenceFormat,
        #[command(flatten)]
        working: Working,
    },
    /// Recompute a DB-only bundle's bounded, nonsecret inventory.
    Inventory {
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
        Action::UpgradeCapacity {
            database,
            expected_installation_id,
            expected_authority_revision,
            operation_id,
            acknowledge_legacy_count_saturation,
            validation_page_rows,
            validation_bytes,
        } => output.emit(
            &bitrouter::offline::upgrade_capacity(
                database,
                expected_installation_id,
                *expected_authority_revision,
                operation_id,
                *acknowledge_legacy_count_saturation,
                bitrouter::offline::ValidationBudget {
                    rows: *validation_page_rows,
                    bytes: *validation_bytes,
                },
            )
            .await?,
        )?,
        Action::Recover {
            plan,
            report,
            working,
        } => match working.options() {
            Some(options) => output.emit(
                &bitrouter::offline::recover_with_options(
                    plan,
                    report,
                    working.validation(),
                    options,
                )
                .await?,
            )?,
            None => output.emit(
                &bitrouter::offline::recover_with_budget(plan, report, working.validation())
                    .await?,
            )?,
        },
        #[cfg(feature = "core")]
        Action::EvalInventory {
            bundle,
            evidence_digest,
            source_kind,
            report,
            working,
        } => match working.options() {
            Some(options) => output.emit(
                &bitrouter::offline::eval_inventory_with_options(
                    bundle,
                    evidence_digest,
                    source_kind,
                    report,
                    options,
                )
                .await?,
            )?,
            None => output.emit(
                &bitrouter::offline::eval_inventory(bundle, evidence_digest, source_kind, report)
                    .await?,
            )?,
        },
        #[cfg(feature = "core")]
        Action::EvalBackfill {
            plan,
            plan_digest,
            report,
            working,
        } => match working.options() {
            Some(options) => output.emit(
                &bitrouter::offline::eval_backfill_with_options(plan, plan_digest, report, options)
                    .await?,
            )?,
            None => {
                output.emit(&bitrouter::offline::eval_backfill(plan, plan_digest, report).await?)?
            }
        },
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
            working,
        } => {
            if let Some(options) = working.options() {
                bitrouter::offline::audit_with_options(
                    bitrouter::offline::AuditSource {
                        archive,
                        evidence_digest,
                    },
                    projection,
                    control_config,
                    *listen,
                    output,
                    working.validation(),
                    options,
                )
                .await?;
            } else {
                bitrouter::offline::audit_with_budget(
                    archive,
                    projection,
                    evidence_digest,
                    control_config,
                    *listen,
                    output,
                    working.validation(),
                )
                .await?;
            }
        }
        Action::Snapshot {
            database,
            bundle,
            format,
            working,
        } => match format {
            EvidenceFormat::V1 => {
                if working.options().is_some() {
                    return Err(BitrouterError::bad_request(
                        "--scratch-directory requires snapshot --format v2",
                    )
                    .into());
                }
                output.emit(&bitrouter::offline::snapshot(database, bundle).await?)?;
            }
            EvidenceFormat::V2 => output.emit(
                &bitrouter::offline::snapshot_with_options(database, bundle, working.required()?)
                    .await?,
            )?,
        },
        Action::Inventory { bundle, working } => match working.options() {
            Some(options) => {
                output.emit(&bitrouter::offline::inventory_with_options(bundle, options).await?)?
            }
            None => output.emit(&bitrouter::offline::inventory(bundle).await?)?,
        },
        Action::Verify {
            bundle,
            evidence_digest,
            working,
        } => match working.options() {
            Some(options) => output.emit(
                &bitrouter::offline::verify_with_options(bundle, evidence_digest, options).await?,
            )?,
            None => output.emit(&bitrouter::offline::verify(bundle, evidence_digest).await?)?,
        },
        Action::Restore {
            bundle,
            evidence_digest,
            database,
            expected_installation_id,
            wrapping_key_env,
            working,
        } => {
            output.emit(
                &bitrouter::offline::restore_with_options(
                    bundle,
                    evidence_digest,
                    database,
                    expected_installation_id,
                    wrapping_key_env,
                    working.validation_page_rows,
                    working.required()?,
                )
                .await?,
            )?;
        }
    }
    Ok(())
}
