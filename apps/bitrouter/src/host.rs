//! Complete foreground daemon lifecycle for compiled extension hosts.

use crate::daemon;
use crate::tracing_filter::resolve_env_filter;
use anyhow::{Context, Result};
use std::sync::Arc;

async fn supervise_http_shutdown<Http, Control, Hup, Term>(
    http: Http,
    control: Control,
    hup: Hup,
    term: Term,
    shutdown: tokio::sync::oneshot::Sender<()>,
) -> Result<()>
where
    Http: std::future::Future<Output = Result<()>> + Send,
    Control: std::future::Future<Output = Result<()>> + Send,
    Hup: std::future::Future<Output = Result<()>> + Send,
    Term: std::future::Future<Output = Result<()>> + Send,
{
    let mut http = Box::pin(http);
    let mut control = Box::pin(control);
    let mut hup = Box::pin(hup);
    let mut term = Box::pin(term);
    let mut hup_open = true;
    let trigger_result = loop {
        tokio::select! {
            result = &mut http => return result,
            result = &mut control => break result,
            result = &mut term => {
                if result.is_err() {
                    tracing::warn!(
                        reason = "termination_signal_listener_unavailable",
                        "termination-signal listener unavailable"
                    );
                }
                break Ok(());
            }
            result = &mut hup, if hup_open => {
                if result.is_err() {
                    tracing::warn!(
                        reason = "sighup_listener_unavailable",
                        "SIGHUP listener unavailable"
                    );
                }
                hup_open = false;
            }
        }
    };

    drop(control);
    drop(hup);
    drop(term);
    let _ = shutdown.send(());
    http.await?;
    trigger_result
}

/// Install the full tracing subscriber for the `serve` command: fmt plus
/// — when OTel is configured — the bridge layer that mirrors `tracing`
/// spans into OTel via the supplied exporter's SDK tracer.
///
/// `tracing-opentelemetry`'s bridge layer captures its tracer eagerly,
/// so this MUST be called after [`bitrouter_telemetry::otel::OtelExporter::new`]
/// has built the real exporter; passing `None` (OTel disabled in config)
/// installs the fmt-only registry.
///
/// This is the one path with a config in hand, so it is where
/// `server.log_level` takes effect. Resolution happens once here — a later
/// `bro reload` re-reads the config but cannot re-install the
/// subscriber, so a changed `log_level` needs a restart.
fn init_serve_tracing_subscriber(
    exporter: Option<&bitrouter_telemetry::otel::OtelExporter>,
    config_log_level: &str,
) -> Result<()> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let (env_filter, warning) = resolve_env_filter(Some(config_log_level));
    let registry = tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer());
    match exporter {
        Some(exp) => registry
            .with(bitrouter_telemetry::otel::subscriber::tracing_subscriber_layer(exp))
            .try_init(),
        None => registry.try_init(),
    }
    .map_err(|error| anyhow::anyhow!("installing host tracing subscriber: {error}"))?;
    if let Some(warning) = warning {
        tracing::warn!("{warning}");
    }
    Ok(())
}

/// Run the product foreground host with compiled extension registrations.
///
/// Registration runs once before database assembly or listener binding. This
/// owns the process working directory, tracing subscriber and signal handlers;
/// run one host per process. Configuration, management, reload and shutdown use
/// the same path as `bro serve`. Detached launch/restart is the caller's job.
pub async fn serve_with_extensions(
    source: &crate::paths::ConfigSource,
    register: impl FnOnce(&mut bitrouter_sdk::extension::ExtensionApi) -> Result<()>,
) -> Result<()> {
    if let Some(located) = crate::daemon_locator::locate_source(source).await? {
        anyhow::bail!(
            "bitrouter is already running (pid {}); stop it before launching this executable again",
            located.pid()
        );
    }
    // Ensure the bitrouter home directory exists (zero-config first-run
    // creates `~/.bitrouter` on demand) and chdir into it. Every
    // relative path in the config — `database.url`,
    // `server.control_socket`, policy / agent / mcp file references —
    // then interprets relative to one stable location instead of
    // whichever CWD the launcher happened to be in. The daemon's
    // runtime artefacts (db, socket, pid, log) all land in the home.
    let home = source.home();
    crate::paths::ensure_home_directory(home)?;
    std::env::set_current_dir(home)
        .with_context(|| format!("chdir to bitrouter home {}", home.display()))?;

    let startup_configuration = crate::reload::load_configuration_baseline(source).await?;
    let mut cfg = startup_configuration.config().clone();
    // Auto-enable the `claude-code` subscription provider when the user has
    // signed in (a `claude-code` credential is in the OAuth store). Runs before
    // the registry merge so the merge fills the inserted provider's
    // `api_base` / `api_protocol` / auth from the fetched registry entry.
    crate::claude_code::enable_if_logged_in(&mut cfg);
    // Fetch + merge the public provider registry before assembly, so the daemon
    // routes every credentialed provider's registered models. Best-effort and
    // cache-backed; a no-op when disabled or unreachable with no cache.
    crate::merge_registry_into(&mut cfg).await;
    announce_zero_config(source, &cfg);
    maybe_announce_telemetry(home);
    let listen = cfg.server.listen.clone();
    // For a `File` source the socket is resolved against the config file's
    // directory (preserves any user override); for `Default` it lives at
    // `<home>/bitrouter.sock`. Shared with `start`/`spawn` via `socket_path_for`.
    let socket_path = daemon::socket_path_for(source, &cfg);
    let pid_path = socket_path.with_extension("pid");
    let remote_control = crate::remote_control::ControlServer::from_config(
        &cfg.control,
        source.clone(),
        socket_path.clone(),
    )?;

    let config_path_for_reload = match source {
        crate::paths::ConfigSource::File(path) => Some(path.as_path()),
        crate::paths::ConfigSource::Default { .. } => None,
    };
    let assembled =
        crate::assemble::build_app_with_extensions(&cfg, config_path_for_reload, register).await?;
    let observe_for_shutdown = assembled.observe.clone();
    let trajectory_outbox_for_shutdown = assembled.trajectory_outbox_publisher.clone();
    // Every failure after assembly follows the same flush/drain path, including
    // listener conflicts and locator/PID publication failures.
    let mut pid_guard = None;
    let result = async {
        // The OTel exporter was just constructed (inside `build_app_with_path`).
        // Hand its SDK tracer to the `tracing-opentelemetry` bridge layer now
        // — the bridge captures its tracer at construction, so this can only
        // happen after the exporter exists.
        init_serve_tracing_subscriber(assembled.otel_exporter.as_deref(), &cfg.server.log_level)?;
        // Surface any deferred OTel-init failure now that the subscriber is up.
        if let Some(msg) = &assembled.otel_init_error {
            tracing::error!("{msg}");
        }
        // Same deferral, same reason: assembly runs before the subscriber, so
        // these are collected there and emitted here. Without this the guard is
        // silent on exactly the path it exists for.
        for msg in &assembled.ignored_config {
            tracing::warn!("{msg}");
        }
        let workflow_trace_capture =
            crate::workflow_state::real_trace::capture_from_env().map_err(anyhow::Error::from)?;
        if workflow_trace_capture.is_some() {
            tracing::info!(
                env = crate::workflow_state::real_trace::WORKFLOW_TRACE_JSONL_ENV,
                "workflow trace capture enabled"
            );
        }
        let app = Arc::new(assembled.app);
        let eval_router = crate::eval::api::router(
            assembled.eval_service.clone(),
            assembled.db.clone(),
            cfg.server.skip_auth,
        );
        let policy_store = assembled.policy_store;
        // Clone before moving the original into `run_control_socket` — we
        // need a handle here too so the shutdown path below can drive the
        // exporter flush before the runtime tears down.
        let observe_provider = assembled.observe;
        let reload_source = match source {
            crate::paths::ConfigSource::File(path) => {
                crate::reload::ReloadSource::File(path.clone())
            }
            crate::paths::ConfigSource::Default { .. } => crate::reload::ReloadSource::Default,
        };
        let administration = crate::actions::administration::Administration {
            source: source.clone(),
            routing: assembled.routing_table.clone(),
            policy: assembled.policy_runtime.clone(),
            observe: observe_provider.clone(),
            request_checks: Some(assembled.request_checks.clone()),
        };
        let acp_runtime_for_control = assembled.acp_runtime.clone();
        let reloader = crate::reload::AppReloader::new(
            policy_store.clone(),
            assembled.routing_table,
            assembled.upstream_executor,
            reload_source,
        )
        .with_startup_configuration(startup_configuration)
        .with_policy_runtime(assembled.policy_runtime)
        .with_policy_table_router(assembled.policy_table_router);
        let server_instance_id = daemon::DaemonReloader::reload_state(&reloader)
            .ok_or_else(|| anyhow::anyhow!("daemon reload state is unavailable at startup"))?
            .server_instance_id;
        let reloader: Arc<dyn daemon::DaemonReloader> = Arc::new(reloader);

        let inference_listener = tokio::net::TcpListener::bind(&listen)
            .await
            .with_context(|| format!("bind inference listener {listen}"))?;
        let control_listener = daemon::bind_control_socket(&socket_path).await?;
        let remote_control = match remote_control {
            Some(server) => Some(
                server
                    .with_administration(administration.clone())
                    .with_reloader(reloader.clone())
                    .bind()
                    .await?,
            ),
            None => None,
        };

        pid_guard = Some(PidFile::create(&pid_path).await?);
        let remote_listen = remote_control.as_ref().map(|server| server.listen());
        let ready_listen = listen.clone();

        let http_app = app.clone();
        // The ingress SERVER span is created from the exporter's own tracer — the
        // SDK installs no global `TracerProvider`, so there is nothing to reach
        // for implicitly. With OTel disabled there is no ingress span at all,
        // which is the honest behaviour: the previous `TraceLayer` ran regardless
        // and built `tracing` spans that went nowhere.
        let otel_router_wrapper = assembled
            .otel_exporter
            .as_deref()
            .map(bitrouter_telemetry::otel::http_layer::router_wrapper);
        let (http_shutdown_tx, http_shutdown_rx) = tokio::sync::oneshot::channel();
        let http = async move {
            let (inference_shutdown_tx, inference_shutdown_rx) = tokio::sync::oneshot::channel();
            let (remote_shutdown_tx, remote_shutdown_rx) = tokio::sync::oneshot::channel();
            // Open an OTel SERVER span per inbound request and publish it on the
            // OTel context, so the bitrouter `chat` INTERNAL span parents on it.
            let otel_wrapper = move |router: axum::Router| match &otel_router_wrapper {
                Some(wrapper) => wrapper(router),
                None => router,
            };
            let inference_shutdown = async move {
                let _ = inference_shutdown_rx.await;
            };
            let inference = async move {
                match workflow_trace_capture {
                    Some(capture) => {
                        let workflow_wrapper = capture.router_wrapper();
                        let eval_router = eval_router.clone();
                        http_app
                            .serve_listener_with_router_wrapper_and_shutdown(
                                inference_listener,
                                move |router| {
                                    workflow_wrapper(otel_wrapper(
                                        router.merge(eval_router.clone()),
                                    ))
                                },
                                inference_shutdown,
                            )
                            .await
                    }
                    None => {
                        http_app
                            .serve_listener_with_router_wrapper_and_shutdown(
                                inference_listener,
                                move |router| otel_wrapper(router.merge(eval_router.clone())),
                                inference_shutdown,
                            )
                            .await
                    }
                }
                .map_err(anyhow::Error::from)
            };
            let remote = async move {
                match remote_control {
                    Some(server) => {
                        server
                            .serve_with_shutdown(async move {
                                let _ = remote_shutdown_rx.await;
                            })
                            .await
                    }
                    None => {
                        let _ = remote_shutdown_rx.await;
                        Ok(())
                    }
                }
            };
            let mut inference = Box::pin(inference);
            let mut remote = Box::pin(remote);
            let mut shutdown = Box::pin(async move {
                let _ = http_shutdown_rx.await;
            });

            tokio::select! {
                result = &mut inference => {
                    let _ = remote_shutdown_tx.send(());
                    remote.await?;
                    result
                }
                result = &mut remote => {
                    let _ = inference_shutdown_tx.send(());
                    inference.await?;
                    result
                }
                _ = &mut shutdown => {
                    let _ = inference_shutdown_tx.send(());
                    let _ = remote_shutdown_tx.send(());
                    let (inference_result, remote_result) = tokio::join!(inference, remote);
                    inference_result?;
                    remote_result
                }
            }
        };
        let locator_socket = socket_path.clone();
        let locator_source = source.clone();
        let locator_instance = server_instance_id;
        let control = daemon::run_bound_control_socket(
            control_listener,
            app.clone(),
            listen,
            reloader.clone(),
            observe_provider,
            daemon::AcpControlPlane {
                runtime: acp_runtime_for_control,
                metering: crate::metering::MeteringStore::new(assembled.db.clone()),
                inventory: Some(assembled.evolution.inventory()),
                evolution: Some(assembled.evolution.clone()),
            },
            Some(administration),
        );
        let control = async move {
            let mut control = Box::pin(control);
            loop {
                let probe = crate::daemon_locator::endpoint_matches(
                    &locator_socket,
                    std::process::id(),
                    &locator_instance,
                );
                tokio::pin!(probe);
                let matches = tokio::select! {
                    result = &mut control => return result,
                    matches = &mut probe => matches,
                };
                if matches {
                    let _locator = crate::daemon_locator::publish(
                        &locator_source,
                        &locator_socket,
                        &locator_instance,
                    )?;
                    println!(
                        "bitrouter {} — serving on {ready_listen} (control: {})",
                        crate::VERSION,
                        locator_socket.display()
                    );
                    if let Some(remote_listen) = remote_listen {
                        println!(
                            "bitrouter {} — remote control on {remote_listen}",
                            crate::VERSION
                        );
                    }
                    return control.await;
                }
                tokio::select! {
                    result = &mut control => return result,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
                }
            }
        };

        // SIGHUP triggers a config reload — reload should be available via either
        // `bro reload` (the control endpoint) *or* a HUP signal. Same fan-out
        // as the Reload command — every reloadable subsystem. SIGHUP is Unix-only;
        // on Windows there is no equivalent, so the HUP future stays pending and
        // reload is reached exclusively through `bro reload`.
        let hup_reloader = reloader.clone();
        let hup = async move {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{SignalKind, signal};
                let mut hup = match signal(SignalKind::hangup()) {
                    Ok(s) => s,
                    Err(e) => return Err::<(), anyhow::Error>(anyhow::Error::from(e)),
                };
                loop {
                    if hup.recv().await.is_none() {
                        return Ok(());
                    }
                    match hup_reloader.reload().await {
                        Ok(()) => tracing::info!("SIGHUP — reload succeeded"),
                        Err(e) => tracing::warn!(error = %e, "SIGHUP reload failed"),
                    }
                }
            }
            #[cfg(not(unix))]
            {
                // No SIGHUP on this platform — keep the reloader handle alive and
                // park forever so the `select!` arm below never fires.
                let _keep = &hup_reloader;
                std::future::pending::<()>().await;
                Ok::<(), anyhow::Error>(())
            }
        };

        // Termination signals end the loop the same way `bro stop` does — so
        // the shutdown path below (observe flush, pid-file cleanup) runs in every
        // graceful termination mode. On Unix that's SIGINT (ctrl-C) and SIGTERM
        // (systemd / `kill`); on Windows it's the console control events
        // (Ctrl-C / Ctrl-Break / window close / system shutdown).
        let term = async {
            #[cfg(unix)]
            {
                use tokio::signal::unix::{SignalKind, signal};
                let mut sigint = signal(SignalKind::interrupt()).map_err(anyhow::Error::from)?;
                let mut sigterm = signal(SignalKind::terminate()).map_err(anyhow::Error::from)?;
                tokio::select! {
                    _ = sigint.recv() => tracing::info!("SIGINT — shutting down"),
                    _ = sigterm.recv() => tracing::info!("SIGTERM — shutting down"),
                }
                Ok::<(), anyhow::Error>(())
            }
            #[cfg(windows)]
            {
                use tokio::signal::windows;
                let mut ctrl_c = windows::ctrl_c().map_err(anyhow::Error::from)?;
                let mut ctrl_break = windows::ctrl_break().map_err(anyhow::Error::from)?;
                let mut ctrl_close = windows::ctrl_close().map_err(anyhow::Error::from)?;
                let mut ctrl_shutdown = windows::ctrl_shutdown().map_err(anyhow::Error::from)?;
                tokio::select! {
                    _ = ctrl_c.recv() => tracing::info!("Ctrl-C — shutting down"),
                    _ = ctrl_break.recv() => tracing::info!("Ctrl-Break — shutting down"),
                    _ = ctrl_close.recv() => tracing::info!("console close — shutting down"),
                    _ = ctrl_shutdown.recv() => tracing::info!("system shutdown — shutting down"),
                }
                Ok::<(), anyhow::Error>(())
            }
        };

        // Control/termination requests signal the SDK server and then keep polling
        // that same future until axum and every required finalizer finish. An HTTP
        // failure still returns directly; a HUP listener failure only disables
        // reload signaling and leaves the server running.
        let evolution_stop = tokio_util::sync::CancellationToken::new();
        let evolution_worker = app.language_model().cloned().map(|pipeline| {
            let worker =
                crate::evolution::scheduler::EvolutionScheduler::new(assembled.evolution.clone());
            let stop = evolution_stop.clone();
            tokio::spawn(async move { worker.run(pipeline, stop).await })
        });
        let control = async {
            let result = control.await;
            evolution_stop.cancel();
            result
        };
        let term = async {
            let result = term.await;
            evolution_stop.cancel();
            result
        };
        let result = supervise_http_shutdown(http, control, hup, term, http_shutdown_tx).await;
        evolution_stop.cancel();
        if let Some(worker) = evolution_worker
            && worker.await.is_err()
        {
            tracing::warn!("checkpoint evolution worker ended unexpectedly");
        }

        result
    }
    .await;

    if let Some(publisher) = trajectory_outbox_for_shutdown {
        match publisher.drain_after_active_worker().await {
            Ok(summary) if summary.failed > 0 => tracing::warn!(
                attempted = summary.attempted,
                delivered = summary.delivered,
                failed = summary.failed,
                "trajectory outbox drain completed with poison items still pending"
            ),
            Ok(_) => {}
            Err(_) => tracing::warn!(
                reason = "shutdown_drain_failed",
                "trajectory outbox shutdown drain failed"
            ),
        }
    }

    // Drive the OTel exporter's flush before anything else drops — its
    // `rt-tokio` background tasks need a live async runtime to drain,
    // and `spawn_blocking` (inside the provider's `shutdown`) parks on
    // a dedicated thread so the runtime is free to keep ticking. The
    // impl is idempotent: a follow-up Drop is a no-op.
    observe_for_shutdown.shutdown().await;
    drop(pid_guard);

    result
}

struct PidFile {
    path: std::path::PathBuf,
    value: String,
    #[cfg(unix)]
    identity: (u64, u64),
}

impl PidFile {
    async fn create(path: &std::path::Path) -> Result<Self> {
        use std::io::Write;

        match std::fs::symlink_metadata(path) {
            Ok(metadata) => {
                anyhow::ensure!(
                    metadata.file_type().is_file(),
                    "refusing to replace non-regular PID file {}",
                    path.display()
                );
                let pid = std::fs::read_to_string(path)?
                    .trim()
                    .parse::<u32>()
                    .with_context(|| {
                        format!("refusing to replace invalid PID file {}", path.display())
                    })?;
                anyhow::ensure!(
                    pid > 0 && !daemon::process_is_alive(pid),
                    "PID file {} belongs to a live process",
                    path.display()
                );
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    let current = std::fs::symlink_metadata(path)?;
                    anyhow::ensure!(
                        (metadata.dev(), metadata.ino()) == (current.dev(), current.ino()),
                        "PID file changed during startup: {}",
                        path.display()
                    );
                }
                std::fs::remove_file(path)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspecting PID file"),
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(path)
            .with_context(|| format!("creating PID file {}", path.display()))?;
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt;
            let metadata = file.metadata()?;
            (metadata.dev(), metadata.ino())
        };
        let guard = Self {
            path: path.to_path_buf(),
            value: std::process::id().to_string(),
            #[cfg(unix)]
            identity,
        };
        if let Err(error) = file.write_all(guard.value.as_bytes()) {
            // The create-new file is ours even if only part of the PID was written.
            drop(file);
            let _ = std::fs::remove_file(path);
            return Err(error).context("writing PID file");
        }
        Ok(guard)
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        let Ok(metadata) = std::fs::symlink_metadata(&self.path) else {
            return;
        };
        if !metadata.file_type().is_file() {
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if (metadata.dev(), metadata.ino()) != self.identity {
                return;
            }
        }
        if matches!(std::fs::read_to_string(&self.path), Ok(value) if value == self.value) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn announce_zero_config(source: &crate::paths::ConfigSource, cfg: &bitrouter_sdk::config::Config) {
    if !source.is_default() {
        return;
    }
    let enabled: Vec<&str> = cfg.providers.keys().map(String::as_str).collect();
    if enabled.is_empty() {
        print_onboarding_hint();
    } else {
        crate::error_report::info(format_args!(
            "zero-config mode — auto-enabled providers: {}",
            enabled.join(", ")
        ));
    }
}

/// Multi-line guidance shown when zero-config detects no credential of any
/// kind. The recommendation chain is intentional:
///
///   1. `bro cloud login` — one OAuth account, every supported model.
///   2. `BITROUTER_API_KEY` — long-lived `brk_…` key, same coverage.
///   3. Any upstream provider the user already pays for, locally.
///
/// Rendered directly (not through `error_report::info`) because that helper
/// is single-line by design.
/// First-run telemetry notice, shown exactly once per install (guarded by a
/// sentinel in the home). BitRouter ships telemetry **off by default**; this
/// notice exists so opting in is an informed, one-time choice. Failure to write
/// the sentinel is non-fatal — telemetry is never blocked on the notice.
fn maybe_announce_telemetry(home: &std::path::Path) {
    match crate::paths::mark_telemetry_notice_shown(home) {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            tracing::debug!("telemetry notice sentinel: {e:#}");
            return;
        }
    }
    let p = crate::style::Palette::for_stderr();
    eprintln!(
        "{cyan}{bold}info:{reset} optional usage telemetry is available — and OFF by default.",
        cyan = p.cyan,
        bold = p.bold,
        reset = p.reset,
    );
    eprintln!();
    eprintln!("  Nothing is sent unless you opt in. Two levels are offered:");
    eprintln!(
        "    • metadata — model, tokens, latency, finish reason, routing (no message content)"
    );
    eprintln!("    • full     — the above plus request + response message content");
    eprintln!();
    eprintln!("  Enable it under plugins.bitrouter-telemetry.telemetry in your config:");
    eprintln!();
    eprintln!("       plugins:");
    eprintln!("         bitrouter-telemetry:");
    eprintln!("           telemetry:");
    eprintln!("             enabled: true");
    eprintln!("             level: metadata   # or: full");
    eprintln!();
    eprintln!("  Remove the block (or set enabled: false) to turn it off again.");
    eprintln!();
}

fn print_onboarding_hint() {
    let p = crate::style::Palette::for_stderr();
    let cli = bitrouter_sdk::invocation::name();
    eprintln!(
        "{cyan}{bold}info:{reset} no providers are configured yet. Choose one:",
        cyan = p.cyan,
        bold = p.bold,
        reset = p.reset,
    );
    eprintln!();
    eprintln!("  1. Sign in to BitRouter Cloud — one account covers every model:");
    eprintln!();
    eprintln!("       {cli} cloud login");
    eprintln!("       {cli} cloud --help        # manage keys, usage, policies, billing");
    eprintln!();
    eprintln!("  2. Or paste a BitRouter API key:");
    eprintln!();
    eprintln!("       export BITROUTER_API_KEY=brk_…");
    eprintln!();
    eprintln!("  3. Or use a provider you already pay for, locally:");
    eprintln!();
    eprintln!("       {cli} providers login claude-code     # Claude Pro/Max subscription");
    eprintln!("       {cli} providers login github-copilot  # GitHub Copilot subscription");
    eprintln!("       {cli} providers login openai-codex    # ChatGPT subscription");
    eprintln!();
    eprintln!("     …or set an API-key env var:");
    eprintln!();
    let env_vars = other_provider_env_var_hints();
    for var in &env_vars {
        eprintln!("       export {var}=…");
    }
    eprintln!();
}

/// Deduplicated, sorted env-var names for every built-in provider except
/// `BITROUTER_API_KEY` (rendered separately as step 2). Used by the
/// onboarding hint.
fn other_provider_env_var_hints() -> Vec<String> {
    let mut vars: Vec<String> = bitrouter_providers::zero_config_env_var_providers()
        .into_iter()
        .map(|(_, env)| env)
        .filter(|v| v != "BITROUTER_API_KEY")
        .collect();
    vars.sort();
    vars.dedup();
    vars
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn pid_publication_preserves_foreign_files_and_recovers_stale_pid() -> anyhow::Result<()>
    {
        let home = tempfile::tempdir()?;
        let path = home.path().join("host.pid");
        std::fs::write(&path, "unrelated contents")?;
        assert!(PidFile::create(&path).await.is_err());
        assert_eq!(std::fs::read_to_string(&path)?, "unrelated contents");
        std::fs::write(&path, std::process::id().to_string())?;
        assert!(PidFile::create(&path).await.is_err());
        assert_eq!(
            std::fs::read_to_string(&path)?,
            std::process::id().to_string()
        );
        std::fs::write(&path, u32::MAX.to_string())?;
        let recovered = PidFile::create(&path).await?;
        drop(recovered);
        assert!(!path.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pid_publication_never_follows_symlinks() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        let target = home.path().join("target");
        let path = home.path().join("host.pid");
        std::fs::write(&target, "preserve")?;
        std::os::unix::fs::symlink(&target, &path)?;
        assert!(PidFile::create(&path).await.is_err());
        assert_eq!(std::fs::read_to_string(&target)?, "preserve");
        assert!(std::fs::symlink_metadata(&path)?.file_type().is_symlink());
        Ok(())
    }

    #[tokio::test]
    async fn pid_cleanup_only_removes_the_owned_record() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        let path = home.path().join("host.pid");
        let owned = PidFile::create(&path).await?;
        drop(owned);
        assert!(!path.exists());

        let replaced = PidFile::create(&path).await?;
        std::fs::write(&path, "replacement process")?;
        drop(replaced);
        assert_eq!(std::fs::read_to_string(&path)?, "replacement process");
        Ok(())
    }

    use super::*;
    #[tokio::test]
    async fn outer_shutdown_keeps_http_future_until_required_drain_recovers() -> anyhow::Result<()>
    {
        for control_trigger in [true, false] {
            let accepting = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let drain_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let recovery = Arc::new(tokio::sync::Notify::new());
            let inflight_release = Arc::new(tokio::sync::Notify::new());
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
            let (control_tx, control_rx) = tokio::sync::oneshot::channel();
            let (term_tx, term_rx) = tokio::sync::oneshot::channel();

            let http_accepting = accepting.clone();
            let http_attempts = drain_attempts.clone();
            let http_recovery = recovery.clone();
            let http_inflight_release = inflight_release.clone();
            let http = async move {
                shutdown_rx
                    .await
                    .map_err(|_| anyhow::anyhow!("outer shutdown sender disappeared"))?;
                http_accepting.store(false, std::sync::atomic::Ordering::SeqCst);
                http_inflight_release.notified().await;
                http_attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                http_recovery.notified().await;
                http_attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            };
            let control = async move {
                control_rx
                    .await
                    .map_err(|_| anyhow::anyhow!("test control sender disappeared"))?;
                Ok(())
            };
            let term = async move {
                term_rx
                    .await
                    .map_err(|_| anyhow::anyhow!("test term sender disappeared"))?;
                Ok(())
            };
            let hup = async move {
                if control_trigger {
                    return Err(anyhow::anyhow!("test HUP setup unavailable"));
                }
                std::future::pending::<anyhow::Result<()>>().await
            };
            let supervision = tokio::spawn(supervise_http_shutdown(
                http,
                control,
                hup,
                term,
                shutdown_tx,
            ));

            tokio::task::yield_now().await;
            assert!(
                !supervision.is_finished(),
                "a HUP setup error must not drop the HTTP server"
            );
            assert!(accepting.load(std::sync::atomic::Ordering::SeqCst));
            if control_trigger {
                control_tx
                    .send(())
                    .map_err(|_| anyhow::anyhow!("test control receiver disappeared"))?;
            } else {
                term_tx
                    .send(())
                    .map_err(|_| anyhow::anyhow!("test term receiver disappeared"))?;
            }
            tokio::task::yield_now().await;
            assert!(!accepting.load(std::sync::atomic::Ordering::SeqCst));
            assert_eq!(drain_attempts.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert!(
                !supervision.is_finished(),
                "in-flight HTTP work must finish before required drain"
            );

            inflight_release.notify_one();
            tokio::task::yield_now().await;
            assert_eq!(drain_attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert!(
                !supervision.is_finished(),
                "the outer supervisor dropped a server waiting on required drain"
            );
            recovery.notify_one();
            tokio::task::yield_now().await;
            assert_eq!(drain_attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
            supervision.await??;
        }
        Ok(())
    }

    #[tokio::test]
    async fn outer_shutdown_returns_http_errors_without_waiting_for_a_trigger() -> anyhow::Result<()>
    {
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let error = supervise_http_shutdown(
            async { Err(anyhow::anyhow!("test HTTP failure")) },
            std::future::pending::<anyhow::Result<()>>(),
            std::future::pending::<anyhow::Result<()>>(),
            std::future::pending::<anyhow::Result<()>>(),
            shutdown_tx,
        )
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("HTTP failure unexpectedly succeeded"))?;
        assert_eq!(error.to_string(), "test HTTP failure");
        assert!(shutdown_rx.await.is_err());
        Ok(())
    }
}
