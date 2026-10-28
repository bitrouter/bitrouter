//! Negative-only changes to a real producer row; indexed bytes remain valid.
use super::*;

#[tokio::test]
#[ignore = "isolated actual native recovery scope campaign; select this parent"]
async fn native_recovery_checks_exact_seed_scope_and_real_acquisition_origin() -> Result<()> {
    campaign(&[
        "scope-control",
        "scope-installation",
        "scope-tenant",
        "scope-principal",
        "scope-revision",
        "scope-execution",
        "origin-seed",
        "origin-current",
        "origin-other-proof",
    ])
    .await
}

pub(super) async fn case(root: &Path, mode: &str) -> Result<()> {
    let stopped = Stopped::produce(root, mode).await?;
    let result = async {
        let original = payload(&stopped.bound.fixture.db).await?;
        let before = terminals(&stopped.bound.fixture.db).await?;
        ensure!(before.is_empty());
        if mode == "scope-control" {
            let session = fixture::open(
                &stopped.bound,
                &stopped.generation,
                &root.join("workspace"),
                Arc::new(HostSpies::default()),
            )
            .await?;
            ensure!(
                session
                    .core()
                    .snapshot()
                    .await
                    .run
                    .as_ref()
                    .is_some_and(|run| run.status == RunStatus::Cancelled)
            );
            ensure!(terminals(&stopped.bound.fixture.db).await?.len() == 2);
            ensure!(stopped.generation.calls() == 0 && stopped.choice.calls() == 1);
            session.close().await?;
            return Ok(());
        }
        let mut row: Value = serde_json::from_str(&original)?;
        let current = stopped
            .bound
            .fixture
            .service
            .startup
            .as_ref()
            .context("actual connector StartupOwner")?
            .stamp(&stopped.bound.fixture.service.cleanup)?;
        if mode == "origin-other-proof" {
            // A second actual Service supplies a different H owner. This is a
            // negative argument, never a synthetic proof used for admission.
            let other_root = tempfile::tempdir()?;
            let other =
                fixture::sqlite(&stopped.generation, &stopped.choice, other_root.path(), 4).await?;
            let origin = stopped
                .bound
                .fixture
                .service
                .startup
                .as_ref()
                .context("actual StartupOwner")?;
            let wrong = origin.bootstrap_stamp(&other.fixture.service.cleanup);
            let other_closed = other.fixture.service.shutdown().await;
            let pool_closed = other.fixture.db.clone().close().await;
            ensure!(wrong.is_err(), "origin accepted another real Service proof");
            other_closed?;
            pool_closed?;
            ensure!(payload(&stopped.bound.fixture.db).await? == original);
            ensure!(terminals(&stopped.bound.fixture.db).await? == before);
            return Ok(());
        }
        let links = row["journal"]["owned_choices"]
            .as_object_mut()
            .context("real owned Choice links")?;
        ensure!(links.len() == 1);
        let link = links.values_mut().next().context("real link")?;
        let seeds = link["seeds"]
            .as_array_mut()
            .context("actual admission pair")?;
        ensure!(seeds.len() == 2);
        match mode {
            "scope-installation" => seeds[1]["installation"] = json!("foreign-installation"),
            "scope-tenant" => seeds[1]["tenant"] = json!("foreign-tenant"),
            "scope-principal" => seeds[1]["principal"] = json!("foreign-principal"),
            "scope-revision" => seeds[1]["revision"] = json!("foreign-revision"),
            "scope-execution" => seeds[1]["execution"] = json!("foreign-execution"),
            "origin-seed" => seeds[1]["origin"] = serde_json::to_value(&current)?,
            "origin-current" => {
                // Keep the complete seed/envelope origin internally consistent:
                // only the actual fresh-acquisition exclusion must reject it.
                for seed in seeds {
                    seed["origin"] = serde_json::to_value(&current)?;
                }
                row["recovery"]["origin"] = serde_json::to_value(&current)?;
            }
            _ => anyhow::bail!("unknown scope counterexample"),
        }
        let corrupted = replace_negative(&stopped.bound.fixture.db, &row).await?;
        ensure!(corrupted != original);
        no_write(&stopped, root, &corrupted, &before).await
    }
    .await;
    stopped.close(result).await
}
