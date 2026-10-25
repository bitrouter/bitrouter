//! Positive ownership through the persistent harness and the actual App pipeline.
use super::*;
mod authority;
mod consumer;
#[cfg(unix)]
mod custody;
mod discovery;
mod server;
mod waits;
use crate::harness::resources::{
    HarnessConfig,
    managed::session::{NativeApproval, NativeSession},
};
use bitrouter_ai::{
    core::protocol::{CoreError, TaskInput, ToolExecute},
    harness::agent::ToolMode,
};
use chrono::Utc;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

struct Approval;
#[async_trait]
impl NativeApproval for Approval {
    async fn approve(&self, _: &ToolExecute) -> std::result::Result<bool, CoreError> {
        Ok(true)
    }
}

fn input() -> TaskInput {
    TaskInput {
        text: "Return the retained native reply".into(),
        model: MODEL.into(),
        effort: None,
        max_output_tokens: Some(25),
        context_limit_bytes: None,
        routing: Default::default(),
        max_concurrent_subagents: None,
        discardable_history: None,
        acceptance_criteria: vec![],
        required_materials: vec![],
        verification: None,
        limits: None,
    }
}

async fn file_fixture(
    provider: &Provider,
    directory: &std::path::Path,
) -> Result<(Fixture, ResourceRef)> {
    let database = directory.join("owned.db");
    let url = format!("sqlite://{}", database.display());
    let owner = crate::tenancy::ownership::acquire(&url)?;
    let db = crate::db::connect_tenancy(&url, &owner).await?;
    // Provision the encrypted resources on this leased file from the start.
    // An in-memory connection's VACUUM INTO can retain SQLITE_OPEN_MEMORY.
    let fixture = Fixture::with_database(db, OperationLimits::default(), &mut |_| {}).await?;
    let (mut fixture, route, _) =
        crate::tenancy::tests::execution::provision_protocol_with_fixture(
            fixture,
            &provider.url,
            "chat_completions",
            &mut |_| {},
        )
        .await?;
    fixture.service.shutdown().await?;
    let metadata = std::fs::metadata(&owner.path)?;
    ensure!(
        metadata.is_file() && metadata.len() > 0,
        "authority file was not materialized"
    );
    // Check SQLite's actual main filename, not merely the sidecar file lease.
    // https://www.sqlite.org/pragma.html#pragma_database_list
    let databases = fixture
        .db
        .query_all(Statement::from_string(
            DatabaseBackend::Sqlite,
            "PRAGMA database_list",
        ))
        .await?;
    let mut main = None;
    for row in databases {
        if row.try_get::<String>("", "name")? == "main" {
            ensure!(main.is_none(), "duplicate main database");
            main = Some(row.try_get::<String>("", "file")?);
        }
    }
    let main = main.context("SQLite main database missing")?;
    ensure!(
        !main.is_empty() && std::path::Path::new(&main).canonicalize()? == owner.path,
        "SQLite main is not the leased authority file"
    );
    let snapshot = store::load(&fixture.db, Utc::now()).await?;
    ensure!(
        snapshot.installation == fixture.installation,
        "persisted installation changed"
    );
    fixture.service = Service::start_prepared(
        fixture.db.clone(),
        Some(owner),
        OperationLimits::default(),
        snapshot,
        Arc::new(Resources::with_key(fixture.installation.clone(), [29; 32])?),
    )
    .await?;
    Ok((fixture, route))
}

#[tokio::test]
async fn persistent_owned_harness_runs_releases_restores_and_keeps_host_hooks_unused() -> Result<()>
{
    let mut provider = Provider::start(false).await?;
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let (fixture, route) = file_fixture(&provider, directory.path()).await?;
    let checked = async {
        let (headers, _) = ready(&fixture, &route).await?;
        let spies = Arc::new(HostSpies::default());
        let app = Arc::new(host_app(&fixture, &provider.url, spies.clone())?.with_tenancy(fixture.service.clone()));
        let mut session = tokio::time::timeout(TIMEOUT, NativeSession::open_authenticated(
            app.clone(), headers.clone(), "same-name", &workspace, ToolMode::ReadOnly, &HarnessConfig::default())).await??;
        let original = session.core().snapshot().await.session_id;
        let state = tokio::time::timeout(TIMEOUT, session.run(input(), Arc::new(Approval), CancellationToken::new())).await??;
        ensure!(state.run.as_ref().is_some_and(|run| run.status == crate::storage::checkpoint::snapshot::RunStatus::Completed));
        let observed = provider.receive().await?;
        ensure!(observed.path == "/v1/chat/completions");
        ensure!(observed.body["model"] == "model-a");
        ensure!(observed.headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer owned-upstream-secret"));
        tokio::time::timeout(TIMEOUT, session.close()).await??;

        // A new credential for the same stable principal restores the same journal.
        let rotated = fixture.issue("native-session-next-key").await?;
        let revision = headers.get("bitrouter-execution-revision").context("revision")?.to_str()?.try_into()?;
        let rotated_headers = super::super::headers(&fixture, rotated.secret.as_deref().context("new key")?, &revision)?;
        let mut restored = tokio::time::timeout(TIMEOUT, NativeSession::open_authenticated(
            app, rotated_headers, "same-name", &workspace, ToolMode::ReadOnly, &HarnessConfig::default())).await??;
        ensure!(restored.core().snapshot().await.session_id == original);
        let state = tokio::time::timeout(TIMEOUT, restored.run(input(), Arc::new(Approval), CancellationToken::new())).await??;
        ensure!(state.run.as_ref().is_some_and(|run| run.status == crate::storage::checkpoint::snapshot::RunStatus::Completed));
        provider.receive().await?;
        tokio::time::timeout(TIMEOUT, restored.close()).await??;
        ensure!(provider.calls() == 2);
        ensure!(spies.transforms.load(Ordering::SeqCst) == 0);
        ensure!(spies.reads.load(Ordering::SeqCst) == 0);
        ensure!(spies.decisions.load(Ordering::SeqCst) == 0);
        let row = fixture.db.query_one(Statement::from_string(DatabaseBackend::Sqlite,
            "SELECT installation_id, tenant_id, session_principal_id, logical_name, revision, payload FROM bro_managed_sessions"))
            .await?.context("owned journal")?;
        ensure!(row.try_get::<String>("", "installation_id")? == fixture.installation.as_str());
        ensure!(row.try_get::<String>("", "tenant_id")? == fixture.tenant.as_str());
        ensure!(row.try_get::<String>("", "session_principal_id")? == fixture.principal.as_str());
        ensure!(row.try_get::<String>("", "logical_name")? == "same-name");
        ensure!(row.try_get::<i64>("", "revision")? > 1);
        let envelope: Value = serde_json::from_str(&row.try_get::<String>("", "payload")?)?;
        ensure!(envelope["journal"]["released"] == true);
        ensure!(envelope["journal"]["grant"]["session_id"] == original);
        ensure!(!envelope.to_string().contains(rotated.secret.as_deref().context("new key")?));
        Ok(())
    }.await;
    finish(&fixture, provider, checked).await
}

#[tokio::test]
async fn unbound_and_unauthenticated_native_openers_reject_before_workspace_reads() -> Result<()> {
    let provider = Provider::start(false).await?;
    let directory = tempfile::tempdir()?;
    let (fixture, route) = file_fixture(&provider, directory.path()).await?;
    let checked = async {
        let (headers, _) = ready(&fixture, &route).await?;
        let app = Arc::new(
            host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                .with_tenancy(fixture.service.clone()),
        );
        let missing_workspace = directory.path().join("does-not-exist");
        let denied = NativeSession::open_authenticated(
            app.clone(),
            HeaderMap::new(),
            "private",
            &missing_workspace,
            ToolMode::ReadOnly,
            &HarnessConfig::default(),
        )
        .await
        .err()
        .context("missing auth accepted")?;
        ensure!(denied.code == bitrouter_ai::core::protocol::ErrorCode::UnauthorizedScope);
        let legacy_store = Arc::new(crate::managed_store::DatabaseNativeStore::new(
            fixture.db.clone(),
            "private".into(),
        )?);
        let denied = NativeSession::open(
            app.clone(),
            CallerContext::new("forged", "principal-a"),
            legacy_store,
            &missing_workspace,
            ToolMode::ReadOnly,
            &HarnessConfig::default(),
        )
        .await
        .err()
        .context("Legacy entry accepted")?;
        ensure!(denied.code == bitrouter_ai::core::protocol::ErrorCode::UnauthorizedScope);
        let mut duplicate = headers;
        duplicate.append("authorization", "Bearer forged".parse()?);
        ensure!(
            NativeSession::open_authenticated(
                app,
                duplicate,
                "private",
                &missing_workspace,
                ToolMode::ReadOnly,
                &HarnessConfig::default()
            )
            .await
            .is_err()
        );
        ensure!(provider.calls() == 0);
        let row = fixture
            .db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS rows FROM bro_managed_sessions",
            ))
            .await?
            .context("count")?;
        ensure!(row.try_get::<i64>("", "rows")? == 0);
        Ok(())
    }
    .await;
    finish(&fixture, provider, checked).await
}

#[tokio::test]
async fn cancelled_open_keeps_host_and_actor_read_custody_until_rollback() -> Result<()> {
    let provider = Provider::start(false).await?;
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let (fixture, route) = file_fixture(&provider, directory.path()).await?;
    let checked = async {
        let (headers, _) = ready(&fixture, &route).await?;
        let app = Arc::new(
            host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                .with_tenancy(fixture.service.clone()),
        );
        let gate = fixture
            .service
            .cleanup
            .hold(crate::tenancy::transaction::Phase::Active);
        let next_app = app.clone();
        let next_headers = headers.clone();
        let next_workspace = workspace.clone();
        let opener = AbortOnDropHandle::new(tokio::spawn(async move {
            NativeSession::open_authenticated(
                next_app,
                next_headers,
                "held",
                &next_workspace,
                ToolMode::ReadOnly,
                &HarnessConfig::default(),
            )
            .await
        }));
        tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        opener.abort();
        ensure!(opener.await.is_err());
        let denied = NativeSession::open_authenticated(
            app,
            headers,
            "another",
            &workspace,
            ToolMode::ReadOnly,
            &HarnessConfig::default(),
        )
        .await
        .err()
        .context("held host slot was released")?;
        ensure!(denied.code == bitrouter_ai::core::protocol::ErrorCode::LimitExceeded);
        let service = fixture.service.clone();
        let shutdown =
            AbortOnDropHandle::new(tokio::spawn(async move { service.shutdown().await }));
        fixture.service.host_stop().cancelled().await;
        ensure!(!shutdown.is_finished());
        fixture.service.cleanup.release_gate();
        tokio::time::timeout(TIMEOUT, shutdown).await???;
        ensure!(provider.calls() == 0);
        Ok(())
    }
    .await;
    fixture.service.cleanup.release_gate();
    finish(&fixture, provider, checked).await
}

#[tokio::test]
async fn revocation_keeps_release_and_ack_cleanup_but_stops_next_native_run() -> Result<()> {
    let provider = Provider::start(false).await?;
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let (fixture, route) = file_fixture(&provider, directory.path()).await?;
    let checked = async {
        let (headers, credential) = ready(&fixture, &route).await?;
        let app = Arc::new(
            host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                .with_tenancy(fixture.service.clone()),
        );
        let mut session = NativeSession::open_authenticated(
            app,
            headers,
            "revoked",
            &workspace,
            ToolMode::ReadOnly,
            &HarnessConfig::default(),
        )
        .await?;
        tokio::time::timeout(
            TIMEOUT,
            session.run(input(), Arc::new(Approval), CancellationToken::new()),
        )
        .await??;
        change_authority(&fixture, &credential, &route, "revoke").await?;
        ensure!(
            tokio::time::timeout(
                TIMEOUT,
                session.run(input(), Arc::new(Approval), CancellationToken::new())
            )
            .await?
            .is_err()
        );
        tokio::time::timeout(TIMEOUT, session.close()).await??;
        ensure!(provider.calls() == 1);
        ensure!(fixture.service.current().is_ok());
        Ok(())
    }
    .await;
    finish(&fixture, provider, checked).await
}

#[tokio::test]
async fn idle_host_keeps_no_execution_lease_and_every_run_uses_retained_r1() -> Result<()> {
    use bitrouter_ai::tenancy::configuration::Activation;
    let mut provider = Provider::start(false).await?;
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let (fixture, route) = file_fixture(&provider, directory.path()).await?;
    let checked = async {
        let (headers, credential) = ready(&fixture, &route).await?;
        let r1 = headers
            .get("bitrouter-execution-revision")
            .context("R1")?
            .to_str()?
            .to_owned();
        let app = Arc::new(
            host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                .with_tenancy(fixture.service.clone()),
        );
        let mut session = NativeSession::open_authenticated(
            app.clone(),
            headers.clone(),
            "fixed",
            &workspace,
            ToolMode::ReadOnly,
            &HarnessConfig::default(),
        )
        .await?;
        // The fixed fixture has four execution slots. Acquire all four while the
        // persistent host is idle; a session-lifetime lease would block the last.
        let mut leases = Vec::new();
        for _ in 0..4 {
            leases.push(
                tokio::time::timeout(TIMEOUT, super::super::admitted(&fixture, &headers)).await??,
            );
        }
        drop(leases);
        let r2 = draft(&fixture, &route, "native-r2", 7).await?;
        result(
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Activate(
                        fixture.tenant.clone(),
                        Activation {
                            operation_id: "activate-native-r2".try_into()?,
                            expected_active_revision: None,
                            revision_id: r2.revision_id.clone(),
                            expected_authority_revision: fixture
                                .service
                                .current()
                                .map_err(failure)?
                                .revision,
                            installation_id: fixture.installation.clone(),
                        },
                    ),
                )
                .await
                .map_err(failure)?,
        )?;
        for _ in 0..2 {
            tokio::time::timeout(
                TIMEOUT,
                session.run(input(), Arc::new(Approval), CancellationToken::new()),
            )
            .await??;
            let observed = provider.receive().await?;
            ensure!(observed.body["model"] == "model-a");
            ensure!(
                observed.body["max_tokens"] == 25,
                "active R2 must not replace retained R1"
            );
            let lease =
                tokio::time::timeout(TIMEOUT, super::super::admitted(&fixture, &headers)).await??;
            drop(lease);
        }
        tokio::time::timeout(TIMEOUT, session.close()).await??;
        let r2_headers = super::super::headers(
            &fixture,
            credential.secret.as_deref().context("key")?,
            &r2.revision_id,
        )?;
        let unavailable_workspace = directory.path().join("must-not-read");
        let denied = NativeSession::open_authenticated(
            app,
            r2_headers,
            "fixed",
            &unavailable_workspace,
            ToolMode::ReadOnly,
            &HarnessConfig::default(),
        )
        .await
        .err()
        .context("changed revision restored")?;
        ensure!(denied.code == bitrouter_ai::core::protocol::ErrorCode::UnauthorizedScope);
        let row = fixture
            .db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT execution_revision, payload FROM bro_managed_sessions",
            ))
            .await?
            .context("native row")?;
        ensure!(row.try_get::<String>("", "execution_revision")? == r1);
        let envelope: Value = serde_json::from_str(&row.try_get::<String>("", "payload")?)?;
        ensure!(envelope["journal"]["released"] == true);
        ensure!(provider.calls() == 2);
        Ok(())
    }
    .await;
    finish(&fixture, provider, checked).await
}
