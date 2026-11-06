//! V2 inputs and borrowed checks for the existing bounded Eval transaction.
use super::*;
use crate::offline::{
    large::{Charge, Options, Workspace},
    recovery::{
        plan::Anchor,
        v2::{inputs::Prepared, memory},
    },
};
use anyhow::Context;
use std::sync::Arc;

pub(super) const FORMAT: &str = "bitrouter.offline.eval-backfill.v2";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanV2 {
    format: String,
    bundle: PathBuf,
    database_evidence_digest: String,
    source_kind: String,
    ownership_evidence_digest: String,
    ownership_evidence: PathBuf,
    manifest: PathBuf,
    manifest_digest: String,
    target: PathBuf,
    target_bundle: PathBuf,
    target_evidence_digest: String,
    target_installation: InstallationId,
    provisioning: Anchor,
}

pub(super) fn decode(bytes: &[u8]) -> Result<(Plan, Option<Anchor>)> {
    #[derive(serde::Deserialize)]
    struct Version {
        format: String,
    }
    if serde_json::from_slice::<Version>(bytes)?.format != FORMAT {
        return Ok((serde_json::from_slice(bytes)?, None));
    }
    let plan: PlanV2 = serde_json::from_slice(bytes)?;
    ensure!(
        plan.provisioning.path.is_absolute(),
        "absolute provisioning path required"
    );
    files::validate_digest(&plan.provisioning.sha256)?;
    Ok((
        Plan {
            format: plan.format,
            bundle: plan.bundle,
            database_evidence_digest: plan.database_evidence_digest,
            source_kind: plan.source_kind,
            ownership_evidence_digest: plan.ownership_evidence_digest,
            ownership_evidence: plan.ownership_evidence,
            manifest: plan.manifest,
            manifest_digest: plan.manifest_digest,
            target: plan.target,
            target_bundle: plan.target_bundle,
            target_evidence_digest: plan.target_evidence_digest,
            target_installation: plan.target_installation,
            provisioning_operations: Vec::new(),
        },
        Some(plan.provisioning),
    ))
}

pub(super) struct Large<'a> {
    pub workspace: &'a Arc<Workspace>,
    pub options: &'a Options,
    pub provisioning: Prepared,
    _metadata: Charge,
}
impl<'a> Large<'a> {
    pub(super) fn prepare(
        plan: &Plan,
        anchor: &Anchor,
        output: &Path,
        workspace: &'a Arc<Workspace>,
        options: &'a Options,
    ) -> Result<Self> {
        let target = plan.target.canonicalize()?;
        let parent = output.parent().unwrap_or(Path::new(".")).canonicalize()?;
        let mut forbidden = vec![
            plan.bundle.canonicalize()?,
            plan.target_bundle.canonicalize()?,
            target.clone(),
            parent.join(output.file_name().context("output filename missing")?),
        ];
        for suffix in ["-wal", "-shm", "-journal"] {
            let mut path = target.as_os_str().to_os_string();
            path.push(suffix);
            forbidden.push(path.into());
        }
        let provisioning = Prepared::open(anchor, &forbidden, workspace)?;
        // Command/path bookkeeping; Connections holds each live cache. Returned
        // evidence reports retain separate actual-capacity charges.
        let metadata = workspace.reserve(65536)?;
        Ok(Self {
            workspace,
            options,
            provisioning,
            _metadata: metadata,
        })
    }

    pub(super) async fn bundle(
        &self,
        path: &Path,
        digest: &str,
        action: &'static str,
    ) -> Result<crate::offline::format::Report> {
        let options = Options {
            working_bytes: self.workspace.available(),
            ..self.options.clone()
        };
        crate::offline::read_bundle_with_options(path, Some(digest), action, &options).await
    }

    pub(super) fn input_charge(&self, path: &Path, maximum: u64) -> Result<Charge> {
        let size = std::fs::symlink_metadata(path)?.len();
        ensure!(size <= maximum, "Eval control input exceeds profile");
        self.workspace.reserve(
            usize::try_from(size)?
                .checked_mul(2)
                .and_then(|n| n.checked_add(8192))
                .ok_or(crate::offline::large::CapacityExceeded)?,
        )
    }

    pub(super) fn decoded_charge(&self, bytes: &[u8]) -> Result<Charge> {
        self.workspace.reserve(memory::json_required(bytes)?)
    }
}

pub(super) async fn inventory_in(
    db: &mut SqliteConnection,
    key: &[u8; 32],
    large: Option<&Large<'_>>,
) -> Result<(crate::offline::format::Inventory, Option<Charge>)> {
    match large {
        Some(large) => {
            let (value, charge) = crate::offline::large::inspect_in(db, key, large.workspace)
                .await?
                .into_parts();
            Ok((value, Some(charge)))
        }
        None => Ok((
            crate::offline::inventory::inspect_eval_in(db, key).await?,
            None,
        )),
    }
}

pub(super) async fn validate(
    db: &mut SqliteConnection,
    inventory: &crate::offline::format::Inventory,
    plan: &Plan,
    assignments: &BTreeMap<String, Assignment>,
    large: &mut Large<'_>,
) -> Result<()> {
    let assignments = assignments
        .values()
        .map(|assignment| (assignment.tenant.clone(), assignment.principal.clone()))
        .collect::<Vec<_>>();
    crate::tenancy::audit::recovery::v2::inspect_eval_authority_in(
        db,
        inventory,
        &plan.target_installation,
        &mut large.provisioning,
        &assignments,
        crate::offline::ValidationBudget {
            rows: 32,
            bytes: large.workspace.available(),
        },
        large.workspace,
    )
    .await?;
    Ok(())
}

pub(super) fn qualify(plan: &Plan, output: &Path, options: &Options) -> Result<()> {
    let scratch = options.scratch_directory.canonicalize()?;
    let target = crate::tenancy::ownership::resolve_path(&plan.target, false)?;
    let output_parent = output.parent().unwrap_or(Path::new(".")).canonicalize()?;
    for bundle in [&plan.bundle, &plan.target_bundle] {
        files::private_directory(bundle)?;
        let bundle = bundle.canonicalize()?;
        ensure!(
            !scratch.starts_with(&bundle)
                && !target.starts_with(&bundle)
                && !output_parent.starts_with(&bundle),
            "Eval scratch/target/output overlaps immutable input"
        );
    }
    Ok(())
}
