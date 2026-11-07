use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    authority::{
        BindingPut, CredentialCreate, CredentialIssued, GrantScope, Permission, PrincipalPut,
        TenantPut, TenantRole, TenantState,
    },
    identity::{BindingId, InstallationId, PrincipalId, TenantId},
    operation::{ErrorCode, Mutation, OperationResult},
};
use chrono::Utc;

use super::{Service, command::Command, model::Caller, store::StoredResponse};

mod authority;
mod authority_ingress;
mod capacity_directory;
#[cfg(unix)]
mod capacity_history;
mod configuration;
mod conformance;
mod directory;
#[cfg(feature = "core")]
mod eval;
pub(in crate::tenancy) mod execution;
mod http;
mod learning_startup;
#[cfg(not(feature = "core"))]
mod no_core;
mod resources;

#[tokio::test]
async fn indexed_authentication_keeps_live_revoke_publication_after_read_cleanup() -> Result<()> {
    let fixture = Fixture::new().await?;
    let credential = fixture.issue("indexed-auth").await?;
    let secret = credential.secret.as_deref().context("issuance secret")?;
    let authenticated = fixture
        .service
        .authenticate_live(secret)
        .await
        .map_err(failure)?;
    fixture
        .service
        .authority
        .read()
        .map_err(|code| failure(code.into()))?
        .check(&authenticated.lease, |view| {
            ensure_authorized(view.facts(), &authenticated.caller, &fixture.tenant)
        })
        .map_err(|code| failure(code.into()))?;
    result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Revoke(
                    fixture.tenant.clone(),
                    credential.credential_id,
                    Mutation {
                        operation_id: "indexed-revoke".try_into()?,
                        expected_revision: credential.credential_revision,
                        installation_id: fixture.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    let denied = fixture
        .service
        .authority
        .read()
        .map_err(|code| failure(code.into()))?
        .check(&authenticated.lease, |view| {
            ensure_authorized(view.facts(), &authenticated.caller, &fixture.tenant)
        });
    ensure!(denied == Err(ErrorCode::Unauthorized));
    ensure!(
        matches!(fixture.service.authenticate_live(secret).await, Err(error) if error.0.code == ErrorCode::Unauthorized)
    );
    fixture.service.shutdown().await?;
    Ok(())
}

fn ensure_authorized(
    state: &super::model::Snapshot,
    caller: &Caller,
    tenant: &TenantId,
) -> std::result::Result<(), ErrorCode> {
    state.authorize(
        caller,
        tenant,
        Permission::ExecutionSubmit,
        &super::mutation::tenant_scope(),
        Utc::now(),
    )
}

#[cfg(all(unix, feature = "otel-http"))]
#[tokio::test]
async fn process_stop_fences_sends_without_waiting_for_database_or_accounting() -> Result<()> {
    use sea_orm::TransactionTrait;
    use std::{
        sync::{atomic::Ordering, mpsc},
        time::Duration,
    };

    let fixture = Fixture::new().await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let before = fixture.service.current().map_err(failure)?;
    ensure!(fixture.service.dispatch_ready(&before));
    let expected_capability = fixture
        .service
        .capability_revision(&before)
        .map_err(|code| failure(code.into()))?;
    let credential = fixture.issue("stop-reader").await?;
    let authenticated = fixture
        .service
        .authenticate_live(credential.secret.as_deref().context("issuance secret")?)
        .await
        .map_err(failure)?;
    let transaction = fixture.db.begin().await?;
    let service = &fixture.service;
    let stopped = std::thread::scope(|scope| -> Result<()> {
        let wait = Duration::from_secs(5);
        let (accounting_held_tx, accounting_held_rx) = mpsc::channel();
        let (authority_held_tx, authority_held_rx) = mpsc::channel();
        let (stop_start_tx, stop_start_rx) = mpsc::channel();
        let (stop_done_tx, stop_done_rx) = mpsc::channel();
        let reader = scope.spawn(move || -> Result<bool> {
            accounting_held_rx.recv_timeout(wait)?;
            let mut authority = service
                .authority
                .read()
                .map_err(|_| anyhow::anyhow!("authority read lock unavailable"))?;
            authority_held_tx.send(())?;
            // Match the publication guard held by dispatch authorization.
            authority
                .check(&authenticated.lease, |view| {
                    if service.capability_revision(view.facts())? != expected_capability {
                        return Err(ErrorCode::CapabilityMismatch);
                    }
                    Ok(service.dispatch_ready(view.facts()))
                })
                .map_err(|code| failure(code.into()))
        });
        let stopper = scope.spawn(move || -> Result<()> {
            stop_start_rx.recv_timeout(wait)?;
            stop_done_tx
                .send(service.stop_dispatch())
                .context("stop observer disconnected")
        });
        let observed = service.admission.with_observation_lock_for_test(|| {
            accounting_held_tx.send(())?;
            authority_held_rx
                .recv_timeout(wait)
                .context("authority reader did not acquire its guard")?;
            stop_start_tx.send(())?;
            stop_done_rx
                .recv_timeout(wait)
                .context("stop waited for accounting through the authority reader")?
        });
        // Release accounting before joining, including on timeout, so the old
        // blocking readiness path fails this test instead of hanging teardown.
        let reader_result = reader
            .join()
            .map_err(|_| anyhow::anyhow!("authority reader thread failed"));
        let stopper_result = stopper
            .join()
            .map_err(|_| anyhow::anyhow!("stop thread failed"));
        observed.map_err(|_| anyhow::anyhow!("accounting lock unavailable"))??;
        ensure!(
            reader_result??,
            "accounting contention denied healthy dispatch"
        );
        stopper_result??;
        Ok(())
    });
    transaction.rollback().await?;
    stopped?;
    // Stopping dispatch does not manufacture a receipt fence.
    ensure!(fixture.service.admission.receipts_ready());
    let after = fixture.service.current().map_err(failure)?;
    ensure!(!fixture.service.dispatch_ready(&after));
    fixture.service.shutdown().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn startup_applies_capacity_before_publishing_the_service() -> Result<()> {
    let fixture = Fixture::new().await?;
    let issued = fixture.issue("capacity-safety").await?;
    fixture.service.shutdown().await?;
    let mut settings = crate::config::tenancy::TenancyConfig::default();
    settings.capacity.ingress.requests = 1;
    settings.capacity.ingress.bytes = 32;
    settings.capacity.management.queue = 3;
    settings.capacity.management.safety_queue = 2;
    settings.capacity.management.reads = 1;
    settings.capacity.execution.instance_active = 17;
    let service = Service::open_profile(
        fixture.db.clone(),
        None,
        &settings,
        &crate::config::ContinuationConfig::default(),
    )
    .await?;
    let first = service
        .admission
        .ingress(32)
        .map_err(|code| failure(code.into()))?;
    ensure!(matches!(
        service.admission.ingress(1),
        Err(ErrorCode::CapacityExceeded)
    ));
    drop(first);
    ensure!(service.admission.ingress(32).is_ok());
    let read = service.read_permit().map_err(failure)?;
    ensure!(
        matches!(service.read_permit(), Err(error) if error.0.code == ErrorCode::CapacityExceeded)
    );
    drop(read);
    ensure!(service.read_permit().is_ok());
    ensure!(service.commands.max_capacity() == 3 && service.safety.max_capacity() == 2);
    ensure!(service.completions.max_capacity() == 17);
    let principal = || -> Result<Command> {
        Ok(Command::Principal(
            "capacity-principal".try_into()?,
            PrincipalPut {
                operation_id: "capacity-principal-put".try_into()?,
                expected_revision: None,
                issuer: "capacity-fixture".into(),
                subject: "capacity-principal".into(),
                installation_id: fixture.installation.clone(),
            },
        ))
    };
    let revoke = || -> Result<Command> {
        Ok(Command::Revoke(
            fixture.tenant.clone(),
            issued.credential_id.clone(),
            Mutation {
                operation_id: "capacity-safety-revoke".try_into()?,
                expected_revision: issued.credential_revision.clone(),
                installation_id: fixture.installation.clone(),
            },
        ))
    };
    let gate = service.cleanup.hold(super::transaction::Phase::Active);
    let mut blocked = Box::pin(service.execute(fixture.operator.clone(), principal()?));
    let mut queued = Vec::new();
    let mut ordinary = Box::pin(service.execute(fixture.operator.clone(), principal()?));
    let checked: Result<()> = async {
        ensure!(futures::poll!(&mut blocked).is_pending());
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified()).await?;
        // Retain a real channel permit while the first revoke enters. Releasing
        // just that permit must admit the next revoke while the first stays queued.
        let mut held = Some(service.safety.clone().try_reserve_owned()?);
        for remaining in [1, 0] {
            ensure!(service.safety.capacity() == 1);
            let mut request = Box::pin(service.execute(fixture.operator.clone(), revoke()?));
            ensure!(futures::poll!(&mut request).is_pending());
            queued.push(request);
            ensure!(service.safety.capacity() == 0);
            drop(held.take());
            ensure!(service.safety.capacity() == remaining);
        }
        let mut excess = Box::pin(service.execute(fixture.operator.clone(), revoke()?));
        ensure!(matches!(futures::poll!(&mut excess), std::task::Poll::Ready(Err(error)) if error.0.code == ErrorCode::CapacityExceeded));
        ensure!(service.safety.capacity() == 0 && service.commands.capacity() == 3);
        ensure!(futures::poll!(&mut ordinary).is_pending());
        ensure!(
            service.commands.capacity() == 2,
            "safety pressure consumed ordinary capacity"
        );
        Ok(())
    }
    .await;
    service.cleanup.release_gate();
    let settled = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        result(blocked.await.map_err(failure)?)?;
        for request in queued {
            result(request.await.map_err(failure)?)?;
        }
        result(ordinary.await.map_err(failure)?)?;
        result(
            service
                .execute(fixture.operator.clone(), revoke()?)
                .await
                .map_err(failure)?,
        )?;
        ensure!(service.safety.capacity() == 2 && service.commands.capacity() == 3);
        Ok::<_, anyhow::Error>(())
    })
    .await;
    service.shutdown().await?;
    checked?;
    settled??;

    settings.capacity.caches.execution_bytes = 1;
    let refused = Service::open_profile(
        fixture.db.clone(),
        None,
        &settings,
        &crate::config::ContinuationConfig::default(),
    )
    .await;
    ensure!(refused.is_err());
    // A construction failure must not leave an actor competing for the database.
    let restarted = Service::open(fixture.db, None).await?;
    restarted.shutdown().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn failed_assembly_does_not_leave_a_detached_authority() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let url = format!(
        "sqlite://{}",
        directory.path().join("authority.db").display()
    );
    let config = crate::config::parse_resolved(&format!(
        "inherit_defaults: false\ntenancy: {{enabled: true}}\nserver: {{skip_auth: false}}\ndatabase: {{url: '{url}'}}\ncontrol:\n  enabled: true\n  credentials:\n    - id: test-operator\n      token_env: TEST_TENANCY_OPERATOR\n      scopes: [control:tenancy]\nplugins:\n  bitrouter-policy:\n    policy_dir: /dev/null\n"
    ))?;
    let failed = crate::assemble::build_app_with_path(
        &config,
        Some(&directory.path().join("bitrouter.yaml")),
    )
    .await
    .err()
    .context("invalid policy store unexpectedly assembled")?;
    ensure!(
        failed
            .to_string()
            .contains("loading policies from /dev/null"),
        "assembly failed before the intended policy-store check: {failed:#}"
    );
    // SQLx returns checked-out connections in spawned tasks, which retain the
    // pool's ownership hook until cleanup finishes. Yield this current-thread
    // runtime while waiting for the real lock; a leaked actor still times out.
    // https://github.com/launchbadge/sqlx/blob/v0.8.6/sqlx-core/src/pool/connection.rs
    let retry_owner = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match super::ownership::acquire(&url) {
                Ok(owner) => break Ok(owner),
                Err(error)
                    if matches!(
                        error.downcast_ref::<std::fs::TryLockError>(),
                        Some(std::fs::TryLockError::WouldBlock)
                    ) =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(error) => break Err(error),
            }
        }
    })
    .await
    .context("failed assembly retained authority ownership past the cleanup deadline")??;
    ensure!(
        super::ownership::acquire(&url).is_err(),
        "retry owner did not exclude another authority"
    );
    let db = crate::db::connect_tenancy(&url, &retry_owner).await?;
    let installation = super::ownership::has_installation(&db).await;
    db.close().await?;
    ensure!(
        !installation?,
        "failed assembly initialized authority before finishing validation"
    );
    drop(retry_owner);
    Ok(())
}

struct Fixture {
    service: Service,
    db: sea_orm::DatabaseConnection,
    operator: Caller,
    installation: InstallationId,
    tenant: TenantId,
    principal: PrincipalId,
    binding: BindingId,
    binding_result: OperationResult,
}

#[tokio::test]
async fn shutdown_settles_admitted_commands_after_the_client_disconnects() -> Result<()> {
    use sea_orm::TransactionTrait;
    let fixture = Fixture::new().await?;
    let transaction = fixture.db.begin().await?;
    let principal: PrincipalId = "principal-before-shutdown".try_into()?;
    let input = PrincipalPut {
        operation_id: "admitted-before-shutdown".try_into()?,
        expected_revision: None,
        issuer: "test".into(),
        subject: "shutdown-subject".into(),
        installation_id: fixture.installation.clone(),
    };
    let command = || Command::Principal(principal.clone(), input.clone());
    let (reply, receive) = tokio::sync::oneshot::channel();
    fixture
        .service
        .commands
        .try_send(
            super::actor::Message {
                native_adapter_configured: false,
                caller: fixture.operator.clone(),
                command: command(),
                reply,
            }
            .into(),
        )
        .map_err(|_| anyhow::anyhow!("test command admission failed"))?;
    drop(receive);
    let service = fixture.service.clone();
    let shutdown = tokio::spawn(async move { service.shutdown().await });
    fixture.service.stop.cancelled().await;
    ensure!(!shutdown.is_finished());
    let rejected = fixture
        .service
        .execute(fixture.operator.clone(), command())
        .await;
    ensure!(matches!(rejected, Err(error) if error.0.code == ErrorCode::AuthorityUnavailable));
    transaction.commit().await?;
    shutdown.await??;
    let Caller::Installation(principal) = &fixture.operator else {
        anyhow::bail!("expected installation operator");
    };
    let stored = super::store::operation(
        &fixture.db,
        &fixture.installation,
        principal,
        &input.operation_id,
    )
    .await?
    .context("admitted command was lost during shutdown")?;
    ensure!(
        result(stored.response)?.status
            == bitrouter_ai::tenancy::operation::OperationStatus::Applied
    );
    ensure!(fixture.service.current().is_err());
    // Repeated cleanup observes the same completed task, without aborting it.
    fixture.service.shutdown().await?;
    Ok(())
}

fn failure(error: super::error::Failure) -> anyhow::Error {
    anyhow::anyhow!("tenancy failure: {:?}", error.0.code)
}

fn result(response: StoredResponse) -> Result<OperationResult> {
    match response {
        StoredResponse::Operation(result) => Ok(result),
        _ => anyhow::bail!("expected an operation result"),
    }
}

fn issued(response: StoredResponse) -> Result<CredentialIssued> {
    match response {
        StoredResponse::Issued(issued) => Ok(issued),
        _ => anyhow::bail!("expected credential issuance"),
    }
}

fn expiry() -> Result<bitrouter_ai::tenancy::identity::Timestamp> {
    Ok((Utc::now() + chrono::Duration::days(1))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        .try_into()?)
}

impl Fixture {
    async fn new() -> Result<Self> {
        let db = crate::db::connect("sqlite::memory:").await?;
        Self::with_database(db, &mut |_| {}).await
    }

    // Test-only injection before Service construction, never a post-admission DB swap.
    // The callback lets isolated backend tests retain every actor outside cancellation.
    async fn with_database(
        db: sea_orm::DatabaseConnection,
        started: &mut (impl FnMut(&Service) + ?Sized),
    ) -> Result<Self> {
        crate::db::run_migrations(&db).await?;
        let service = Service::open(db.clone(), None).await?;
        started(&service);
        let installation = service.current().map_err(failure)?.installation.clone();
        let operator =
            Caller::Installation(super::operator_principal("test-operator").map_err(failure)?);
        let tenant: TenantId = "tenant-a".try_into()?;
        let principal: PrincipalId = "principal-a".try_into()?;
        let binding: BindingId = "binding-a".try_into()?;
        result(
            service
                .execute(
                    operator.clone(),
                    Command::Tenant(
                        tenant.clone(),
                        TenantPut {
                            operation_id: "create-tenant".try_into()?,
                            expected_revision: None,
                            state: TenantState::Active,
                            installation_id: installation.clone(),
                        },
                    ),
                )
                .await
                .map_err(failure)?,
        )?;
        result(
            service
                .execute(
                    operator.clone(),
                    Command::Principal(
                        principal.clone(),
                        PrincipalPut {
                            operation_id: "create-principal".try_into()?,
                            expected_revision: None,
                            issuer: "test".into(),
                            subject: "subject".into(),
                            installation_id: installation.clone(),
                        },
                    ),
                )
                .await
                .map_err(failure)?,
        )?;
        let binding_result = result(
            service
                .execute(
                    operator.clone(),
                    Command::Binding(
                        tenant.clone(),
                        binding.clone(),
                        BindingPut {
                            operation_id: "create-binding".try_into()?,
                            expected_revision: None,
                            principal_id: principal.clone(),
                            role: TenantRole::TenantAdmin,
                            scope: super::mutation::tenant_scope(),
                            expires_at: expiry()?,
                            expected_management_owner: None,
                            installation_id: installation.clone(),
                        },
                    ),
                )
                .await
                .map_err(failure)?,
        )?;
        Ok(Self {
            service,
            db,
            operator,
            installation,
            tenant,
            principal,
            binding,
            binding_result,
        })
    }

    async fn issuance(&self, operation: &str) -> Result<CredentialCreate> {
        let state = self.service.current().map_err(failure)?;
        let binding =
            super::store::directory::binding_in(&self.db, &self.tenant, &self.binding, 64 * 1024)
                .await?
                .context("binding missing")?;
        Ok(CredentialCreate {
            operation_id: operation.try_into()?,
            expected_authority_revision: state.revision,
            principal_id: self.principal.clone(),
            permissions: TenantRole::TenantAdmin.permissions().to_vec(),
            scope: super::mutation::tenant_scope(),
            expires_at: binding.expires.wire,
            rotate_credential_id: None,
            binding_id: self.binding.clone(),
            installation_id: self.installation.clone(),
        })
    }

    async fn issue(&self, operation: &str) -> Result<CredentialIssued> {
        issued(
            self.service
                .execute(
                    self.operator.clone(),
                    Command::Issue(self.tenant.clone(), self.issuance(operation).await?),
                )
                .await
                .map_err(failure)?,
        )
    }
}

#[tokio::test]
async fn concurrent_issuance_returns_one_secret_and_replay_survives_restart() -> Result<()> {
    let fixture = Fixture::new().await?;
    let input = fixture.issuance("issue-once").await?;
    let (first, second) = tokio::join!(
        fixture.service.execute(
            fixture.operator.clone(),
            Command::Issue(fixture.tenant.clone(), input.clone())
        ),
        fixture.service.execute(
            fixture.operator.clone(),
            Command::Issue(fixture.tenant.clone(), input.clone())
        ),
    );
    let first = issued(first.map_err(failure)?)?;
    let second = issued(second.map_err(failure)?)?;
    ensure!(first.credential_id == second.credential_id);
    ensure!(usize::from(first.secret.is_some()) + usize::from(second.secret.is_some()) == 1);
    let stored = super::store::operation(
        &fixture.db,
        &fixture.installation,
        fixture
            .service
            .current()
            .map_err(failure)?
            .principal(&fixture.operator, Utc::now())
            .map_err(|code| anyhow::anyhow!("{code:?}"))?,
        &input.operation_id,
    )
    .await?
    .context("operation missing")?;
    ensure!(issued(stored.response)?.secret.is_none());
    let alive = fixture.service.live.clone();
    drop(fixture.service);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while alive.load(std::sync::atomic::Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let reopened = Service::open(fixture.db, None).await?;
    let replay = issued(
        reopened
            .execute(fixture.operator, Command::Issue(fixture.tenant, input))
            .await
            .map_err(failure)?,
    )?;
    ensure!(replay.credential_id == first.credential_id && replay.secret.is_none());
    Ok(())
}

#[tokio::test]
async fn revoked_credentials_are_unusable_before_success_is_observed() -> Result<()> {
    let fixture = Fixture::new().await?;
    let credential = fixture.issue("issue-revoke").await?;
    let secret = credential
        .secret
        .as_deref()
        .context("first response omitted secret")?;
    let caller = fixture
        .service
        .authenticate(secret)
        .await
        .map_err(failure)?;
    fixture
        .service
        .caller_facts_for_test(&caller)
        .await
        .map_err(failure)?
        .authorize(
            &caller,
            &fixture.tenant,
            Permission::ExecutionSubmit,
            &super::mutation::tenant_scope(),
            Utc::now(),
        )
        .map_err(|code| anyhow::anyhow!("{code:?}"))?;
    let revoked = result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Revoke(
                    fixture.tenant.clone(),
                    credential.credential_id,
                    Mutation {
                        operation_id: "revoke".try_into()?,
                        expected_revision: credential.credential_revision,
                        installation_id: fixture.installation,
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(fixture.service.authenticate(secret).await.is_err());
    let current = fixture.service.current().map_err(failure)?;
    ensure!(Some(current.revision) == revoked.authority_revision);
    ensure!(
        fixture
            .service
            .caller_facts_for_test(&caller)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn binding_reduction_intersects_existing_credentials() -> Result<()> {
    let fixture = Fixture::new().await?;
    let credential = fixture.issue("issue-reduction").await?;
    let caller = fixture
        .service
        .authenticate(credential.secret.as_deref().context("missing secret")?)
        .await
        .map_err(failure)?;
    let principal = fixture
        .service
        .current()
        .map_err(failure)?
        .principal(&fixture.operator, Utc::now())
        .map_err(|code| anyhow::anyhow!("{code:?}"))?
        .clone();
    result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Binding(
                    fixture.tenant.clone(),
                    fixture.binding.clone(),
                    BindingPut {
                        operation_id: "reduce-binding".try_into()?,
                        expected_revision: fixture.binding_result.resource_revision,
                        principal_id: fixture.principal,
                        role: TenantRole::Viewer,
                        scope: super::mutation::tenant_scope(),
                        expires_at: expiry()?,
                        expected_management_owner: Some(principal),
                        installation_id: fixture.installation,
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    let state = fixture
        .service
        .caller_facts_for_test(&caller)
        .await
        .map_err(failure)?;
    ensure!(
        state.authorize(
            &caller,
            &fixture.tenant,
            Permission::ExecutionSubmit,
            &super::mutation::tenant_scope(),
            Utc::now()
        ) == Err(ErrorCode::Forbidden)
    );
    ensure!(
        state
            .authorize(
                &caller,
                &fixture.tenant,
                Permission::ResourceRead,
                &super::mutation::tenant_scope(),
                Utc::now()
            )
            .is_ok()
    );
    Ok(())
}

#[test]
fn resource_scope_never_becomes_tenant_wide_or_crosses_owner_namespaces() -> Result<()> {
    use bitrouter_ai::tenancy::authority::{OwnerKind, ResourceKey, ResourceKind};
    let tenant: TenantId = "tenant-a".try_into()?;
    let key = ResourceKey {
        owner_kind: OwnerKind::Tenant,
        owner_id: "tenant-a".try_into()?,
        resource_kind: ResourceKind::Route,
        resource_id: "route".try_into()?,
    };
    let narrow = GrantScope::Resources {
        resources: vec![key.clone()],
    };
    ensure!(!super::authority::scope_subset(
        &super::mutation::tenant_scope(),
        &narrow,
        &tenant
    ));
    let mut foreign = key.clone();
    foreign.owner_id = "tenant-b".try_into()?;
    ensure!(!super::authority::covers_key(
        &super::mutation::tenant_scope(),
        &tenant,
        &foreign
    ));
    let mut pool = key;
    pool.owner_kind = OwnerKind::OperatorPool;
    ensure!(!super::authority::covers_key(
        &super::mutation::tenant_scope(),
        &tenant,
        &pool
    ));
    ensure!(super::authority::covers_key(
        &GrantScope::Tenant {
            operator_pools: vec![pool.clone()]
        },
        &tenant,
        &pool
    ));
    Ok(())
}

#[tokio::test]
async fn a_failed_management_read_fences_cached_authentication() -> Result<()> {
    let fixture = Fixture::new().await?;
    let credential = fixture.issue("issue-before-storage-failure").await?;
    let secret = credential
        .secret
        .as_deref()
        .context("missing issuance secret")?;
    ensure!(fixture.service.authenticate(secret).await.is_ok());
    fixture.db.close().await?;
    let read = fixture
        .service
        .operation(&fixture.operator, &"create-tenant".try_into()?)
        .await;
    ensure!(matches!(read, Err(error) if error.0.code == ErrorCode::StorageUnavailable));
    ensure!(
        matches!(fixture.service.authenticate(secret).await, Err(error) if error.0.code == ErrorCode::AuthorityUnavailable)
    );
    Ok(())
}

#[tokio::test]
async fn command_identity_is_not_partitioned_by_target_or_kind() -> Result<()> {
    let fixture = Fixture::new().await?;
    let response = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Tenant(
                "tenant-b".try_into()?,
                TenantPut {
                    operation_id: "create-tenant".try_into()?,
                    expected_revision: None,
                    state: TenantState::Active,
                    installation_id: fixture.installation.clone(),
                },
            ),
        )
        .await;
    ensure!(matches!(response, Err(error) if error.0.code == ErrorCode::OperationConflict));
    let response = fixture
        .service
        .execute(
            fixture.operator,
            Command::Principal(
                "principal-b".try_into()?,
                PrincipalPut {
                    operation_id: "create-tenant".try_into()?,
                    expected_revision: None,
                    issuer: "other".into(),
                    subject: "other".into(),
                    installation_id: fixture.installation,
                },
            ),
        )
        .await;
    ensure!(matches!(response, Err(error) if error.0.code == ErrorCode::OperationConflict));
    ensure!(
        super::store::directory::tenant_in(&fixture.db, &"tenant-b".try_into()?, 4096)
            .await?
            .is_none()
    );
    ensure!(
        super::store::directory::principal_in(&fixture.db, &"principal-b".try_into()?, 4096)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn protected_http_and_sdk_share_real_authority_without_dispatch_eligibility() -> Result<()> {
    use bitrouter_ai::control::ControlScope;
    use bitrouter_sdk::{
        gateway::HttpVersion,
        tenancy::{TenancyClient, TenancyOptions},
    };
    let fixture = Fixture::new().await?;
    let token = "o".repeat(32);
    let operator = crate::remote_control::auth::ControlAuth::from_lookup(
        &crate::config::ControlConfig {
            credentials: vec![crate::config::ControlCredentialConfig {
                id: "test-operator".into(),
                token_env: "TEST_OPERATOR_TOKEN".into(),
                scopes: vec![ControlScope::Tenancy],
            }],
            ..Default::default()
        },
        |_| Some(token.clone()),
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let router = super::http::router(fixture.service.clone(), operator);
    let stop = tokio_util::sync::CancellationToken::new();
    let _cleanup = stop.clone().drop_guard();
    let shutdown = stop.clone();
    let task = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
    });
    let versions = [
        HttpVersion::Http1Only,
        #[cfg(feature = "http2")]
        HttpVersion::Http2PriorKnowledge,
    ];
    for (index, version) in versions.into_iter().enumerate() {
        let client = TenancyClient::with_options(
            &endpoint,
            &token,
            TenancyOptions {
                http_version: version,
                ..Default::default()
            },
        )?;
        let capabilities = client.capabilities().await?;
        ensure!(capabilities.installation_id == fixture.installation);
        ensure!(
            !capabilities.dispatch_enabled
                && !capabilities.strict_receipts
                && !capabilities.revision_bound_dispatch
        );
        // A remote operator can derive its management identity solely from its
        // public configuration, without a database read or private server helper.
        let mut binding = BindingPut {
            operation_id: format!("http-binding-create-{index}").try_into()?,
            expected_revision: None,
            principal_id: fixture.principal.clone(),
            role: TenantRole::Viewer,
            scope: super::mutation::tenant_scope(),
            expires_at: expiry()?,
            expected_management_owner: None,
            installation_id: fixture.installation.clone(),
        };
        let binding_id: BindingId = format!("http-binding-{index}").try_into()?;
        let created = client
            .put_binding(&fixture.tenant, &binding_id, &binding)
            .await?;
        binding.operation_id = format!("http-binding-update-{index}").try_into()?;
        binding.expected_revision = created.resource_revision;
        binding.expected_management_owner =
            Some(PrincipalId::for_control_credential("test-operator")?);
        ensure!(
            binding
                .expected_management_owner
                .as_ref()
                .map(PrincipalId::as_str)
                == Some("operator.test-operator")
        );
        binding.role = TenantRole::Editor;
        let updated = client
            .put_binding(&fixture.tenant, &binding_id, &binding)
            .await?;
        ensure!(updated.resource_revision != binding.expected_revision);
        let input = fixture.issuance(&format!("http-issue-{index}")).await?;
        let credential = client.issue_credential(&fixture.tenant, &input).await?;
        let replay = client.issue_credential(&fixture.tenant, &input).await?;
        ensure!(
            credential.secret.is_some()
                && replay.secret.is_none()
                && credential.credential_id == replay.credential_id
        );
        let observed = client
            .credential(
                &fixture.installation,
                &fixture.tenant,
                &credential.credential_id,
            )
            .await?;
        ensure!(
            observed.credential_revision == credential.credential_revision
                && observed.state == bitrouter_ai::tenancy::authority::CredentialState::Active
        );
        let tenant_client = TenancyClient::new(
            &endpoint,
            credential
                .secret
                .as_deref()
                .context("missing initial secret")?,
        )?;
        ensure!(
            tenant_client
                .put_tenant(
                    &fixture.tenant,
                    &TenantPut {
                        operation_id: format!("tenant-cannot-stop-{index}").try_into()?,
                        expected_revision: None,
                        state: TenantState::Suspended,
                        installation_id: fixture.installation.clone(),
                    }
                )
                .await
                .is_err()
        );
        let observed = client
            .active_revision(&fixture.installation, &fixture.tenant)
            .await?;
        let mut input = fixture.configuration_input(&format!("http-config-{index}"))?;
        input.expected_authority_revision = observed.authority_revision;
        let stored = client.put_configuration(&fixture.tenant, &input).await?;
        let recovered = client
            .configuration(&fixture.installation, &fixture.tenant, &stored.revision_id)
            .await?;
        ensure!(recovered.document == input.document);
        let operation = client
            .operation(&fixture.installation, &input.operation_id)
            .await?;
        let current = client
            .active_revision(&fixture.installation, &fixture.tenant)
            .await?;
        ensure!(
            current.authority_revision
                == operation
                    .authority_revision
                    .context("configuration result authority missing")?
        );
        ensure!(current.revision_id == observed.revision_id);
        let activation = bitrouter_ai::tenancy::configuration::Activation {
            operation_id: format!("http-activate-{index}").try_into()?,
            expected_active_revision: current.revision_id,
            revision_id: stored.revision_id.clone(),
            expected_authority_revision: current.authority_revision,
            installation_id: fixture.installation.clone(),
        };
        client.activate(&fixture.tenant, &activation).await?;
        ensure!(
            client
                .active_revision(&fixture.installation, &fixture.tenant)
                .await?
                .revision_id
                == Some(stored.revision_id)
        );
    }
    stop.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(5), task).await???;
    Ok(())
}

#[tokio::test]
async fn directory_growth_crosses_the_old_tenant_ceiling_and_restarts() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    for index in 0..40 {
        result(
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Tenant(
                        format!("growth-{index:04}").try_into()?,
                        TenantPut {
                            operation_id: format!("growth-create-{index:04}").try_into()?,
                            expected_revision: None,
                            state: TenantState::Active,
                            installation_id: fixture.installation.clone(),
                        },
                    ),
                )
                .await
                .map_err(failure)?,
        )?;
    }
    let before = fixture.service.current().map_err(failure)?;
    ensure!(!before.saturated);
    fixture.service.shutdown().await?;
    fixture.service = Service::open(fixture.db.clone(), None).await?;
    ensure!(fixture.service.current().map_err(failure)?.revision == before.revision);
    for index in [0, 31, 32, 39] {
        let tenant: TenantId = format!("growth-{index:04}").try_into()?;
        let stored = super::store::directory::tenant_in(&fixture.db, &tenant, 4096)
            .await?
            .context("created tenant missing after restart")?;
        result(
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Tenant(
                        tenant,
                        TenantPut {
                            operation_id: format!("growth-suspend-{index:04}").try_into()?,
                            expected_revision: Some(stored.revision),
                            state: TenantState::Suspended,
                            installation_id: fixture.installation.clone(),
                        },
                    ),
                )
                .await
                .map_err(failure)?,
        )?;
    }
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn same_principal_in_another_tenant_cannot_read_or_replay_a_saved_command() -> Result<()> {
    let fixture = Fixture::new().await?;
    let credential_a = fixture.issue("issue-a").await?;
    let caller_a = fixture
        .service
        .authenticate(
            credential_a
                .secret
                .as_deref()
                .context("missing first secret")?,
        )
        .await
        .map_err(failure)?;
    let binding = BindingPut {
        operation_id: "tenant-owned-command".try_into()?,
        expected_revision: None,
        principal_id: fixture.principal.clone(),
        role: TenantRole::Viewer,
        scope: super::mutation::tenant_scope(),
        expires_at: credential_a.expires_at.clone(),
        expected_management_owner: None,
        installation_id: fixture.installation.clone(),
    };
    result(
        fixture
            .service
            .execute(
                caller_a.clone(),
                Command::Binding(
                    fixture.tenant.clone(),
                    "delegated".try_into()?,
                    binding.clone(),
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    let tenant_b: TenantId = "tenant-b".try_into()?;
    result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Tenant(
                    tenant_b.clone(),
                    TenantPut {
                        operation_id: "create-b".try_into()?,
                        expected_revision: None,
                        state: TenantState::Active,
                        installation_id: fixture.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    let mut grant_b = binding.clone();
    grant_b.operation_id = "bind-b".try_into()?;
    grant_b.role = TenantRole::TenantAdmin;
    result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Binding(tenant_b.clone(), "binding-b".try_into()?, grant_b),
            )
            .await
            .map_err(failure)?,
    )?;
    let mut request_b = fixture.issuance("issue-b").await?;
    request_b.binding_id = "binding-b".try_into()?;
    let credential_b = issued(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Issue(tenant_b, request_b),
            )
            .await
            .map_err(failure)?,
    )?;
    let caller_b = fixture
        .service
        .authenticate(
            credential_b
                .secret
                .as_deref()
                .context("missing second secret")?,
        )
        .await
        .map_err(failure)?;
    let read = fixture
        .service
        .operation(&caller_b, &binding.operation_id)
        .await;
    ensure!(matches!(read, Err(error) if error.0.code == ErrorCode::Forbidden));
    let replay = fixture
        .service
        .execute(
            caller_b,
            Command::Binding(
                fixture.tenant.clone(),
                "delegated".try_into()?,
                binding.clone(),
            ),
        )
        .await;
    ensure!(matches!(replay, Err(error) if error.0.code == ErrorCode::Forbidden));
    ensure!(
        fixture
            .service
            .operation(&caller_a, &binding.operation_id)
            .await
            .is_ok()
    );
    Ok(())
}

#[tokio::test]
async fn a_restricted_admin_cannot_delegate_tenant_wide_authority() -> Result<()> {
    use bitrouter_ai::tenancy::authority::{OwnerKind, ResourceKey, ResourceKind};
    let fixture = Fixture::new().await?;
    let credential = fixture.issue("issue-narrowing").await?;
    let caller = fixture
        .service
        .authenticate(credential.secret.as_deref().context("missing secret")?)
        .await
        .map_err(failure)?;
    let scope = GrantScope::Resources {
        resources: vec![ResourceKey {
            owner_kind: OwnerKind::Tenant,
            owner_id: fixture.tenant.as_str().try_into()?,
            resource_kind: ResourceKind::Route,
            resource_id: "route-a".try_into()?,
        }],
    };
    let owner = fixture
        .service
        .current()
        .map_err(failure)?
        .principal(&fixture.operator, Utc::now())
        .map_err(|code| anyhow::anyhow!("{code:?}"))?
        .clone();
    result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Binding(
                    fixture.tenant.clone(),
                    fixture.binding.clone(),
                    BindingPut {
                        operation_id: "narrow-admin".try_into()?,
                        expected_revision: fixture.binding_result.resource_revision,
                        principal_id: fixture.principal.clone(),
                        role: TenantRole::TenantAdmin,
                        scope: scope.clone(),
                        expires_at: credential.expires_at.clone(),
                        expected_management_owner: Some(owner),
                        installation_id: fixture.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    let mut command = BindingPut {
        operation_id: "escalate".try_into()?,
        expected_revision: None,
        principal_id: fixture.principal,
        role: TenantRole::TenantAdmin,
        scope: super::mutation::tenant_scope(),
        expires_at: credential.expires_at,
        expected_management_owner: None,
        installation_id: fixture.installation,
    };
    let denied = fixture
        .service
        .execute(
            caller.clone(),
            Command::Binding(
                fixture.tenant.clone(),
                "escalated".try_into()?,
                command.clone(),
            ),
        )
        .await;
    ensure!(matches!(denied, Err(error) if error.0.code == ErrorCode::Forbidden));
    command.operation_id = "bounded-delegation".try_into()?;
    command.scope = scope;
    result(
        fixture
            .service
            .execute(
                caller,
                Command::Binding(fixture.tenant, "bounded".try_into()?, command),
            )
            .await
            .map_err(failure)?,
    )?;
    Ok(())
}
