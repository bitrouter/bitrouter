//! Full-profile producer for the common bootstrap cross-feature regression.
use super::*;
use crate::tenancy::tests::learning_startup::{MODE, ROOT, Transfer, WITNESS};
use std::io::Write;
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
#[ignore = "only the bounded cross-feature parent runs and kills this isolated producer"]
async fn full_producer() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(ROOT).context("startup file directory")?);
    let mode = std::env::var(MODE)?;
    ensure!(matches!(mode.as_str(), "clean" | "pending"));
    let path = root.join(format!("{mode}.db"));
    let owner = crate::tenancy::ownership::acquire_path(&path, true)?;
    let db = crate::db::connect_tenancy(&format!("sqlite://{}?mode=rwc", path.display()), &owner)
        .await?;
    let fixture = Fixture::with_database(db, OperationLimits::default(), &mut |_| {}).await?;
    let provider = Provider::start(false).await?;
    let mut judge = Judge::new(true).await?;
    let ready = Ready::from_fixture(
        fixture,
        Some(owner),
        &provider.url,
        &judge.url,
        true,
        LearningMode::Observe,
        1,
    )
    .await?;
    ensure!(std::fs::metadata(&path)?.len() > 0);
    ensure!(
        ready
            .fixture
            .service
            .current()
            .map_err(failure)?
            .active_configurations
            .is_empty()
    );
    let transfer = Transfer {
        installation: ready.fixture.installation.clone(),
        tenant: ready.fixture.tenant.clone(),
        revision: ready.revision.clone(),
    };
    std::fs::write(path.with_extension("json"), serde_json::to_vec(&transfer)?)?;
    if mode == "clean" {
        let stopped = ready.fixture.service.shutdown().await;
        let closed = ready.fixture.db.clone().close().await;
        let judge_stop = judge.stop().await;
        let provider_stop = provider.finish().await;
        stopped
            .and(closed.map_err(Into::into))
            .and(judge_stop)
            .and(provider_stop)?;
        drop(ready);
        println!("\n{WITNESS}");
        std::io::stdout().flush()?;
        return Ok(());
    }
    let port = ready.port().await?;
    let work = port.clone();
    let task = AbortOnDropHandle::new(tokio::spawn(async move { work.checkpoint(native()).await }));
    judge.receive().await?; // A real paid send follows actor-acknowledged acceptance.
    let jobs = bodies(&ready.fixture, "owned_judge_job").await?;
    ensure!(jobs.len() == 1 && jobs[0]["status"] == "accepted");
    ensure!(bodies(&ready.fixture, "owned_judge_pending").await?.len() == 1);
    ensure!(
        judge.calls.load(Ordering::SeqCst) == 1 && provider.calls() == 0 && !task.is_finished()
    );
    println!("\n{WITNESS}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    tokio::time::timeout(
        Duration::from_secs(30),
        BufReader::new(tokio::io::stdin().take(32)).read_line(&mut line),
    )
    .await??;
    anyhow::bail!("pending producer must be killed while actual accepted work is held")
}
