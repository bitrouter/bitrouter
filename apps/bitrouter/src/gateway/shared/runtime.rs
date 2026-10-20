//! Official shared-mode foreground host. All roots belong to one lifecycle.

mod health;
mod lifecycle;
mod publication;
mod workers;

#[cfg(all(test, unix))]
mod process_tests;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use tokio::net::TcpListener;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::authority::Authority;
use super::crypto::DeploymentKey;
use super::engine::Engine;
use super::generations::Generations;
use super::operator_service::{self, auth::Auth};
use super::store::{self, Store};
use super::telemetry::Telemetry;
use super::transport::owned::Transport;
use crate::config::shared_process::Process;
use health::Health;
use lifecycle::Lifecycle;
use workers::Workers;

/// This entry never consults local daemon, home, policy or credential discovery.
pub(crate) async fn serve(process: Process) -> Result<()> {
    let mut signals = Signals::new()?;
    let key = DeploymentKey::from_environment(&process.shared.deployment_key_env)?;
    let operator_auth = process
        .operator
        .as_ref()
        .map(Auth::new)
        .transpose()?
        .map(Arc::new);
    let timeout = Duration::from_millis(process.shared.transaction_timeout_ms);
    let main = store::connect(
        &process.database_url,
        process.shared.pool_connections - 1,
        timeout,
    )
    .await?;
    let result = async {
        let heartbeat = store::connect(&process.database_url, 1, timeout).await?;
        let result = serve_with_pools(
            &process,
            main.clone(),
            heartbeat.clone(),
            key,
            operator_auth,
            &mut signals,
        )
        .await;
        let closed = heartbeat.close().await;
        result?;
        closed.context("closing shared heartbeat pool")?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let closed = main.close().await;
    result?;
    closed.context("closing shared main pool")?;
    Ok(())
}

async fn serve_with_pools(
    process: &Process,
    main: sea_orm::DatabaseConnection,
    heartbeat: sea_orm::DatabaseConnection,
    key: DeploymentKey,
    operator_auth: Option<Arc<Auth>>,
    signals: &mut Signals,
) -> Result<()> {
    let authority = Authority::new(
        Store::new(
            main,
            &process.shared.namespace,
            Duration::from_millis(process.shared.transaction_timeout_ms),
        )?,
        key,
    );
    // Join never initializes or republishes a namespace. Prove the published
    // authority/keyring is readable before creating the process incarnation.
    let head = authority
        .head()
        .await
        .context("loading initialized shared namespace")?;
    ensure!(
        head.config_generation > 0,
        "shared namespace has no published configuration"
    );
    authority.keys(&head).await?;
    let joined_at = Instant::now();
    let engine = Engine::join(authority.clone(), process.shared.owner_lease_ms).await?;
    let telemetry = match Telemetry::new(process.telemetry.clone()) {
        Ok(telemetry) => Arc::new(telemetry),
        Err(error) => {
            if engine.drain().await.is_err() {
                tracing::warn!("failed startup owner withdrawal is unconfirmed");
            }
            return Err(error);
        }
    };
    let generations = match Generations::new(engine.clone(), &process.shared, telemetry.clone()) {
        Ok(generations) => Arc::new(generations),
        Err(error) => {
            if engine.drain().await.is_err() {
                tracing::warn!("failed startup owner withdrawal is unconfirmed");
            }
            telemetry.force_join().await?;
            return Err(error);
        }
    };
    let health = Health::new(
        generations.serving().clone(),
        Duration::from_millis(
            process.shared.worker_interval_ms + process.shared.transaction_timeout_ms,
        ),
        joined_at + Duration::from_millis(process.shared.owner_lease_ms),
    );
    let heartbeat = match engine.heartbeat_handle(heartbeat) {
        Ok(heartbeat) => heartbeat,
        Err(error) => {
            generations.withdraw();
            if engine.drain().await.is_err() {
                tracing::warn!("failed startup owner withdrawal is unconfirmed");
            }
            telemetry.force_join().await?;
            return Err(error);
        }
    };
    let workers = Workers::new(heartbeat, health.clone(), process);
    let mut lifecycle = Lifecycle::new(engine, generations, workers, health, telemetry.clone());
    let result = async {
        #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
        if process.telemetry.is_some() {
            // Match the local host's propagator without local attribution I/O.
            opentelemetry::global::set_text_map_propagator(opentelemetry_sdk::propagation::TraceContextPropagator::new());
        }
        crate::host::init_serve_tracing_subscriber(
            #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
            telemetry.exporter().as_deref(), &process.server.log_level)?;
        if let Some(config) = &process.operator {
            let auth = operator_auth.clone().context("operator authentication snapshot missing")?;
            let listener = tokio::select! {
                biased;
                signal = signals.recv() => { signal?; return Ok(()); },
                worker = lifecycle.workers.wait_any() => anyhow::bail!("shared {worker} worker exited during operator binding"),
                listener = TcpListener::bind(config.listen) => listener.context("binding shared operator listener")?,
            };
            lifecycle.operator_transport = Some(Transport::start(listener,
                operator_service::router(authority.clone(), auth, Duration::from_millis(process.shared.request_timeout_ms)),
                super::transport::Limits::for_operator(&process.shared, config.max_connections))?);
        }
        let initial = publication::initial(&authority, process, &lifecycle.generations, &lifecycle.health);
        tokio::select! {
            biased;
            signal = signals.recv() => { signal?; return Ok(()); },
            worker = lifecycle.workers.wait_any() => anyhow::bail!("shared {worker} worker exited during startup"),
            result = wait_operator(&lifecycle.operator_transport) => { result?; anyhow::bail!("shared operator accept task exited during startup"); },
            result = initial => result?,
        }
        let listener = tokio::select! {
            biased;
            signal = signals.recv() => { signal?; return Ok(()); },
            worker = lifecycle.workers.wait_any() => anyhow::bail!("shared {worker} worker exited during binding"),
            result = wait_operator(&lifecycle.operator_transport) => { result?; anyhow::bail!("shared operator accept task exited during binding"); },
            listener = TcpListener::bind(&process.server.listen) => listener.context("binding shared gateway listener")?,
        };
        lifecycle.transport = Some(Transport::start(listener, lifecycle.generations.serving().router(), (&process.shared).into())?);
        lifecycle.workers.start_processing(authority, process.clone(), lifecycle.generations.clone(),
            lifecycle.health.clone(), lifecycle.engine.clone())?;
        tracing::info!(listen = %process.server.listen, namespace = %process.shared.namespace, "shared gateway ready for authority checks");
        let transport = lifecycle.transport.as_ref().context("shared transport missing")?;
        tokio::select! {
            biased;
            signal = signals.recv() => signal,
            worker = lifecycle.workers.wait_any() => anyhow::bail!("shared {worker} worker exited"),
            result = wait_operator(&lifecycle.operator_transport) => { result?; anyhow::bail!("shared operator accept task exited"); },
            result = transport.wait_driver() => { result?; anyhow::bail!("shared accept task exited"); },
        }
    }.await;
    // A second signal selects force, but the same stored owner keeps driving
    // joins. No signal listener or shutdown future owns a task handle.
    let deadline = Instant::now() + Duration::from_millis(process.shared.drain_timeout_ms);
    let force = CancellationToken::new();
    let report = {
        let wait = lifecycle.shutdown_until(deadline, &force);
        tokio::pin!(wait);
        tokio::select! {
            report = &mut wait => report?,
            signal = signals.recv() => {
                if signal.is_err() { tracing::warn!("shared signal listener stopped during shutdown"); }
                force.cancel();
                wait.await?
            },
        }
    };
    tracing::info!(forced = report.forced, withdrawal = ?report.withdrawal,
        quiescent = report.quiescent, durable_resolved = report.durable_resolved, clean = report.clean,
        "shared gateway shutdown completed");
    result?;
    ensure!(
        report.clean,
        "shared gateway stopped with interrupted or unresolved work"
    );
    Ok(())
}

async fn wait_operator(transport: &Option<Transport>) -> Result<()> {
    match transport {
        Some(transport) => transport.wait_driver().await.map(|_| ()),
        None => std::future::pending().await,
    }
}

struct Signals {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(windows)]
    interrupt: tokio::signal::windows::CtrlC,
    #[cfg(windows)]
    terminate: tokio::signal::windows::CtrlShutdown,
    #[cfg(windows)]
    interrupt_break: tokio::signal::windows::CtrlBreak,
    #[cfg(windows)]
    close: tokio::signal::windows::CtrlClose,
}

impl Signals {
    fn new() -> Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                interrupt: signal(SignalKind::interrupt())?,
                terminate: signal(SignalKind::terminate())?,
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                interrupt: tokio::signal::windows::ctrl_c()?,
                terminate: tokio::signal::windows::ctrl_shutdown()?,
                interrupt_break: tokio::signal::windows::ctrl_break()?,
                close: tokio::signal::windows::ctrl_close()?,
            })
        }
    }

    async fn recv(&mut self) -> Result<()> {
        #[cfg(unix)]
        tokio::select! {
            signal = self.interrupt.recv() => signal.context("shared interrupt listener closed"),
            signal = self.terminate.recv() => signal.context("shared termination listener closed"),
        }
        #[cfg(windows)]
        tokio::select! {
            signal = self.interrupt.recv() => signal.context("shared interrupt listener closed"),
            signal = self.terminate.recv() => signal.context("shared termination listener closed"),
            signal = self.interrupt_break.recv() => signal.context("shared break listener closed"),
            signal = self.close.recv() => signal.context("shared console-close listener closed"),
        }
    }
}
