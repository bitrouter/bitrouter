use super::*;
use crate::offline::{
    large::Options,
    recovery::v2::format::{PROVISIONING, ProvisioningHeader, ProvisioningRecord},
};
use sea_orm::sqlx::{Connection as _, Row as _};
use std::os::unix::fs::PermissionsExt;

fn options(fixture: &Fixture) -> Result<Options> {
    std::fs::set_permissions(
        fixture.directory.path(),
        std::fs::Permissions::from_mode(0o700),
    )?;
    Ok(Options {
        working_bytes: 64 * 1024 * 1024,
        scratch_directory: fixture.directory.path().canonicalize()?,
        min_free_bytes: 1,
    })
}

async fn plan(fixture: &mut Fixture, malformed_last: bool) -> Result<()> {
    std::fs::set_permissions(&fixture.target, std::fs::Permissions::from_mode(0o600))?;
    let owner = crate::tenancy::ownership::acquire_path(&fixture.target, false)?;
    let path = owner.path.clone();
    let mut connections = Connections::new(CancellationToken::new(), Some(owner));
    // The v1 snapshot reopened this owned WAL target read-only. A real writer
    // close settles SQLite's WAL before the immutable-source check below.
    connections.open_destination(&path).await?;
    let mut tx = connections
        .source
        .as_mut()
        .context("target")?
        .begin()
        .await?;
    let rows = sea_orm::sqlx::query("SELECT principal_id,result_json FROM tenancy_operations")
        .fetch_all(&mut *tx)
        .await?;
    let mut operations = rows
        .into_iter()
        .map(|row| {
            let principal = PrincipalId::try_from(row.try_get::<String, _>("principal_id")?)?;
            let result = serde_json::from_str::<bitrouter_ai::tenancy::operation::OperationResult>(
                &row.try_get::<String, _>("result_json")?,
            )?;
            Ok::<_, anyhow::Error>((principal, result))
        })
        .collect::<Result<Vec<_>>>()?;
    operations.sort_by_key(|(_, result)| result.authority_revision.map(|revision| revision.get()));
    tx.rollback().await?;
    connections.close().await?;
    crate::offline::large::Source::inspect(&path)?;
    drop(connections);
    let header = ProvisioningHeader {
        format: PROVISIONING.into(),
        installation_id: fixture.provision.installation.clone(),
        operation_count: operations.len() as u64,
        configuration_count: 0,
        active_count: 0,
        resource_count: 0,
    };
    let mut bytes = serde_json::to_vec(&header)?;
    bytes.push(b'\n');
    for (principal_id, result) in operations {
        bytes.extend(serde_json::to_vec(&ProvisioningRecord::Operation {
            principal_id,
            result,
        })?);
        bytes.push(b'\n');
    }
    if malformed_last {
        bytes.extend_from_slice(b"{}\n");
    }
    let path = fixture.directory.path().join("provisioning.jsonl");
    files::write_new(&path, &bytes)?;
    fixture.plan["format"] = json!("bitrouter.offline.eval-backfill.v2");
    fixture
        .plan
        .as_object_mut()
        .context("plan")?
        .remove("provisioning_operations");
    fixture.plan["provisioning"] = json!({"path":path,"sha256":files::sha(&bytes)});
    Ok(())
}

#[tokio::test]
async fn v2_borrowed_provisioning_imports_the_same_selected_graph_and_refuses_replay() -> Result<()>
{
    tokio::time::timeout(TIMEOUT, async {
        let mut fixture = Fixture::new().await?;
        plan(&mut fixture, false).await?;
        let (path, anchor, output) = fixture.write_plan("large-apply")?;
        let report = apply::run_with_options(&path, &anchor, &output, options(&fixture)?).await?;
        let value = serde_json::to_value(report)?;
        ensure!(value["format"] == "bitrouter.offline.eval-backfill-report.v2");
        ensure!(value["imported_counts"] == json!([2, 2, 2, 2]) && value["provenance_rows"] == 2);
        ensure!(output.join("RECORDED").exists() && !output.join("INCOMPLETE").exists());
        let replay = fixture.directory.path().join("large-replay");
        ensure!(
            apply::run_with_options(&path, &anchor, &replay, options(&fixture)?)
                .await
                .is_err()
        );
        ensure!(!replay.join("RECORDED").exists());
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn v2_extra_final_record_refuses_without_target_writes() -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        let mut fixture = Fixture::new().await?;
        plan(&mut fixture, true).await?;
        let before = std::fs::read(&fixture.target)?;
        let (path, anchor, output) = fixture.write_plan("large-last-record")?;
        ensure!(
            apply::run_with_options(&path, &anchor, &output, options(&fixture)?)
                .await
                .is_err()
        );
        ensure!(std::fs::read(&fixture.target)? == before);
        ensure!(!output.join("RECORDED").exists());
        let owner = crate::tenancy::ownership::acquire_path(&fixture.target, false)?;
        drop(owner);
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn v2_input_bundle_overlap_refuses_before_scratch_or_target_lock_creation() -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        let mut fixture = Fixture::new().await?;
        plan(&mut fixture, false).await?;
        let source = fixture.bundle.join("snapshot.sqlite");
        let original = std::fs::read(&source)?;
        let original_plan = fixture.plan.clone();
        let mut scratch = options(&fixture)?;
        scratch.scratch_directory = fixture.bundle.canonicalize()?;
        let (path, anchor, output) = fixture.write_plan("bad-scratch")?;
        ensure!(
            apply::run_with_options(&path, &anchor, &output, scratch)
                .await
                .is_err()
        );
        fixture.plan = original_plan;
        let alias = fixture.directory.path().join("bundle-alias");
        std::os::unix::fs::symlink(&fixture.bundle, &alias)?;
        fixture.plan["target"] = json!(alias.join("snapshot.sqlite"));
        let (path, anchor, output) = fixture.write_plan("bad-target")?;
        ensure!(
            apply::run_with_options(&path, &anchor, &output, options(&fixture)?)
                .await
                .is_err()
        );
        ensure!(std::fs::read(&source)? == original);
        ensure!(std::fs::read_dir(&fixture.bundle)?.count() == 4);
        super::super::super::read_bundle(
            &fixture.bundle,
            Some(
                fixture.plan["database_evidence_digest"]
                    .as_str()
                    .context("anchor")?,
            ),
            "verify",
        )
        .await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
