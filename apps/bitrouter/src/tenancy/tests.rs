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
mod configuration;
mod conformance;
#[cfg(feature = "core")]
mod eval;
pub(in crate::tenancy) mod execution;
mod http;
mod learning_startup;
#[cfg(not(feature = "core"))]
mod no_core;

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
        Self::with_limits(super::actor::OperationLimits::default()).await
    }

    async fn with_limits(limits: super::actor::OperationLimits) -> Result<Self> {
        let db = crate::db::connect("sqlite::memory:").await?;
        Self::with_database(db, limits, &mut |_| {}).await
    }

    // Test-only injection before Service construction, never a post-admission DB swap.
    // The callback lets isolated backend tests retain every actor outside cancellation.
    async fn with_database(
        db: sea_orm::DatabaseConnection,
        limits: super::actor::OperationLimits,
        started: &mut (impl FnMut(&Service) + ?Sized),
    ) -> Result<Self> {
        crate::db::run_migrations(&db).await?;
        let service = Service::open_with_limits(db.clone(), None, limits).await?;
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

    fn issuance(&self, operation: &str) -> Result<CredentialCreate> {
        let state = self.service.current().map_err(failure)?;
        Ok(CredentialCreate {
            operation_id: operation.try_into()?,
            expected_authority_revision: state.revision,
            principal_id: self.principal.clone(),
            permissions: TenantRole::TenantAdmin.permissions().to_vec(),
            scope: super::mutation::tenant_scope(),
            expires_at: state
                .bindings
                .get(&(self.tenant.clone(), self.binding.clone()))
                .context("binding missing")?
                .expires
                .wire
                .clone(),
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
                    Command::Issue(self.tenant.clone(), self.issuance(operation)?),
                )
                .await
                .map_err(failure)?,
        )
    }
}

#[tokio::test]
async fn concurrent_issuance_returns_one_secret_and_replay_survives_restart() -> Result<()> {
    let fixture = Fixture::new().await?;
    let input = fixture.issuance("issue-once")?;
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
    let caller = fixture.service.authenticate(secret).map_err(failure)?;
    fixture
        .service
        .current()
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
    ensure!(fixture.service.authenticate(secret).is_err());
    let current = fixture.service.current().map_err(failure)?;
    ensure!(Some(current.revision) == revoked.authority_revision);
    ensure!(
        current
            .authorize(
                &caller,
                &fixture.tenant,
                Permission::ExecutionSubmit,
                &super::mutation::tenant_scope(),
                Utc::now()
            )
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
    let state = fixture.service.current().map_err(failure)?;
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
    ensure!(fixture.service.authenticate(secret).is_ok());
    fixture.db.close().await?;
    let read = fixture
        .service
        .operation(&fixture.operator, &"create-tenant".try_into()?)
        .await;
    ensure!(matches!(read, Err(error) if error.0.code == ErrorCode::StorageUnavailable));
    ensure!(
        matches!(fixture.service.authenticate(secret), Err(error) if error.0.code == ErrorCode::AuthorityUnavailable)
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
    let state = fixture.service.current().map_err(failure)?;
    ensure!(!state.tenants.contains_key(&"tenant-b".try_into()?));
    ensure!(!state.principals.contains_key(&"principal-b".try_into()?));
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
        let input = fixture.issuance(&format!("http-issue-{index}"))?;
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
async fn saturation_reserves_only_stop_commands_and_survives_restart() -> Result<()> {
    // A small private test bound exercises the production state machine and
    // real journal without allocating 100,000 fixture rows. Public limits stay
    // fixed at the frozen profile and cannot be changed through configuration.
    let fixture = Fixture::with_limits(super::actor::OperationLimits {
        ordinary: 4,
        safety: 2,
    })
    .await?;
    let before = fixture
        .service
        .capabilities(&fixture.operator)
        .map_err(failure)?;
    let credential = fixture.issue("fourth-ordinary-command").await?;
    let after = fixture
        .service
        .capabilities(&fixture.operator)
        .map_err(failure)?;
    ensure!(before.capability_revision != after.capability_revision);
    let creation = Command::Tenant(
        "new-tenant".try_into()?,
        TenantPut {
            operation_id: "no-reserved-creation".try_into()?,
            expected_revision: None,
            state: TenantState::Suspended,
            installation_id: fixture.installation.clone(),
        },
    );
    ensure!(
        matches!(fixture.service.execute(fixture.operator.clone(), creation).await, Err(error) if error.0.code == ErrorCode::CapacityExceeded)
    );
    result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Revoke(
                    fixture.tenant.clone(),
                    credential.credential_id,
                    Mutation {
                        operation_id: "reserved-revoke".try_into()?,
                        expected_revision: credential.credential_revision,
                        installation_id: fixture.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    let tenant_revision = fixture
        .service
        .current()
        .map_err(failure)?
        .tenants
        .get(&fixture.tenant)
        .context("tenant missing")?
        .revision
        .clone();
    let stopped = result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Tenant(
                    fixture.tenant.clone(),
                    TenantPut {
                        operation_id: "reserved-stop".try_into()?,
                        expected_revision: Some(tenant_revision),
                        state: TenantState::Suspended,
                        installation_id: fixture.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(
        matches!(fixture.service.execute(fixture.operator.clone(), Command::Tenant(fixture.tenant.clone(), TenantPut {
        operation_id: "reserve-exhausted".try_into()?, expected_revision: stopped.resource_revision, state: TenantState::Deleting, installation_id: fixture.installation.clone(),
    })).await, Err(error) if error.0.code == ErrorCode::CapacityExceeded)
    );
    let alive = fixture.service.live.clone();
    drop(fixture.service);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while alive.load(std::sync::atomic::Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let reopened = Service::open(fixture.db, None).await?;
    ensure!(reopened.current().map_err(failure)?.saturated);
    ensure!(
        matches!(reopened.execute(fixture.operator, Command::Principal("extra-principal".try_into()?, PrincipalPut {
        operation_id: "still-saturated".try_into()?, expected_revision: None, issuer: "new".into(), subject: "new".into(), installation_id: fixture.installation,
    })).await, Err(error) if error.0.code == ErrorCode::CapacityExceeded)
    );
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
    let mut request_b = fixture.issuance("issue-b")?;
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
