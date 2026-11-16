//! Current isolated reader consumes genuine predecessor terminal jobs and budgets.
use super::*;
use bitrouter_ai::tenancy::identity::{ExecutionId, InstallationId, TenantId};

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
#[ignore = "internal terminal history reader; select the bounded revision-budget parent"]
async fn consumer() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(ROOT).context("terminal history root")?);
    let bytes = std::fs::read(root.join("terminal-history.json"))?;
    ensure!(bytes.len() <= 1024 * 1024);
    let original: Value = serde_json::from_slice(&bytes)?;
    let installation: InstallationId = original["installation_id"]
        .as_str()
        .context("installation")?
        .try_into()?;
    let tenant: TenantId = original["tenant_id"]
        .as_str()
        .context("tenant")?
        .try_into()?;
    let jobs = original["jobs"].as_array().context("historical jobs")?;
    let counters = original["counters"]
        .as_array()
        .context("historical counters")?;
    ensure!(
        jobs.len() == 2
            && counters.len() == 2
            && jobs[0]["configuration"] != jobs[1]["configuration"]
    );
    let database = crate::db::connect_owned_tenancy(&format!(
        "sqlite://{}",
        root.join("native-paid.db").display()
    ))
    .await?;
    let db = database.database().clone();
    let service = Service::open_owned_profile(
        database,
        &crate::config::tenancy::TenancyConfig {
            enabled: true,
            wrapping_key_env: Some(KEY.into()),
            ..Default::default()
        },
        &crate::config::ContinuationConfig::default(),
    )
    .await?;
    let checked = async {
        same_rows(rows(&db, "owned_judge_job").await?, jobs)?;
        same_rows(rows(&db, "owned_judge_budget").await?, counters)?;
        for kind in ["owned_judge_pending", "owned_judge_active_v3"] {
            ensure!(rows(&db, kind).await?.is_empty());
        }
        let mut receipts = Vec::new();
        for job in jobs {
            let execution: ExecutionId = job["execution"]
                .as_str()
                .context("historical execution")?
                .try_into()?;
            receipts.push(
                crate::tenancy::receipts::store::lookup(&db, &installation, &tenant, &execution)
                    .await?
                    .context("historical receipt")?,
            );
        }
        ensure!(serde_json::to_value(receipts)? == original["receipts"]);
        ensure!(service.current().is_ok());
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let stopped = service.shutdown().await;
    let closed = db.close().await;
    checked.and(stopped).and(closed.map_err(Into::into))?;
    signal("native-terminal-recovered")?;
    Ok(())
}

pub(in crate::tenancy) async fn reopen_twice(root: &Path) -> Result<()> {
    for _ in 0..2 {
        let mut consumer = Child::start("terminal::consumer", root, "terminal", None)?;
        let observed = consumer.line("native-terminal-recovered").await;
        consumer.finish(false, observed).await?;
    }
    Ok(())
}
