//! Real controller, child, mounted App and actor; never a fabricated capture proof.
//! Build the Rust peer separately, then set BITROUTER_OWNED_CAPTURE_FIXTURE to it.
#[cfg(unix)]
mod faults;
#[cfg(unix)]
mod ingress;
#[cfg(all(
    unix,
    feature = "core",
    any(feature = "db-postgres", feature = "db-mysql")
))]
mod server;

use super::*;
use crate::{
    acp_cli::LaunchOptions,
    config::agent::{AcpAgentConfig, AcpTransport},
    harness::acp::controlled::{ControlledSession, LocalControllerLaunch},
};
use bitrouter_ai::tenancy::configuration::LearningMode;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn prepared(
    fixture: &Fixture,
    route: &ResourceRef,
    mode: LearningMode,
) -> Result<(HeaderMap, CredentialIssued)> {
    let mut input = fixture.configuration_input("capture-configuration")?;
    input.document.routes.push(route.clone());
    input.document.generation_defaults.max_output_tokens = 25;
    input.document.learning.mode = mode;
    let revision = match fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Configuration(fixture.tenant.clone(), input),
        )
        .await
        .map_err(failure)?
    {
        crate::tenancy::store::StoredResponse::Configuration(revision) => revision,
        _ => anyhow::bail!("configuration reply"),
    };
    let credential = fixture.issue("capture-key").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    Ok((
        headers(
            fixture,
            credential.secret.as_deref().context("key")?,
            &revision.revision_id,
        )?,
        credential,
    ))
}

fn executable() -> Result<PathBuf> {
    let path = PathBuf::from(std::env::var_os("BITROUTER_OWNED_CAPTURE_FIXTURE").context(
        "set BITROUTER_OWNED_CAPTURE_FIXTURE to the separately built owned_capture_fixture example",
    )?);
    ensure!(
        path.is_absolute() && path.is_file(),
        "fixture executable must be an existing absolute path"
    );
    Ok(path)
}
fn launch(
    program: &Path,
    marker: &Path,
    origin: &str,
    gate: Option<&Path>,
) -> LocalControllerLaunch {
    let mut env = HashMap::from([(
        "CAPTURE_FIXTURE_MARKER".into(),
        marker.to_string_lossy().into_owned(),
    )]);
    if let Some(gate) = gate {
        env.insert(
            "CAPTURE_FIXTURE_INITIALIZE_GATE".into(),
            gate.to_string_lossy().into_owned(),
        );
    }
    LocalControllerLaunch {
        source: "capture-fixture".into(),
        agent: AcpAgentConfig {
            name: "capture-fixture".into(),
            transport: AcpTransport::Stdio {
                command: program.to_string_lossy().into_owned(),
                args: vec![],
                env,
            },
        },
        listener_origin: origin.into(),
        options: LaunchOptions::default(),
    }
}
struct Listener {
    origin: String,
    stop: CancellationToken,
    task: Option<AbortOnDropHandle<crate::gateway::error::Result<()>>>,
}
impl Listener {
    async fn start(app: Arc<App>) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let origin = format!("http://{}", listener.local_addr()?);
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        // Public embedding listener, no test-added tenancy router or mock dispatch.
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
            app.serve_listener_with_router_wrapper_and_shutdown(
                listener,
                |router| router,
                stopped.cancelled_owned(),
            )
            .await
        }));
        Ok(Self {
            origin,
            stop,
            task: Some(task),
        })
    }
    async fn finish(mut self) -> Result<()> {
        self.stop.cancel();
        tokio::time::timeout(TIMEOUT, self.task.take().context("listener joined")?).await???;
        Ok(())
    }
}
impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
async fn count(fixture: &Fixture, table: &str) -> Result<i64> {
    // Table names are compile-time test inputs, never caller-controlled SQL.
    let row = fixture
        .db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            format!("SELECT COUNT(*) AS n FROM {table}"),
        ))
        .await?
        .context("count")?;
    Ok(row.try_get("", "n")?)
}
async fn reaped(marker: &Path) -> Result<()> {
    let value: Value = serde_json::from_slice(&tokio::fs::read(marker).await?)?;
    ensure!(
        value["inherited_home"] == false,
        "ambient HOME crossed owned launch"
    );
    let raw = i32::try_from(value["pid"].as_u64().context("child PID")?)?;
    let pid = rustix::process::Pid::from_raw(raw).context("valid child PID")?;
    ensure!(
        rustix::process::test_kill_process_group(pid) == Err(rustix::io::Errno::SRCH),
        "child group remains after successful shutdown"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn real_controller_passive_modes_freeze_owned_prefix_without_judge_or_legacy_reads()
-> Result<()> {
    let program = executable()?;
    for mode in [
        LearningMode::Off,
        LearningMode::Observe,
        LearningMode::Explore,
    ] {
        let mut provider = Provider::start(false).await?;
        let (fixture, route, _) = provision_protocol(&provider.url, "chat_completions").await?;
        let directory = tempfile::tempdir()?;
        let marker = directory.path().join("child.json");
        let spies = Arc::new(HostSpies::default());
        let app = Arc::new(
            host_app(&fixture, &provider.url, spies.clone())?.with_tenancy(fixture.service.clone()),
        );
        let listener = Listener::start(app.clone()).await?;
        let mut session = None;
        let checked = async {
            let (ingress, _) = prepared(&fixture, &route, mode).await?;
            session = Some(tokio::time::timeout(TIMEOUT, ControlledSession::open_authenticated(app.clone(), ingress.clone(), launch(&program, &marker, &listener.origin, None), CancellationToken::new())).await??);
            let current = session.as_ref().context("session")?;
            // Native036 and capture consume the SAME host slot, before workspace I/O.
            let native = crate::harness::resources::managed::session::NativeSession::open_authenticated(app.clone(), ingress, "competing", directory.path(), bitrouter_ai::harness::agent::ToolMode::ReadOnly, &crate::harness::resources::HarnessConfig::default()).await;
            let denied = native.err().context("native consumer acquired a second host slot")?;
            ensure!(denied.code == bitrouter_ai::core::protocol::ErrorCode::LimitExceeded, "native open did not refuse the shared host slot");
            let id = current.new_session(directory.path().to_path_buf()).await?;
            current.prompt(&id, "private task").await?;
            let observed = provider.receive().await?;
            ensure!(observed.body["model"] == "model-a" && observed.body["max_tokens"] == 25);
            ensure!(observed.headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer owned-upstream-secret"));
            let checkpoint = current.checkpoint(&id).await?;
            ensure!(checkpoint.is_none() == (mode == LearningMode::Off));
            if let Some(checkpoint) = checkpoint {
                ensure!(checkpoint.observations >= 5 && checkpoint.watermark > 0);
                let before = count(&fixture, "acp_checkpoints").await?;
                ensure!(current.checkpoint(&id).await? == Some(checkpoint));
                ensure!(count(&fixture, "acp_checkpoints").await? == before && before == 1);
                let row = fixture.db.query_one(Statement::from_string(DatabaseBackend::Sqlite, "SELECT installation_id, tenant_id, creator_principal_id, manifest_json FROM acp_checkpoints")).await?.context("checkpoint")?;
                ensure!(row.try_get::<String>("", "installation_id")? == fixture.installation.as_str());
                ensure!(row.try_get::<String>("", "tenant_id")? == fixture.tenant.as_str());
                ensure!(row.try_get::<String>("", "creator_principal_id")? == fixture.principal.as_str());
                let manifest: String = row.try_get("", "manifest_json")?;
                ensure!(!manifest.contains("private task") && !manifest.contains("native reply") && !manifest.contains("owned-upstream-secret"));
            }
            ensure!(crate::acp_trajectory::CanonicalStore::new(fixture.db.clone()).list(fixture.principal.as_str(), "capture-fixture").await?.is_empty());
            ensure!(count(&fixture, "acp_canonical_sessions").await? == 1);
            ensure!(count(&fixture, "checkpoint_evolution WHERE kind = 'judge_job'").await? == 0);
            ensure!(provider.calls() == 1 && spies.decisions.load(Ordering::SeqCst) == 0 && spies.reads.load(Ordering::SeqCst) == 0 && spies.transforms.load(Ordering::SeqCst) == 0);
            Ok(())
        }.await;
        let closed = match session.take() {
            Some(session) => session.shutdown().await,
            None => Ok(()),
        };
        let stopped = listener.finish().await;
        let checked = checked.and(closed).and(stopped).and(reaped(&marker).await);
        finish(&fixture, provider, checked).await?;
    }
    Ok(())
}

#[tokio::test]
async fn owned_controller_denials_precede_capture_rows_child_or_workspace() -> Result<()> {
    for denial in ["unauthenticated", "scoped"] {
        let provider = Provider::start(false).await?;
        let (fixture, route, _) = provision_protocol(&provider.url, "chat_completions").await?;
        let directory = tempfile::tempdir()?;
        let marker = directory.path().join("must-not-exist");
        let checked = async {
            let (mut ingress, credential) =
                prepared(&fixture, &route, LearningMode::Observe).await?;
            if denial == "unauthenticated" {
                ingress.clear();
            } else {
                change_authority(&fixture, &credential, &route, "scope").await?;
            }
            let app = Arc::new(
                host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                    .with_tenancy(fixture.service.clone()),
            );
            ensure!(
                ControlledSession::open_authenticated(
                    app,
                    ingress,
                    launch(
                        Path::new("/must-not-execute"),
                        &marker,
                        "http://127.0.0.1:1",
                        None
                    ),
                    CancellationToken::new()
                )
                .await
                .is_err()
            );
            ensure!(
                !marker.exists()
                    && count(&fixture, "acp_capture_connections").await? == 0
                    && provider.calls() == 0
            );
            Ok(())
        }
        .await;
        finish(&fixture, provider, checked).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn revocation_refuses_new_content_but_retains_admitted_disconnect_cleanup() -> Result<()> {
    let program = executable()?;
    let provider = Provider::start(false).await?;
    let (fixture, route, _) = provision_protocol(&provider.url, "chat_completions").await?;
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join("child.json");
    let app = Arc::new(
        host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
            .with_tenancy(fixture.service.clone()),
    );
    let listener = Listener::start(app.clone()).await?;
    let mut session = None;
    let checked = async {
        let (ingress, credential) = prepared(&fixture, &route, LearningMode::Observe).await?;
        session = Some(
            ControlledSession::open_authenticated(
                app,
                ingress,
                launch(&program, &marker, &listener.origin, None),
                CancellationToken::new(),
            )
            .await?,
        );
        let current = session.as_ref().context("session")?;
        let id = current.new_session(directory.path().to_path_buf()).await?;
        current.prompt(&id, "first admitted turn").await?;
        change_authority(&fixture, &credential, &route, "revoke").await?;
        ensure!(current.checkpoint(&id).await.is_err());
        ensure!(current.prompt(&id, "must not dispatch").await.is_err());
        ensure!(provider.calls() == 1 && count(&fixture, "acp_checkpoints").await? == 0);
        Ok(())
    }
    .await;
    let closed = match session.take() {
        Some(session) => session.shutdown().await,
        None => Ok(()),
    };
    let after = async {
        reaped(&marker).await?;
        ensure!(fixture.service.current().is_ok());
        let row = fixture
            .db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT event_json FROM acp_capture_events ORDER BY sequence DESC LIMIT 1",
            ))
            .await?
            .context("terminal capture")?;
        let event: Value = serde_json::from_str(&row.try_get::<String>("", "event_json")?)?;
        ensure!(event["kind"] == "disconnected");
        Ok(())
    }
    .await;
    let stopped = listener.finish().await;
    finish(
        &fixture,
        provider,
        checked.and(closed).and(after).and(stopped),
    )
    .await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn cancelled_initialize_waiter_keeps_real_child_owner_until_service_drain() -> Result<()> {
    let program = executable()?;
    let provider = Provider::start(false).await?;
    let (fixture, route, _) = provision_protocol(&provider.url, "chat_completions").await?;
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join("child.json");
    let gate_path = directory.path().join("initialize.sock");
    let gate = tokio::net::UnixListener::bind(&gate_path)?;
    let app = Arc::new(
        host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
            .with_tenancy(fixture.service.clone()),
    );
    let checked = async {
        let (ingress, _) = prepared(&fixture, &route, LearningMode::Observe).await?;
        let launch = launch(&program, &marker, "http://127.0.0.1:1", Some(&gate_path));
        let opener = AbortOnDropHandle::new(tokio::spawn(async move {
            ControlledSession::open_authenticated(app, ingress, launch, CancellationToken::new())
                .await
        }));
        let (mut admitted, _) = tokio::time::timeout(TIMEOUT, gate.accept()).await??;
        let mut bytes = [0; 5];
        tokio::time::timeout(TIMEOUT, admitted.read_exact(&mut bytes)).await??;
        ensure!(&bytes == b"ready");
        // Only the waiting API task is aborted. The Service owns the real child.
        opener.abort();
        ensure!(opener.await.is_err());
        tokio::time::timeout(Duration::from_secs(40), fixture.service.shutdown()).await??;
        reaped(&marker).await?;
        ensure!(provider.calls() == 0 && count(&fixture, "acp_capture_connections").await? == 1);
        let process: Value = serde_json::from_slice(&tokio::fs::read(&marker).await?)?;
        ensure!(
            process["provider_set"] == 0,
            "cancelled initialization configured a provider"
        );
        Ok(())
    }
    .await;
    finish(&fixture, provider, checked).await
}

#[tokio::test]
async fn cancelled_capture_create_waiter_retains_shared_slot_and_marks_unstarted_connection()
-> Result<()> {
    let provider = Provider::start(false).await?;
    let (fixture, route, _) = provision_protocol(&provider.url, "chat_completions").await?;
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join("never-spawned");
    let checked = async {
        let (ingress, _) = prepared(&fixture, &route, LearningMode::Observe).await?;
        let app = Arc::new(
            host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                .with_tenancy(fixture.service.clone()),
        );
        let gate = fixture
            .service
            .cleanup
            .hold(crate::tenancy::transaction::Phase::Active);
        let child_app = app.clone();
        let child_headers = ingress.clone();
        let proposed = launch(
            Path::new("/must-not-execute"),
            &marker,
            "http://127.0.0.1:1",
            None,
        );
        let opener = AbortOnDropHandle::new(tokio::spawn(async move {
            ControlledSession::open_authenticated(
                child_app,
                child_headers,
                proposed,
                CancellationToken::new(),
            )
            .await
        }));
        tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        opener.abort();
        ensure!(opener.await.is_err());
        ensure!(
            ControlledSession::open_authenticated(
                app,
                ingress,
                launch(
                    Path::new("/must-not-execute"),
                    &marker,
                    "http://127.0.0.1:1",
                    None
                ),
                CancellationToken::new()
            )
            .await
            .is_err(),
            "abandoned waiter released shared slot"
        );
        let service = fixture.service.clone();
        let shutdown =
            AbortOnDropHandle::new(tokio::spawn(async move { service.shutdown().await }));
        fixture.service.host_stop().cancelled().await;
        ensure!(!shutdown.is_finished() && !marker.exists());
        fixture.service.cleanup.release_gate();
        tokio::time::timeout(TIMEOUT, shutdown).await???;
        // Creation was genuinely admitted; either rolled back or durably closed
        // without claiming a child connection ever initialized. Never recording.
        let rows = fixture
            .db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT state, metadata_json FROM acp_capture_connections",
            ))
            .await?;
        for row in rows {
            ensure!(row.try_get::<String>("", "state")? == "interrupted");
            let metadata: Value =
                serde_json::from_str(&row.try_get::<String>("", "metadata_json")?)?;
            ensure!(metadata["connected"] == false && metadata["closed"] == true);
        }
        ensure!(!marker.exists() && provider.calls() == 0);
        Ok(())
    }
    .await;
    fixture.service.cleanup.release_gate();
    finish(&fixture, provider, checked).await
}

#[tokio::test]
async fn capture_profile_rechecks_current_role_and_original_credential_permissions() -> Result<()> {
    use bitrouter_ai::tenancy::authority::Permission;
    let provider = Provider::start(false).await?;
    let (fixture, route, _) = provision_protocol(&provider.url, "chat_completions").await?;
    let checked = async {
        let (ingress, _) = prepared(&fixture, &route, LearningMode::Observe).await?;
        for (name, role, allowed) in [
            ("viewer", TenantRole::Viewer, false),
            ("editor", TenantRole::Editor, true),
            ("operator", TenantRole::TenantOperator, false),
            ("admin", TenantRole::TenantAdmin, true),
        ] {
            let snapshot = fixture.service.current().map_err(failure)?;
            let previous = snapshot
                .bindings
                .get(&(fixture.tenant.clone(), fixture.binding.clone()))
                .context("binding")?;
            result(
                fixture
                    .service
                    .execute(
                        fixture.operator.clone(),
                        Command::Binding(
                            fixture.tenant.clone(),
                            fixture.binding.clone(),
                            BindingPut {
                                operation_id: format!("capture-role-{name}").try_into()?,
                                expected_revision: Some(previous.revision.clone()),
                                principal_id: fixture.principal.clone(),
                                role,
                                scope: GrantScope::Tenant {
                                    operator_pools: vec![],
                                },
                                expires_at: previous.expires.wire.clone(),
                                expected_management_owner: previous
                                    .managed_by
                                    .as_ref()
                                    .map(|owner| owner.principal.clone()),
                                installation_id: fixture.installation.clone(),
                            },
                        ),
                    )
                    .await
                    .map_err(failure)?,
            )?;
            ensure!(
                fixture
                    .service
                    .prepare_capture(
                        &ingress,
                        "fixture",
                        "http://127.0.0.1:1",
                        CancellationToken::new()
                    )
                    .await
                    .is_ok()
                    == allowed,
                "role {name}"
            );
        }
        ensure!(count(&fixture, "acp_capture_connections").await? == 0 && provider.calls() == 0);
        Ok(())
    }
    .await;
    finish(&fixture, provider, checked).await?;

    let full = [
        Permission::ExecutionSubmit,
        Permission::ResourceRead,
        Permission::EvaluationSubmit,
        Permission::CredentialUse,
    ];
    for (index, removed) in full.iter().enumerate() {
        // Each case owns a fresh authority: the original key and one restricted
        // key fit the existing two-live-credentials-per-principal limit.
        let provider = Provider::start(false).await?;
        let (fixture, route, _) = provision_protocol(&provider.url, "chat_completions").await?;
        let checked = async {
            let (ingress, _) = prepared(&fixture, &route, LearningMode::Observe).await?;
            fixture
                .service
                .prepare_capture(
                    &ingress,
                    "fixture",
                    "http://127.0.0.1:1",
                    CancellationToken::new(),
                )
                .await
                .map_err(failure)?;
            let revision = ingress
                .get("bitrouter-execution-revision")
                .context("revision")?
                .to_str()?
                .try_into()?;
            let mut input = fixture.issuance(&format!("capture-missing-{index}"))?;
            input.permissions = full
                .iter()
                .copied()
                .filter(|permission| permission != removed)
                .collect();
            let credential = match fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Issue(fixture.tenant.clone(), input),
                )
                .await
                .map_err(failure)?
            {
                crate::tenancy::store::StoredResponse::Issued(value) => value,
                crate::tenancy::store::StoredResponse::Failure(error) => {
                    anyhow::bail!("credential case {removed:?}: {:?}", error.code)
                }
                _ => anyhow::bail!("unexpected credential reply for {removed:?}"),
            };
            let limited = headers(
                &fixture,
                credential.secret.as_deref().context("key")?,
                &revision,
            )?;
            let denied = fixture
                .service
                .prepare_capture(
                    &limited,
                    "fixture",
                    "http://127.0.0.1:1",
                    CancellationToken::new(),
                )
                .await;
            ensure!(
                matches!(denied, Err(error) if error.0.code == ErrorCode::Forbidden),
                "missing permission {removed:?} must deny capture"
            );
            ensure!(
                count(&fixture, "acp_capture_connections").await? == 0 && provider.calls() == 0
            );
            Ok(())
        }
        .await;
        finish(&fixture, provider, checked).await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn failed_provider_initialization_keeps_partial_client_child_and_capture_until_joined()
-> Result<()> {
    let program = executable()?;
    let provider = Provider::start(false).await?;
    let (fixture, route, _) = provision_protocol(&provider.url, "chat_completions").await?;
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join("child.json");
    let checked = async {
        let (ingress, _) = prepared(&fixture, &route, LearningMode::Observe).await?;
        let app = Arc::new(
            host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                .with_tenancy(fixture.service.clone()),
        );
        let mut proposed = launch(&program, &marker, "http://127.0.0.1:1", None);
        let AcpTransport::Stdio { env, .. } = &mut proposed.agent.transport;
        env.insert("CAPTURE_FIXTURE_FAIL_PROVIDER".into(), "1".into());
        ensure!(
            tokio::time::timeout(
                TIMEOUT,
                ControlledSession::open_authenticated(
                    app,
                    ingress,
                    proposed,
                    CancellationToken::new()
                )
            )
            .await?
            .is_err()
        );
        // Opener error is not cleanup acknowledgement. Drain the real owner;
        // this includes the client driver created before initialize returned.
        tokio::time::timeout(Duration::from_secs(40), fixture.service.shutdown()).await??;
        reaped(&marker).await?;
        ensure!(provider.calls() == 0 && count(&fixture, "acp_canonical_sessions").await? == 0);
        let row = fixture
            .db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT metadata_json FROM acp_capture_connections",
            ))
            .await?
            .context("capture connection")?;
        let metadata: Value = serde_json::from_str(&row.try_get::<String>("", "metadata_json")?)?;
        ensure!(metadata["closed"] == true);
        Ok(())
    }
    .await;
    finish(&fixture, provider, checked).await
}

#[tokio::test]
#[ignore = "requires separately built BITROUTER_OWNED_CAPTURE_FIXTURE; no nested Cargo"]
async fn revoke_across_initialize_wait_prevents_the_next_provider_configuration_effect()
-> Result<()> {
    let program = executable()?;
    let provider = Provider::start(false).await?;
    let (fixture, route, _) = provision_protocol(&provider.url, "chat_completions").await?;
    let directory = tempfile::tempdir()?;
    let marker = directory.path().join("child.json");
    let gate_path = directory.path().join("initialize.sock");
    let gate = tokio::net::UnixListener::bind(&gate_path)?;
    let checked = async {
        let (ingress, credential) = prepared(&fixture, &route, LearningMode::Observe).await?;
        let app = Arc::new(
            host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                .with_tenancy(fixture.service.clone()),
        );
        let proposed = launch(&program, &marker, "http://127.0.0.1:1", Some(&gate_path));
        let opener = AbortOnDropHandle::new(tokio::spawn(async move {
            ControlledSession::open_authenticated(app, ingress, proposed, CancellationToken::new())
                .await
        }));
        let (mut admitted, _) = tokio::time::timeout(TIMEOUT, gate.accept()).await??;
        let mut bytes = [0; 5];
        tokio::time::timeout(TIMEOUT, admitted.read_exact(&mut bytes)).await??;
        ensure!(&bytes == b"ready");
        change_authority(&fixture, &credential, &route, "revoke").await?;
        admitted.write_all(b"x").await?;
        ensure!(tokio::time::timeout(TIMEOUT, opener).await??.is_err());
        tokio::time::timeout(Duration::from_secs(40), fixture.service.shutdown()).await??;
        let process: Value = serde_json::from_slice(&tokio::fs::read(&marker).await?)?;
        ensure!(
            process["provider_set"] == 0,
            "revoked initialization configured a provider"
        );
        reaped(&marker).await?;
        ensure!(provider.calls() == 0 && count(&fixture, "acp_canonical_sessions").await? == 0);
        Ok(())
    }
    .await;
    finish(&fixture, provider, checked).await
}
