//! Real child death after durable Accept and an actual provider request.
use super::*;
const CHILD: &str =
    "tenancy::tests::execution::native::capture::learning::qualification::crash::accepted_child";
#[tokio::test]
#[ignore = "only the bounded crash parent provisions and kills this authority process"]
async fn accepted_child() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(process::ROOT).context("crash root")?);
    let subject = std::env::var(process::SUBJECT)?;
    let judge = std::env::var(process::JUDGE)?;
    let ready = file_ready(&root.join("owned.db"), &subject, &judge).await?;
    let port = ready.port().await?;
    let work = port.clone();
    let task = AbortOnDropHandle::new(tokio::spawn(async move { work.checkpoint(native()).await }));
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let jobs = bodies(&ready.fixture, "owned_judge_job").await?;
            if jobs.len() == 1 && jobs[0]["status"] == "accepted" {
                ensure!(bodies(&ready.fixture, "owned_judge_pending").await?.len() == 1);
                break Ok::<_, anyhow::Error>(());
            }
            ensure!(!task.is_finished(), "paid child ended before acceptance");
            tokio::task::yield_now().await;
        }
    })
    .await??;
    process::witness_and_wait().await?;
    anyhow::bail!("crash parent unexpectedly allowed graceful child return")
}
async fn retained(db: &sea_orm::DatabaseConnection) -> Result<Vec<(String, String, i64)>> {
    let rows = db.query_all(Statement::from_string(db.get_database_backend(), "SELECT record_id, body, revision FROM checkpoint_evolution WHERE kind IN ('owned_judge_job','owned_judge_budget','owned_judge_pending') ORDER BY record_id LIMIT 17")).await?;
    ensure!(rows.len() <= 16);
    rows.into_iter()
        .map(|row| {
            Ok((
                row.try_get("", "record_id")?,
                row.try_get("", "body")?,
                row.try_get("", "revision")?,
            ))
        })
        .collect()
}
#[tokio::test]
async fn killed_file_backed_paid_owner_cannot_restart_or_replay_its_uncertain_provider()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("owned.db");
    let provider = Provider::start(false).await?;
    let mut judge = Judge::new(true).await?;
    let mut child = process::Child::start(
        CHILD,
        root.path(),
        "crash",
        Some((&provider.url, &judge.url)),
    )?;
    let observed = async {
        judge.receive().await?;
        child.ready().await?;
        file_held(&path)?;
        ensure!(judge.calls.load(Ordering::SeqCst) == 1);
        Ok(())
    }
    .await;
    let killed = child.finish(true, observed).await;
    judge.release.cancel();
    let checked = async {
        killed?;
        let owner = crate::tenancy::ownership::acquire_path(&path, false)?;
        let db =
            crate::db::connect_tenancy(&format!("sqlite://{}", path.display()), &owner).await?;
        let before = retained(&db).await?;
        ensure!(
            before.len() == 3,
            "accepted job/budget/pending not committed before death"
        );
        let snapshot = store::load(&db, Utc::now()).await?;
        let resources = Resources::with_key(snapshot.installation.clone(), [29; 32])?;
        let attempt = Service::start_prepared(
            db.clone(),
            Some(owner.clone()),
            OperationLimits::default(),
            snapshot,
            Arc::new(resources),
        )
        .await;
        let rejected = match attempt {
            Ok(service) => {
                let stopped = service.shutdown().await;
                stopped?;
                Err(anyhow::anyhow!("interrupted job produced readiness"))
            }
            Err(_) => Ok(()),
        };
        let after = retained(&db).await;
        let closed = db.close().await;
        drop(owner);
        rejected?;
        ensure!(
            after? == before,
            "restart repaired/refunded/replayed the interrupted job"
        );
        closed?;
        ensure!(judge.calls.load(Ordering::SeqCst) == 1 && provider.calls() == 0);
        drop(crate::tenancy::ownership::acquire_path(&path, false)?);
        Ok(())
    }
    .await;
    let judge_stop = judge.stop().await;
    let provider_stop = provider.finish().await;
    checked.and(judge_stop).and(provider_stop)
}
