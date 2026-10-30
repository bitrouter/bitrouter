//! Actual native consumer authority, including waits before local resource startup.
use super::*;
use crate::tenancy::hosts::HostBoundary;
use bitrouter_ai::tenancy::authority::{CredentialCreate, Permission};

async fn binding(
    fixture: &Fixture,
    role: TenantRole,
    scope: GrantScope,
    operation: &str,
) -> Result<()> {
    let state = fixture.service.current().map_err(failure)?;
    let prior = state
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
                        operation_id: operation.try_into()?,
                        expected_revision: Some(prior.revision.clone()),
                        principal_id: fixture.principal.clone(),
                        role,
                        scope,
                        expires_at: prior.expires.wire.clone(),
                        expected_management_owner: prior
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
    Ok(())
}

async fn issue(fixture: &Fixture, input: CredentialCreate) -> Result<CredentialIssued> {
    crate::tenancy::tests::issued(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Issue(fixture.tenant.clone(), input),
            )
            .await
            .map_err(failure)?,
    )
}

async fn rows(fixture: &Fixture) -> Result<i64> {
    let row = fixture
        .db
        .query_one(Statement::from_string(
            fixture.db.get_database_backend(),
            "SELECT COUNT(*) AS total FROM bro_managed_sessions",
        ))
        .await?
        .context("native count")?;
    Ok(row.try_get("", "total")?)
}

#[tokio::test]
async fn owned_proxy_rejects_split_driver_calls_without_poisoning_owned_run() -> Result<()> {
    let mut provider = Provider::start(false).await?;
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let (fixture, route) = file_fixture(&provider, directory.path()).await?;
    let checked = tokio::time::timeout(TIMEOUT, async {
        let (headers, _) = ready(&fixture, &route).await?;
        let app = Arc::new(
            host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                .with_tenancy(fixture.service.clone()),
        );
        let mut session = NativeSession::open_authenticated(
            app,
            headers,
            "split-driver",
            &workspace,
            ToolMode::ReadOnly,
            &HarnessConfig::default(),
        )
        .await?;
        let head = session.core().head().await;
        let before = fixture
            .db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT revision, payload FROM bro_managed_sessions",
            ))
            .await?
            .context("opened journal")?;

        let denied = session
            .start("split-input", input())
            .await
            .err()
            .context("owned proxy accepted direct start")?;
        ensure!(denied.code == bitrouter_ai::core::protocol::ErrorCode::UnauthorizedScope);
        ensure!(denied.commit_status == bitrouter_ai::core::protocol::CommitStatus::NotCommitted);
        let denied = session
            .drive(Arc::new(Approval), CancellationToken::new())
            .await
            .err()
            .context("owned proxy accepted direct drive")?;
        ensure!(denied.code == bitrouter_ai::core::protocol::ErrorCode::UnauthorizedScope);
        ensure!(denied.commit_status == bitrouter_ai::core::protocol::CommitStatus::NotCommitted);
        ensure!(session.core().head().await == head);
        ensure!(session.core().operation("split-input").await.is_none());
        ensure!(provider.calls() == 0);
        let after = fixture
            .db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT revision, payload FROM bro_managed_sessions",
            ))
            .await?
            .context("journal after rejected split calls")?;
        ensure!(before.try_get::<i64>("", "revision")? == after.try_get::<i64>("", "revision")?);
        ensure!(
            before.try_get::<String>("", "payload")? == after.try_get::<String>("", "payload")?
        );

        // Use the same handle: rejecting the CLI split API must leave its real
        // owner usable, with no queued input or damaged receiver to recover.
        let state = session
            .run(input(), Arc::new(Approval), CancellationToken::new())
            .await?;
        ensure!(state.run.as_ref().is_some_and(|run| {
            run.status == crate::storage::checkpoint::snapshot::RunStatus::Completed
        }));
        let observed = provider.receive().await?;
        ensure!(observed.body["model"] == "model-a");
        ensure!(
            observed
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                == Some("Bearer owned-upstream-secret")
        );
        session.close().await?;
        ensure!(provider.calls() == 1 && rows(&fixture).await? == 1);
        ensure!(fixture.service.host_slots_for_test() == 1);
        Ok(())
    })
    .await
    .context("owned split-driver deadline")
    .and_then(|outcome| outcome);
    finish(&fixture, provider, checked).await
}

#[tokio::test]
async fn native_host_uses_credential_and_current_binding_permission_intersection() -> Result<()> {
    for (name, role, permissions, allowed) in [
        (
            "viewer",
            TenantRole::Viewer,
            TenantRole::TenantAdmin.permissions().to_vec(),
            false,
        ),
        (
            "operator",
            TenantRole::TenantOperator,
            TenantRole::TenantAdmin.permissions().to_vec(),
            false,
        ),
        (
            "editor",
            TenantRole::Editor,
            TenantRole::TenantAdmin.permissions().to_vec(),
            true,
        ),
        (
            "admin",
            TenantRole::TenantAdmin,
            TenantRole::TenantAdmin.permissions().to_vec(),
            true,
        ),
        (
            "execute-only",
            TenantRole::TenantAdmin,
            vec![Permission::ExecutionSubmit],
            false,
        ),
        (
            "credential-only",
            TenantRole::TenantAdmin,
            vec![Permission::CredentialUse],
            false,
        ),
        (
            "minimum",
            TenantRole::TenantAdmin,
            vec![Permission::ExecutionSubmit, Permission::CredentialUse],
            true,
        ),
    ] {
        let provider = Provider::start(false).await?;
        let directory = tempfile::tempdir()?;
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let (fixture, route) = file_fixture(&provider, directory.path()).await?;
        let checked = tokio::time::timeout(TIMEOUT, async {
            let revision = draft(&fixture, &route, "role-config", 25).await?;
            fixture
                .service
                .dispatch_test_ready
                .store(true, Ordering::Release);
            // Issue under the actual admin binding, then narrow that binding.
            let mut request = fixture.issuance("role-key")?;
            request.permissions = permissions;
            let key = issue(&fixture, request).await?;
            binding(
                &fixture,
                role,
                GrantScope::Tenant {
                    operator_pools: vec![],
                },
                "role-change",
            )
            .await?;
            let headers = super::super::super::headers(
                &fixture,
                key.secret.as_deref().context("key")?,
                &revision.revision_id,
            )?;
            let app = Arc::new(
                host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                    .with_tenancy(fixture.service.clone()),
            );
            let path = if allowed {
                workspace.clone()
            } else {
                directory.path().join("not-readable")
            };
            let opened = NativeSession::open_authenticated(
                app,
                headers,
                name,
                &path,
                ToolMode::ReadOnly,
                &HarnessConfig::default(),
            )
            .await;
            if allowed {
                let mut session = opened?;
                session
                    .run(input(), Arc::new(Approval), CancellationToken::new())
                    .await?;
                session.close().await?;
                ensure!(
                    provider.calls() == 1 && rows(&fixture).await? == 1,
                    "{name}"
                );
            } else {
                ensure!(
                    opened.err().context("permission ceiling bypassed")?.code
                        == bitrouter_ai::core::protocol::ErrorCode::UnauthorizedScope,
                    "{name}"
                );
                ensure!(
                    provider.calls() == 0 && rows(&fixture).await? == 0,
                    "{name}"
                );
            }
            ensure!(fixture.service.host_slots_for_test() == 1);
            Ok(())
        })
        .await
        .context("native permission matrix deadline")
        .and_then(|outcome| outcome);
        finish(&fixture, provider, checked).await?;
    }
    Ok(())
}

#[tokio::test]
async fn native_resource_grants_require_the_exact_retained_route_and_credential_closure()
-> Result<()> {
    let provider = Provider::start(false).await?;
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let (fixture, route) = file_fixture(&provider, directory.path()).await?;
    let checked = tokio::time::timeout(TIMEOUT, async {
        let revision = draft(&fixture, &route, "scope-config", 25).await?;
        fixture
            .service
            .dispatch_test_ready
            .store(true, Ordering::Release);
        let mut credential = route.key();
        credential.resource_kind = bitrouter_ai::tenancy::authority::ResourceKind::Credential;
        credential.resource_id = "upstream-slot".try_into()?;
        let complete = GrantScope::Resources {
            resources: vec![route.key(), credential.clone()],
        };
        let mut request = fixture.issuance("closure-key")?;
        request.scope = complete.clone();
        let key = issue(&fixture, request).await?;
        binding(
            &fixture,
            TenantRole::TenantAdmin,
            complete,
            "complete-binding",
        )
        .await?;
        let headers = super::super::super::headers(
            &fixture,
            key.secret.as_deref().context("key")?,
            &revision.revision_id,
        )?;
        let app = Arc::new(
            host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                .with_tenancy(fixture.service.clone()),
        );
        let mut session = NativeSession::open_authenticated(
            app.clone(),
            headers.clone(),
            "scoped",
            &workspace,
            ToolMode::ReadOnly,
            &HarnessConfig::default(),
        )
        .await?;
        session
            .run(input(), Arc::new(Approval), CancellationToken::new())
            .await?;
        session.close().await?;
        for (operation, scope) in [
            (
                "route-only",
                GrantScope::Resources {
                    resources: vec![route.key()],
                },
            ),
            (
                "credential-only",
                GrantScope::Resources {
                    resources: vec![credential],
                },
            ),
        ] {
            binding(&fixture, TenantRole::TenantAdmin, scope, operation).await?;
            let denied = NativeSession::open_authenticated(
                app.clone(),
                headers.clone(),
                "scoped",
                &directory.path().join("not-readable"),
                ToolMode::ReadOnly,
                &HarnessConfig::default(),
            )
            .await;
            ensure!(
                denied.err().context("incomplete closure restored")?.code
                    == bitrouter_ai::core::protocol::ErrorCode::UnauthorizedScope
            );
        }
        ensure!(rows(&fixture).await? == 1 && provider.calls() == 1);
        Ok(())
    })
    .await
    .context("native scope matrix deadline")
    .and_then(|outcome| outcome);
    finish(&fixture, provider, checked).await
}

#[tokio::test]
async fn native_opener_rechecks_authority_after_discovery_waits_and_before_journal_insert()
-> Result<()> {
    for (point, change) in [
        (HostBoundary::BeforeTransport, "revoke"),
        (HostBoundary::BeforeTransport, "suspend"),
        (HostBoundary::BeforeTransport, "scope"),
        (HostBoundary::Resources, "revoke"),
        (HostBoundary::Resources, "credential-expiry"),
        (HostBoundary::Resources, "binding-expiry"),
    ] {
        let provider = Provider::start(false).await?;
        let directory = tempfile::tempdir()?;
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let (fixture, route) = file_fixture(&provider, directory.path()).await?;
        let gate = fixture.service.hold_host(point)?;
        let checked = tokio::time::timeout(TIMEOUT, async {
            let (headers, key) = ready(&fixture, &route).await?;
            let app = Arc::new(
                host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                    .with_tenancy(fixture.service.clone()),
            );
            let opened = AbortOnDropHandle::new(tokio::spawn(async move {
                NativeSession::open_authenticated(
                    app,
                    headers,
                    "wait",
                    &workspace,
                    ToolMode::ReadOnly,
                    &HarnessConfig::default(),
                )
                .await
            }));
            gate.entered.notified().await;
            ensure!(fixture.service.host_slots_for_test() == 0 && rows(&fixture).await? == 0);
            if change.ends_with("expiry") {
                // Advance only the real grant deadline across the held boundary;
                // no fake authorizer, revision change, sleep or public clock seam.
                let expired = crate::tenancy::model::Deadline::parse(
                    (Utc::now() - chrono::Duration::seconds(1))
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                        .try_into()?,
                )?;
                let mut published = fixture
                    .service
                    .snapshot
                    .write()
                    .map_err(|_| anyhow::anyhow!("snapshot lock"))?;
                let state = Arc::make_mut(&mut published);
                if change == "credential-expiry" {
                    state
                        .credentials
                        .get_mut(&key.credential_id)
                        .context("credential")?
                        .expires = expired;
                } else {
                    state
                        .bindings
                        .get_mut(&(fixture.tenant.clone(), fixture.binding.clone()))
                        .context("binding")?
                        .expires = expired;
                }
            } else {
                change_authority(&fixture, &key, &route, change).await?;
            }
            gate.release();
            ensure!(opened.await?.is_err());
            fixture.service.shutdown().await?;
            ensure!(fixture.service.host_slots_for_test() == 1);
            ensure!(
                fixture.service.cleanup.disposition()
                    != crate::tenancy::transaction::Disposition::Ambiguous
            );
            ensure!(rows(&fixture).await? == 0 && provider.calls() == 0);
            Ok(())
        })
        .await
        .context("native held-authority deadline")
        .and_then(|outcome| outcome);
        gate.release();
        finish(&fixture, provider, checked).await?;
    }
    Ok(())
}

#[tokio::test]
async fn checked_host_identity_cannot_cross_two_live_services() -> Result<()> {
    let provider = Provider::start(false).await?;
    let directory = tempfile::tempdir()?;
    let (fixture, route) = file_fixture(&provider, directory.path()).await?;
    let (foreign, _, _) = provision_protocol(&provider.url, "chat_completions").await?;
    let checked = async {
        let (headers, _) = ready(&fixture, &route).await?;
        let authority = fixture
            .service
            .session_authority(&headers, "incarnation")
            .await
            .map_err(failure)?;
        let slot = fixture
            .service
            .host_slot(authority.host())
            .map_err(failure)?;
        drop(slot);
        // The source remains eligible: rejection cannot be explained by an old
        // stopped Service or expired authority instead of the origin seal.
        authority.check().map_err(failure)?;
        let denied = foreign
            .service
            .host_slot(authority.host())
            .err()
            .context("cross-Service host accepted")?;
        ensure!(denied.0.code == ErrorCode::AuthorityUnavailable);
        let denied = foreign
            .service
            .load_native_journal(authority)
            .await
            .err()
            .context("cross-Service journal accepted")?
            .core();
        ensure!(denied.commit_status == bitrouter_ai::core::protocol::CommitStatus::NotCommitted);
        ensure!(foreign.service.host_slots_for_test() == 1 && rows(&foreign).await? == 0);
        ensure!(fixture.service.host_slots_for_test() == 1 && rows(&fixture).await? == 0);
        ensure!(provider.calls() == 0);
        Ok(())
    }
    .await;
    let foreign_closed = foreign.service.shutdown().await;
    let result = finish(&fixture, provider, checked).await;
    foreign_closed?;
    result
}

#[tokio::test]
async fn postcommit_expiry_suppresses_open_but_exact_readback_releases_the_persisted_owner()
-> Result<()> {
    let provider = Provider::start(false).await?;
    let directory = tempfile::tempdir()?;
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace)?;
    let (fixture, route) = file_fixture(&provider, directory.path()).await?;
    let resources = fixture.service.hold_host(HostBoundary::Resources)?;
    let checked = tokio::time::timeout(TIMEOUT, async {
        let (headers, key) = ready(&fixture, &route).await?;
        let app = Arc::new(
            host_app(&fixture, &provider.url, Arc::new(HostSpies::default()))?
                .with_tenancy(fixture.service.clone()),
        );
        let opener = AbortOnDropHandle::new(tokio::spawn(async move {
            NativeSession::open_authenticated(
                app,
                headers,
                "committed",
                &workspace,
                ToolMode::ReadOnly,
                &HarnessConfig::default(),
            )
            .await
        }));
        resources.entered.notified().await;
        let commit = fixture
            .service
            .cleanup
            .hold(crate::tenancy::transaction::Phase::Committing);
        resources.release();
        commit.entered.notified().await;
        let expired = crate::tenancy::model::Deadline::parse(
            (Utc::now() - chrono::Duration::seconds(1))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                .try_into()?,
        )?;
        {
            let mut published = fixture
                .service
                .snapshot
                .write()
                .map_err(|_| anyhow::anyhow!("snapshot lock"))?;
            Arc::make_mut(&mut published)
                .credentials
                .get_mut(&key.credential_id)
                .context("credential")?
                .expires = expired;
        }
        fixture.service.cleanup.release_gate();
        let rejection = opener
            .await?
            .err()
            .context("expired opener delivered a session")?;
        ensure!(rejection.commit_status == bitrouter_ai::core::protocol::CommitStatus::Committed);
        // The reply is not a cleanup acknowledgement. Wait for the actual host
        // slot to return after exact readback, resource close and release CAS.
        while fixture.service.host_slots_for_test() == 0 {
            tokio::task::yield_now().await;
        }
        ensure!(
            fixture.service.current().is_ok(),
            "confirmed readback unnecessarily quarantined the host"
        );
        let row = fixture
            .db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT revision, payload FROM bro_managed_sessions",
            ))
            .await?
            .context("committed native row")?;
        ensure!(
            row.try_get::<i64>("", "revision")? == 2,
            "initial write was retried instead of reconciled and released"
        );
        let saved: Value = serde_json::from_str(&row.try_get::<String>("", "payload")?)?;
        ensure!(saved["journal"]["released"] == true);
        ensure!(saved["journal"]["grant"]["execution_epoch"] == 1);
        ensure!(provider.calls() == 0);
        Ok(())
    })
    .await
    .context("postcommit native cleanup deadline")
    .and_then(|outcome| outcome);
    resources.release();
    fixture.service.cleanup.release_gate();
    finish(&fixture, provider, checked).await
}
