//! Real transaction failures in actual HTTP publication, isolated to children.
//! Eventual SQLite cleanup cannot substitute for the owning H acknowledgement.
use super::*;
use crate::tenancy::{
    Service,
    resources::Resources,
    transaction::{Disposition, Phase},
};
use std::{io::Write, path::Path, process::Stdio};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

const DATABASE: &str = "BITROUTER_TEST_HTTP034_UNKNOWN_DATABASE";
const FAULT: &str = "BITROUTER_TEST_HTTP034_UNKNOWN_FAULT";
const RETAINED: &str = "http034-unknown-owner-retained";

#[derive(Clone, Copy)]
enum Fault {
    Begin,
    Rollback,
    Unwind,
}
impl Fault {
    fn name(self) -> &'static str {
        match self {
            Self::Begin => "begin",
            Self::Rollback => "rollback",
            Self::Unwind => "unwind",
        }
    }
    fn parse(value: &str) -> Result<Self> {
        match value {
            "begin" => Ok(Self::Begin),
            "rollback" => Ok(Self::Rollback),
            "unwind" => Ok(Self::Unwind),
            _ => anyhow::bail!("unknown HTTP custody fault"),
        }
    }
    fn phase(self) -> Phase {
        match self {
            Self::Begin => Phase::Beginning,
            Self::Rollback => Phase::RollingBack,
            Self::Unwind => Phase::Active,
        }
    }
}

async fn owned_ready(
    path: &Path,
    provider: &Server,
) -> Result<(Ready, sea_orm::DatabaseConnection)> {
    let owner = crate::tenancy::ownership::acquire_path(path, true)?;
    let db = crate::db::connect_tenancy(&format!("sqlite://{}", path.display()), &owner).await?;
    let fixture = Fixture::with_database(db.clone(), Default::default(), &mut |_| {}).await?;
    let (mut fixture, route, _) = super::super::provision_protocol_with_fixture(
        fixture,
        &provider.url,
        "responses",
        &mut |_| {},
    )
    .await?;
    fixture.service.shutdown().await?;
    let snapshot = crate::tenancy::store::load(&db, chrono::Utc::now()).await?;
    fixture.service = Service::start_prepared(
        db.clone(),
        Some(owner),
        Default::default(),
        snapshot,
        Arc::new(Resources::with_key(fixture.installation.clone(), [29; 32])?),
    )
    .await?;
    let revision = draft(&fixture, &route, "http-unknown-draft", 32).await?;
    let issued = fixture.issue("http-unknown-caller").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let headers = headers(
        &fixture,
        issued.secret.as_deref().context("caller")?,
        &revision.revision_id,
    )?;
    Ok((
        Ready {
            fixture,
            headers,
            route,
            revision,
        },
        db,
    ))
}

async fn exercise(path: &Path, fault: Fault) -> Result<()> {
    let mut provider = Server::start().await?;
    let (ready, db) = owned_ready(path, &provider).await?;
    if matches!(fault, Fault::Rollback) {
        // SQLite itself rolls back the transaction. SeaORM still consumes its
        // open transaction in Control::rollback, whose real SQL ROLLBACK fails.
        db.execute_unprepared("CREATE TRIGGER http034_consuming_rollback BEFORE INSERT ON provider_continuations BEGIN SELECT RAISE(ROLLBACK, 'http034 rollback fault'); END").await?;
    }
    let release = Arc::new(tokio::sync::Notify::new());
    *provider
        .state
        .pause
        .lock()
        .map_err(|_| anyhow::anyhow!("provider pause"))? = Some(release.clone());
    let mut response = Box::pin(send_response(
        &ready,
        ready.headers.clone(),
        body(false, None),
    ));
    tokio::time::timeout(WAIT, async {
        tokio::select! {
            result = &mut response => { let _ = result?; anyhow::bail!("request ended before actual provider send"); }
            wire = provider.wire() => { wire?; Ok::<_, anyhow::Error>(()) }
        }
    }).await??;
    if matches!(fault, Fault::Begin) {
        // The tenancy SQLite pool has exactly one connection. Raw BEGIN does
        // not advance SQLx's transaction depth; the following real ORM BEGIN
        // fails inside SeaORM's already-constructed transaction. This is not a
        // typed pool-acquire error, and no test-owned ORM transaction is held.
        db.execute_unprepared("BEGIN").await?;
    }
    // Credential/admission reads completed before the provider accepted work.
    // The next transaction belongs to this actual HTTP Prepare operation.
    let gate = ready.fixture.service.cleanup.hold(fault.phase());
    release.notify_one();
    let entered = tokio::time::timeout(WAIT, async {
        tokio::select! {
            result = &mut response => { let _ = result?; anyhow::bail!("publication bypassed expected H phase"); }
            () = gate.entered.notified() => Ok::<_, anyhow::Error>(()),
        }
    }).await;
    if matches!(fault, Fault::Unwind) {
        ready.fixture.service.cleanup.unwind_gate()?;
    } else {
        ready.fixture.service.cleanup.release_gate();
    }
    entered??;
    let (status, _) = tokio::time::timeout(WAIT, &mut response).await??;
    drop(response);
    ensure!(!status.is_success(), "unknown publication reported success");
    ensure!(
        provider.state.calls.load(Ordering::SeqCst) == 1,
        "fault retried provider"
    );
    tokio::time::timeout(WAIT, ready.fixture.service.cleanup.quarantined()).await?;
    ensure!(ready.fixture.service.cleanup.disposition() == Disposition::Ambiguous);
    ensure!(ready.fixture.service.cleanup.ensure_clean().is_err());
    ensure!(ready.fixture.service.current().is_err());
    let tickets = ready
        .fixture
        .service
        .http_continuation
        .state
        .pending_tickets()
        .map_err(|_| anyhow::anyhow!("pending custody"))?;
    ensure!(
        tickets.len() == 1,
        "unknown publication lost its generation"
    );
    ensure!(
        ready
            .fixture
            .service
            .http_continuation
            .state
            .matches(tickets.first().context("ticket")?)
    );
    // The user-visible error is not an acknowledged completion and cannot
    // free its execution admission. No clean exception applies to these faults.
    use futures::FutureExt;
    ensure!(
        ready
            .fixture
            .service
            .admission
            .wait_drained()
            .now_or_never()
            .is_none()
    );
    ensure!(row_count(&ready).await? == 0);
    // Remove all normal owners and close the physical pool before the parent
    // probes the lock. Only the quarantined authority/pipeline custody remains.
    let proof = ready.fixture.service.cleanup.clone();
    drop((tickets, ready));
    tokio::time::timeout(WAIT, db.close()).await??;
    ensure!(proof.disposition() == Disposition::Ambiguous);
    ensure!(proof.ensure_clean().is_err());
    drop(proof);
    provider.close().await?;
    Ok(())
}

#[tokio::test]
#[ignore = "invoked by the owner-retention parent with an isolated file database"]
async fn actual_http_unknown_owner_child() -> Result<()> {
    let path = std::env::var_os(DATABASE).context("isolated database")?;
    let fault = Fault::parse(&std::env::var(FAULT)?)?;
    tokio::time::timeout(Duration::from_secs(20), exercise(Path::new(&path), fault)).await??;
    // Serial libtest prints its test-name prefix without a newline.
    println!("\n{RETAINED}:{}", fault.name());
    std::io::stdout().flush()?;
    let mut input = BufReader::new(tokio::io::stdin().take(32));
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(20), input.read_line(&mut line)).await??;
    Ok(())
}

async fn retained_owner(fault: Fault) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("http034-unknown.sqlite");
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "tenancy::tests::execution::http_continuation::unknown::actual_http_unknown_owner_child",
            "--ignored", "--nocapture", "--test-threads=1"])
        .env(DATABASE, &database).env(FAULT, fault.name())
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .kill_on_drop(true).spawn()?;
    let stdout = child.stdout.take().context("child stdout")?;
    let mut stdout = BufReader::new(stdout.take(8193));
    let stderr = child.stderr.take().context("child stderr")?;
    let diagnostics = AbortOnDropHandle::new(tokio::spawn(async move {
        let mut stderr = stderr.take(16_385);
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await?;
        Ok::<_, std::io::Error>(bytes)
    }));
    let expected = format!("{RETAINED}:{}", fault.name());
    tokio::time::timeout(Duration::from_secs(25), async {
        let mut total = 0;
        loop {
            let mut line = String::new();
            let count = stdout.read_line(&mut line).await?;
            ensure!(
                count != 0,
                "child ended before actual unknown-owner acknowledgement"
            );
            total += count;
            ensure!(total <= 8192, "bounded child diagnostics exceeded");
            if line.trim() == expected {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await??;
    match crate::tenancy::ownership::acquire_path(&database, false) {
        Err(error) => ensure!(
            matches!(
                error.downcast_ref::<std::fs::TryLockError>(),
                Some(std::fs::TryLockError::WouldBlock)
            ),
            "owner probe failed for an unrelated reason"
        ),
        Ok(owner) => {
            drop(owner);
            anyhow::bail!("unknown transaction released the authority lease");
        }
    }
    drop(child.stdin.take());
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait()).await??;
    let stderr = tokio::time::timeout(Duration::from_secs(5), diagnostics).await???;
    ensure!(stderr.len() <= 16_384, "bounded child stderr exceeded");
    ensure!(
        status.success(),
        "HTTP unknown child failed: {status}: {}",
        String::from_utf8_lossy(&stderr)
    );
    drop(stdout);
    drop(crate::tenancy::ownership::acquire_path(&database, false)?);
    Ok(())
}

#[tokio::test]
async fn actual_http_begin_error_keeps_owner_after_all_normal_handles_close() -> Result<()> {
    retained_owner(Fault::Begin).await
}

#[tokio::test]
async fn actual_http_consuming_rollback_error_keeps_owner_after_all_normal_handles_close()
-> Result<()> {
    retained_owner(Fault::Rollback).await
}

#[tokio::test]
async fn actual_http_caught_unwind_keeps_owner_after_all_normal_handles_close() -> Result<()> {
    retained_owner(Fault::Unwind).await
}
