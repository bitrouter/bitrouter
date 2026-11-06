use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    authority::{AuditReaderCreate, CredentialIssued, PrincipalPut, TenantPut, TenantState},
    configuration::{
        Configuration, ConfigurationPut, GenerationDefaults, Learning, LearningMode, Limits,
    },
    identity::{InstallationId, OperationId, PrincipalId, TenantId},
    operation::{ErrorCode, Mutation, OperationStatus},
};
use chrono::Utc;
use sea_orm::{ConnectionTrait, DatabaseConnection, TransactionTrait};
use tokio::sync::oneshot;

use super::{Message, store};
use crate::tenancy::{
    Service, command::Command, error::Failure, model::Caller, ownership, store::StoredResponse,
};

fn failure(error: Failure) -> anyhow::Error {
    anyhow::anyhow!("tenancy failure: {:?}", error.0.code)
}

fn applied(response: StoredResponse) -> Result<()> {
    ensure!(
        matches!(response, StoredResponse::Operation(result) if result.status == OperationStatus::Applied),
        "expected an applied authority command"
    );
    Ok(())
}

struct Fixture {
    service: Service,
    db: DatabaseConnection,
    operator: Caller,
    installation: InstallationId,
    tenant: TenantId,
    credential: CredentialIssued,
}

impl Fixture {
    async fn open(db: DatabaseConnection, owner: Option<ownership::Lease>) -> Result<Self> {
        crate::db::run_migrations(&db).await?;
        let service = Service::open(db.clone(), owner).await?;
        let installation = service.current().map_err(failure)?.installation.clone();
        let operator = Caller::Installation(
            crate::tenancy::operator_principal("deadline-operator").map_err(failure)?,
        );
        let tenant: TenantId = "deadline-tenant".try_into()?;
        let principal: PrincipalId = "deadline-reader".try_into()?;
        applied(
            service
                .execute(
                    operator.clone(),
                    Command::Tenant(
                        tenant.clone(),
                        TenantPut {
                            operation_id: "create-deadline-tenant".try_into()?,
                            expected_revision: None,
                            state: TenantState::Active,
                            installation_id: installation.clone(),
                        },
                    ),
                )
                .await
                .map_err(failure)?,
        )?;
        applied(
            service
                .execute(
                    operator.clone(),
                    Command::Principal(
                        principal.clone(),
                        PrincipalPut {
                            operation_id: "create-deadline-reader".try_into()?,
                            expected_revision: None,
                            issuer: "deadline-test".into(),
                            subject: "reader".into(),
                            installation_id: installation.clone(),
                        },
                    ),
                )
                .await
                .map_err(failure)?,
        )?;
        let issued = service
            .execute(
                operator.clone(),
                Command::AuditReader(
                    tenant.clone(),
                    AuditReaderCreate {
                        operation_id: "issue-deadline-reader".try_into()?,
                        principal_id: principal,
                        expected_authority_revision: service.current().map_err(failure)?.revision,
                        expires_at: (Utc::now() + chrono::Duration::days(1))
                            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                            .try_into()?,
                        installation_id: installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?;
        let StoredResponse::Issued(credential) = issued else {
            anyhow::bail!("expected one-time audit credential");
        };
        Ok(Self {
            service,
            db,
            operator,
            installation,
            tenant,
            credential,
        })
    }

    fn revoke(&self, operation: &str) -> Result<Command> {
        Ok(Command::Revoke(
            self.tenant.clone(),
            self.credential.credential_id.clone(),
            Mutation {
                operation_id: operation.try_into()?,
                expected_revision: self.credential.credential_revision.clone(),
                installation_id: self.installation.clone(),
            },
        ))
    }

    fn enqueue(
        &self,
        command: Command,
    ) -> Result<oneshot::Receiver<Result<StoredResponse, Failure>>> {
        command.validate()?;
        let (reply, receive) = oneshot::channel();
        self.service
            .commands
            .try_send(
                Message {
                    caller: self.operator.clone(),
                    command,
                    reply,
                }
                .into(),
            )
            .map_err(|_| anyhow::anyhow!("test actor admission failed"))?;
        // Observe the actor reply directly so the HTTP/client wait deadline
        // cannot mask whether the actor cancelled preparation or kept committing.
        Ok(receive)
    }

    async fn operation(&self, id: &OperationId) -> Result<Option<store::Operation>> {
        let Caller::Installation(principal) = &self.operator else {
            anyhow::bail!("expected installation operator");
        };
        store::operation(&self.db, &self.installation, principal, id).await
    }

    async fn retained_tenant(&self) -> Result<crate::tenancy::model::Tenant> {
        store::directory::tenant_in(&self.db, &self.tenant, self.service.authority.load_budget())
            .await?
            .context("retained tenant missing")
    }
}

#[tokio::test]
async fn read_only_deadline_cancels_draft_and_preserves_revoke_and_suspend() -> Result<()> {
    let fixture = Fixture::open(crate::db::connect("sqlite::memory:").await?, None).await?;
    let before = fixture.service.current().map_err(failure)?;
    let operation: OperationId = "draft-blocked-on-read".try_into()?;
    let command = Command::Configuration(
        fixture.tenant.clone(),
        ConfigurationPut {
            operation_id: operation.clone(),
            expected_authority_revision: before.revision,
            installation_id: fixture.installation.clone(),
            document: Configuration {
                routes: Vec::new(),
                policy: None,
                generation_defaults: GenerationDefaults {
                    max_output_tokens: 1024,
                    temperature: 0.0,
                },
                learning: Learning {
                    mode: LearningMode::Off,
                    max_evaluations: 0,
                },
                limits: Limits {
                    active_executions: 1,
                    queued_work: 0,
                    learning_jobs: 0,
                },
            },
        },
    );
    // Occupy the only connection. The read deadline stops further preparation,
    // but the actor must retain its original BEGIN until it can roll it back.
    let held = fixture.db.begin().await?;
    let (begins, rollbacks) = fixture.service.cleanup.sql_counts_for_test();
    let mut response = fixture.enqueue(command)?;
    tokio::time::timeout(
        Duration::from_secs(10),
        fixture.service.cleanup.read_stopped.notified(),
    )
    .await
    .context("read-only deadline did not stop preparation")?;
    ensure!(matches!(
        response.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    ensure!(
        fixture.service.cleanup.disposition() == crate::tenancy::transaction::Disposition::Pending
    );
    let after = fixture.service.current().map_err(failure)?;
    ensure!(after.revision == before.revision && after.operation_ids == before.operation_ids);
    held.rollback().await?;
    let response = tokio::time::timeout(Duration::from_secs(3), response)
        .await
        .context("cancelled preparation did not finish rollback")??;
    ensure!(matches!(response, Err(error)
        if error.0.code == ErrorCode::AuthorityUnavailable
            && error.0.operation_id.as_ref() == Some(&operation)));
    ensure!(fixture.service.cleanup.sql_counts_for_test() == (begins + 1, rollbacks + 1));
    ensure!(
        fixture.service.cleanup.disposition()
            == crate::tenancy::transaction::Disposition::RolledBack
    );

    // Releasing storage must not resume the cancelled draft. Subsequent real
    // stop commands prove the same actor remains live and can durably mutate.
    applied(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                fixture.revoke("revoke-after-timeout")?,
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(
        fixture
            .service
            .authenticate(
                fixture
                    .credential
                    .secret
                    .as_deref()
                    .context("credential secret missing")?
            )
            .await
            .is_err()
    );
    let revision = fixture.retained_tenant().await?.revision;
    applied(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Tenant(
                    fixture.tenant.clone(),
                    TenantPut {
                        operation_id: "suspend-after-timeout".try_into()?,
                        expected_revision: Some(revision),
                        state: TenantState::Suspended,
                        installation_id: fixture.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(fixture.retained_tenant().await?.state == TenantState::Suspended);
    ensure!(fixture.operation(&operation).await?.is_none());
    let count = fixture
        .db
        .query_one(sea_orm::Statement::from_string(
            sea_orm::DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM tenancy_configurations",
        ))
        .await?
        .context("configuration count missing")?;
    ensure!(count.try_get::<i64>("", "count")? == 0);
    fixture.service.shutdown().await?;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn commit_deadline_fences_and_holds_owner_until_durable_settlement() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let url = format!(
        "sqlite://{}",
        directory.path().join("deadline.db").display()
    );
    let owner = ownership::acquire(&url)?;
    let db = crate::db::connect_tenancy(&url, &owner).await?;
    let fixture = Fixture::open(db, Some(owner)).await?;
    // Hold the actual write transaction immediately before consuming COMMIT.
    // A preparation/read failure cannot substitute for a late durable write.
    let gate = fixture
        .service
        .cleanup
        .hold(crate::tenancy::transaction::Phase::Committing);
    let operation: OperationId = "commit-outlives-deadline".try_into()?;
    let command = Command::Principal(
        "late-durable-principal".try_into()?,
        PrincipalPut {
            operation_id: operation.clone(),
            expected_revision: None,
            issuer: "deadline-test".into(),
            subject: "late-durable-principal".into(),
            installation_id: fixture.installation.clone(),
        },
    );
    let mut receive = fixture.enqueue(command)?;
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await?;
    tokio::time::pause();
    tokio::time::advance(crate::tenancy::MANAGEMENT_TIMEOUT).await;
    tokio::time::timeout(
        Duration::from_secs(1),
        fixture.service.cleanup.wait_fenced(),
    )
    .await
    .context("uncertain commit did not fence authority")?;
    tokio::time::resume();
    ensure!(
        matches!(receive.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
        "actor replied before the uncertain transaction settled"
    );
    ensure!(fixture.service.current().is_err());
    ensure!(matches!(fixture.service.execute(
        fixture.operator.clone(), fixture.revoke("blocked-by-commit-fence")?,
    ).await, Err(error) if error.0.code == ErrorCode::AuthorityUnavailable));

    let service = fixture.service.clone();
    let shutdown = tokio::spawn(async move { service.shutdown().await });
    fixture.service.stop.cancelled().await;
    ensure!(
        !shutdown.is_finished(),
        "shutdown abandoned the unsettled commit"
    );
    ensure!(
        ownership::acquire_path(&directory.path().join("deadline.db"), false).is_err(),
        "authority owner was released before commit settlement"
    );
    fixture.service.cleanup.release_gate();
    let response = tokio::time::timeout(Duration::from_secs(10), receive).await??;
    ensure!(matches!(response, Err(error)
        if error.0.code == ErrorCode::AuthorityUnavailable
            && error.0.operation_id.as_ref() == Some(&operation)));
    tokio::time::timeout(Duration::from_secs(10), shutdown).await???;
    ensure!(
        fixture.service.cleanup.disposition()
            == crate::tenancy::transaction::Disposition::Committed
    );
    let stored = fixture
        .operation(&operation)
        .await?
        .context("late commit was lost")?;
    ensure!(stored.result.status == OperationStatus::Applied);
    ensure!(
        fixture.service.current().is_err(),
        "late success cleared the fence"
    );
    Ok(())
}
mod hardening;
#[cfg(unix)]
mod quarantine;
mod scoped_commands;
