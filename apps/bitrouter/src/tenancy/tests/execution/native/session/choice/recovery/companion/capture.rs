//! A real, even cleanly closed capture connection is not a retained reaper proof.
use super::*;
use crate::{
    acp_cli::LaunchOptions,
    config::agent::{AcpAgentConfig, AcpTransport},
    harness::acp::controlled::{ControlledSession, LocalControllerLaunch},
};

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn native_recovery_refuses_actual_capture_history_without_retained_reaper_proof() -> Result<()>
{
    campaign(&["profile-capture"]).await
}

pub(super) async fn case(root: &Path) -> Result<()> {
    let program = PathBuf::from(std::env::var_os("BITROUTER_OWNED_CAPTURE_FIXTURE").context(
        "set BITROUTER_OWNED_CAPTURE_FIXTURE to the separately built owned_capture_fixture",
    )?);
    ensure!(program.is_absolute() && program.is_file());
    let stopped = Stopped::produce(root, "profile-capture").await?;
    let app = Arc::new(
        host_app(
            &stopped.bound.fixture,
            &stopped.generation.url,
            Arc::new(HostSpies::default()),
        )?
        .with_tenancy(stopped.bound.fixture.service.clone()),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let origin = format!("http://{}", listener.local_addr()?);
    let stop = CancellationToken::new();
    let done = stop.clone();
    let served = app.clone();
    let server = AbortOnDropHandle::new(tokio::spawn(async move {
        served
            .serve_listener_with_router_wrapper_and_shutdown(
                listener,
                |router| router,
                done.cancelled_owned(),
            )
            .await
    }));
    let marker = root.join("capture-child.json");
    let result = async {
        let launch = LocalControllerLaunch {
            source: "recovery-capture-refusal".into(), listener_origin: origin,
            options: LaunchOptions::default(),
            agent: AcpAgentConfig { name: "recovery-capture-refusal".into(),
                transport: AcpTransport::Stdio { command: program.to_string_lossy().into_owned(),
                    args: vec![], env: std::collections::HashMap::from([(
                        "CAPTURE_FIXTURE_MARKER".into(), marker.to_string_lossy().into_owned())]) } },
        };
        let capture = tokio::time::timeout(TIMEOUT, ControlledSession::open_authenticated(
            app, stopped.bound.headers.clone(), launch, CancellationToken::new())).await??;
        tokio::time::timeout(TIMEOUT, capture.shutdown()).await??;
        let child: Value = serde_json::from_slice(&std::fs::read(&marker)?)?;
        let pid = rustix::process::Pid::from_raw(i32::try_from(child["pid"].as_u64().context("actual peer PID")?)?)
            .context("valid peer PID")?;
        ensure!(rustix::process::test_kill_process_group(pid) == Err(rustix::io::Errno::SRCH));
        let row = stopped.bound.fixture.db.query_one(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS n FROM acp_capture_connections WHERE installation_id=? AND tenant_id=?",
            [stopped.bound.fixture.installation.as_str().into(),stopped.bound.fixture.tenant.as_str().into()]))
            .await?.context("actual capture history")?;
        ensure!(row.try_get::<i64>("","n")? == 1);
        let before = payload(&stopped.bound.fixture.db).await?;
        let receipts = terminals(&stopped.bound.fixture.db).await?;
        no_write(&stopped, root, &before, &receipts).await
    }.await;
    stop.cancel();
    let joined = tokio::time::timeout(TIMEOUT, server).await;
    let closed = stopped.close(result).await;
    closed?;
    joined???;
    Ok(())
}
