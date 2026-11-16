//! Genuine predecessor terminal history survives two current bootstraps unchanged.
use super::*;
use bitrouter_ai::tenancy::identity::{ExecutionId, InstallationId, TenantId};
use tokio::io::AsyncReadExt;

fn same_rows(mut actual: Vec<Value>, expected: &[Value]) -> Result<()> {
    // The predecessor export has no SQL ordering contract. Compare complete
    // rows as a multiset; retain duplicates and every persisted field.
    let mut expected = expected.to_vec();
    actual.sort_unstable_by_key(Value::to_string);
    expected.sort_unstable_by_key(Value::to_string);
    ensure!(
        actual == expected,
        "historical records changed across bootstrap"
    );
    Ok(())
}

#[tokio::test]
async fn native_paid_revision_budget_survives_rotation_reactivation_and_two_revision_bootstrap()
-> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(180), async {
        let root = tempfile::tempdir()?;
        let mut child = tokio::process::Command::new(recovery::process::historical_producer()?)
            .args(["--exact", "tenancy::tests::execution::native::session::experiment::paid::revision_budget::native_paid_revision_budget_survives_rotation_reactivation_and_two_revision_bootstrap", "--nocapture", "--test-threads=1"])
            .env("BITROUTER_PRE_M0_TERMINAL_ROOT", root.path())
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn()?;
        let mut out = child.stdout.take().context("predecessor stdout")?.take(8193);
        let mut err = child.stderr.take().context("predecessor stderr")?.take(8193);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let (_, _, status) = tokio::try_join!(out.read_to_end(&mut stdout), err.read_to_end(&mut stderr), child.wait())?;
        ensure!(stdout.len() <= 8192 && stderr.len() <= 8192, "predecessor diagnostics bound");
        ensure!(status.success(), "terminal history producer failed: {}", String::from_utf8_lossy(&stderr));
        ensure!(String::from_utf8_lossy(&stdout).contains("pre-m0-terminal-history-exported"), "missing genuine history export acknowledgement");
        let bytes = std::fs::read(root.path().join("terminal-history.json"))?;
        ensure!(bytes.len() <= 1024 * 1024);
        let original: Value = serde_json::from_slice(&bytes)?;
        let installation: InstallationId = original["installation_id"].as_str().context("installation")?.try_into()?;
        let tenant: TenantId = original["tenant_id"].as_str().context("tenant")?.try_into()?;
        let jobs = original["jobs"].as_array().context("historical jobs")?;
        let counters = original["counters"].as_array().context("historical counters")?;
        ensure!(jobs.len() == 2 && counters.len() == 2 && jobs[0]["configuration"] != jobs[1]["configuration"]);
        for _ in 0..2 {
            let database = crate::db::connect_owned_tenancy(&format!("sqlite://{}", root.path().join("native-paid.db").display())).await?;
            let (db, owner, claim) = database.claim()?;
            let service = Service::start_resources_with_continuations(
                db.clone(), Some(owner), crate::tenancy::store::load(&db, Utc::now()).await?,
                Arc::new(crate::tenancy::resources::Resources::with_key(installation.clone(), [29; 32])?),
                crate::tenancy::Provisioning {
                    import: None, continuations: &crate::config::ContinuationConfig::default(), claim: Some(claim),
                    receipt_observer: crate::telemetry::receipt_latency::Sink::default(),
                    config: &crate::config::tenancy::TenancyConfig::default(),
                }, None,
            ).await?;
            let checked = async {
                same_rows(recovery::rows(&db, "owned_judge_job").await?, jobs)?;
                same_rows(recovery::rows(&db, "owned_judge_budget").await?, counters)?;
                for kind in ["owned_judge_pending", "owned_judge_active_v3"] {
                    ensure!(recovery::rows(&db, kind).await?.is_empty());
                }
                let mut receipts = Vec::new();
                for job in jobs {
                    let execution: ExecutionId = job["execution"].as_str().context("historical execution")?.try_into()?;
                    receipts.push(crate::tenancy::receipts::store::lookup(&db, &installation, &tenant, &execution).await?.context("historical receipt")?);
                }
                ensure!(serde_json::to_value(receipts)? == original["receipts"]);
                ensure!(service.current().is_ok());
                Ok::<_, anyhow::Error>(())
            }.await;
            let stopped = service.shutdown().await;
            let closed = db.close().await;
            checked.and(stopped).and(closed.map_err(Into::into))?;
        }
        Ok::<_, anyhow::Error>(())
    }).await?
}
