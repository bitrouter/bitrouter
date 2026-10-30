//! Real retired opaque files remain fenced and are checked again after SQL close.
use super::*;

fn selected(fixture: &Fixture) -> Result<PathBuf> {
    Ok(PathBuf::from(
        fixture.plan["selected_files"][0]["path"]
            .as_str()
            .context("selected retired file path")?,
    ))
}

#[tokio::test]
#[ignore = "actual bro CLI and normal Service/SDK producers; explicit binary/key required"]
async fn actual_cli_refuses_changed_linked_or_nonprivate_selected_files() -> Result<()> {
    let binary = binary()?;
    tokio::time::timeout(Duration::from_secs(110), async {
        for mode in ["changed", "symlink", "hardlink", "public"] {
            let fixture = Fixture::new().await?;
            let source = stopped_bytes(&fixture.source)?;
            let target = stopped_bytes(&fixture.target)?;
            let archive = files::read(&fixture.bundle.join("snapshot.sqlite"), 32 * 1024 * 1024)?;
            let selected = selected(&fixture)?;
            let original = files::read(&selected, 1024)?;
            match mode {
                "changed" => {
                    let mut changed = original.clone();
                    changed[0] ^= 1;
                    std::fs::write(&selected, changed)?;
                }
                "symlink" => {
                    let moved = fixture.directory.path().join("retired-original.secret");
                    std::fs::rename(&selected, &moved)?;
                    std::os::unix::fs::symlink(&moved, &selected)?;
                }
                "hardlink" => {
                    std::fs::hard_link(
                        &selected,
                        fixture.directory.path().join("retired-alias.secret"),
                    )?;
                }
                "public" => {
                    std::fs::set_permissions(&selected, std::fs::Permissions::from_mode(0o644))?;
                }
                _ => anyhow::bail!("unknown negative selected-file fixture"),
            }
            let plan = fixture.directory.path().join("selected-plan.json");
            fixture::private_json(&plan, &fixture.plan)?;
            let report = fixture.directory.path().join("selected-report");
            rejected(
                &invoke(&binary, &fixture, &plan, &report, None).await?,
                &report,
            )?;
            ensure!(
                stopped_bytes(&fixture.source)? == source
                    && stopped_bytes(&fixture.target)? == target
                    && files::read(&fixture.bundle.join("snapshot.sqlite"), 32 * 1024 * 1024)?
                        == archive,
                "selected-file rejection changed original database or archive bytes"
            );
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("selected-file CLI campaign watchdog")?
}

#[tokio::test]
#[ignore = "real held read and actual bro CLI control; explicit binary/key required"]
async fn selected_file_change_during_actual_read_prevents_publication_after_worker_close()
-> Result<()> {
    let binary = binary()?;
    tokio::time::timeout(Duration::from_secs(110), async {
        let fixture = Fixture::new().await?;
        let source = stopped_bytes(&fixture.source)?;
        let target = stopped_bytes(&fixture.target)?;
        let selected = selected(&fixture)?;
        let original = files::read(&selected, 1024)?;
        let path = fixture.directory.path().join("held-selected-plan.json");
        fixture::private_json(&path, &fixture.plan)?;
        let report = fixture.directory.path().join("held-selected-report");
        let held = Hold {
            entered: CancellationToken::new(),
            release: CancellationToken::new(),
        };
        let _release_on_failure = held.release.clone().drop_guard();
        let task_held = held.clone();
        let task_path = path.clone();
        let task_report = report.clone();
        let caller = tokio::spawn(async move {
            super::super::run_inner(&task_path, &task_report, Some(task_held)).await
        });
        let changed = async {
            tokio::time::timeout(Duration::from_secs(10), held.entered.cancelled())
                .await
                .context("actual recovery read never reached hold")?;
            ensure!(
                crate::tenancy::ownership::acquire_path(&fixture.target, false).is_err(),
                "held SQL worker lost its original file owner"
            );
            ensure!(
                files::read(&report.join("selected-000.opaque"), 1024)? == original
                    && !report.join("VERIFIED").exists(),
                "original retired-file read was not captured before held SQL"
            );
            let mut altered = original.clone();
            altered[0] ^= 1;
            std::fs::write(&selected, &altered)?;
            Ok::<_, anyhow::Error>(altered)
        }
        .await;
        held.release.cancel();
        // Join the original task after release even if the observation failed;
        // cancellation does not substitute for the real worker close ACK.
        let completed = tokio::time::timeout(Duration::from_secs(20), caller).await;
        let altered = changed?;
        let outcome = completed.context("recovery worker did not finish")??;
        ensure!(
            outcome.is_err(),
            "changed selected file was accepted after actual SQL close"
        );
        let owner = crate::tenancy::ownership::acquire_path(&fixture.target, false)?;
        drop(owner);
        ensure!(
            report.join("INCOMPLETE").exists()
                && !report.join("VERIFIED").exists()
                && !report.join("report.json").exists()
                && files::read(&report.join("selected-000.opaque"), 1024)? == original
                && files::read(&selected, 1024)? == altered
                && stopped_bytes(&fixture.source)? == source
                && stopped_bytes(&fixture.target)? == target,
            "changed-file refusal repaired inputs, changed databases, or published success"
        );
        std::fs::write(&selected, &original)?;
        let good_report = fixture.directory.path().join("restored-selected-report");
        let control = invoke(&binary, &fixture, &path, &good_report, None).await?;
        ensure!(
            control.status.code() == Some(0),
            "restored actual CLI control failed"
        );
        let value: Value = serde_json::from_slice(&control.stdout)?;
        ensure!(
            value["fresh_provisioning_verified"] == true
                && value["external_writers_independently_excluded"] == false
                && value["complete"] == false
                && value["cutover_ready"] == false
                && good_report.join("VERIFIED").exists()
                && !good_report.join("INCOMPLETE").exists()
                && report.join("INCOMPLETE").exists()
                && !report.join("VERIFIED").exists(),
            "new successful control rewrote earlier failure or overclaimed external fencing"
        );
        ensure!(
            stopped_bytes(&fixture.source)? == source && stopped_bytes(&fixture.target)? == target,
            "restored read-only CLI control changed database bytes"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("held selected-file campaign watchdog")?
}
