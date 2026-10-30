//! Corrupt a real accepted job's parent projection; do not invent human authority.
use super::*;
use sea_orm::sea_query::{Alias, Expr, Query};
const CHILD: &str = "tenancy::tests::execution::native::capture::learning::qualification::integrity::wrong_parent_child";
async fn exercise(path: &Path) -> Result<()> {
    let provider = Provider::start(false).await?;
    let mut judge = Judge::new(true).await?;
    let ready = file_ready(path, &provider.url, &judge.url).await?;
    let (a, ap) = ready.port_pair().await?;
    let (b, bp) = ready.port_pair().await?;
    ensure!(a.association().session_key(native())? != b.association().session_key(native())?);
    let caller = ap.clone();
    let task = AbortOnDropHandle::new(tokio::spawn(
        async move { caller.checkpoint(native()).await },
    ));
    judge.receive().await?;
    let jobs = bodies(&ready.fixture, "owned_judge_job").await?;
    ensure!(jobs.len() == 1 && jobs[0]["status"] == "accepted");
    let job = jobs[0]["job"].as_str().context("accepted job")?;
    let execution = jobs[0]["execution"]
        .as_str()
        .context("execution")?
        .try_into()?;
    let backend = ready.fixture.db.get_database_backend();
    // Test-only persisted corruption: two real same-tenant parents exist. The
    // immutable body and pending index still name A; only the SQL projection is
    // changed to B. No Scope, grant, proof, evaluator output or human revision is fabricated.
    let update = Query::update()
        .table(Alias::new("checkpoint_evolution"))
        .value(
            Alias::new("session_key"),
            b.association().session_key(native())?,
        )
        .and_where(Expr::col(Alias::new("record_id")).eq(job))
        .and_where(Expr::col(Alias::new("kind")).eq("owned_judge_job"))
        .to_owned();
    ensure!(
        ready
            .fixture
            .db
            .execute(backend.build(&update))
            .await?
            .rows_affected()
            == 1
    );
    judge.release.cancel();
    tokio::time::timeout(TIMEOUT, async {
        while ready.fixture.service.current().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    // Receipt ACK precedes the failed assessment completion participant.
    let receipt = crate::tenancy::receipts::store::lookup(
        &ready.fixture.db,
        &ready.fixture.installation,
        &ready.fixture.tenant,
        &execution,
    )
    .await?
    .context("known actual provider receipt")?;
    ensure!(
        receipt.attempts.len() == 1
            && receipt.attempts[0].usage.input_tokens == Some(4)
            && receipt.attempts[0].usage.output_tokens == Some(2)
    );
    ensure!(
        bodies(&ready.fixture, "owned_judge_job").await? == jobs,
        "failed completion changed immutable body"
    );
    ensure!(bodies(&ready.fixture, "owned_judge_pending").await?.len() == 1);
    ensure!(count(&ready.fixture, "acp_assessment_revisions").await? == 0);
    ensure!(judge.calls.load(Ordering::SeqCst) == 1 && provider.calls() == 0);
    ensure!(
        ready
            .fixture
            .service
            .admission
            .wait_drained()
            .now_or_never()
            .is_none()
    );
    // Only advance after the real failed participant returned/rolled back. This
    // times out the original waiter, not a live SQL operation or a fake rollback.
    ensure!(ready.fixture.service.cleanup.disposition() == Disposition::RolledBack);
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(91)).await;
    tokio::time::resume();
    ensure!(tokio::time::timeout(TIMEOUT, task).await??.is_err());
    let first = tokio::time::timeout(Duration::from_secs(3), ap.finish()).await;
    let second = tokio::time::timeout(Duration::from_secs(3), bp.finish()).await;
    let shutdown =
        tokio::time::timeout(Duration::from_secs(3), ready.fixture.service.shutdown()).await;
    let closed = tokio::time::timeout(TIMEOUT, ready.fixture.db.clone().close()).await;
    let judge_stop = judge.stop().await;
    let provider_stop = provider.finish().await;
    let _capture_results = (first, second);
    ensure!(
        !matches!(shutdown, Ok(Ok(()))),
        "unsettled accepted job reported clean"
    );
    closed??;
    judge_stop?;
    provider_stop?;
    Ok(())
}
#[tokio::test(flavor = "current_thread")]
#[ignore = "bounded wrong-parent completion child, real file DB and actual provider"]
async fn wrong_parent_child() -> Result<()> {
    let root = PathBuf::from(std::env::var_os(process::ROOT).context("owned root")?);
    // Parent has the wall-clock kill/reap deadline. An outer Tokio deadline
    // would incorrectly expire when this child advances the old request clock.
    exercise(&root.join("owned.db")).await?;
    process::witness_and_wait().await
}
#[tokio::test]
async fn wrong_persisted_parent_rejects_paid_completion_and_retains_known_usage_and_owner()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("owned.db");
    let mut child = process::Child::start(CHILD, root.path(), "wrong-parent", None)?;
    let checked = async {
        child.ready().await?;
        file_held(&path)
    }
    .await;
    child.finish(false, checked).await?;
    drop(crate::tenancy::ownership::acquire_path(&path, false)?);
    Ok(())
}
