use super::*;
use crate::tenancy::transaction::{
    CLEANUP_TIMEOUT, Control, Disposition, Phase, Proof, ReadConnection,
};
use bitrouter_ai::tenancy::{
    authority::{BindingPut, CredentialCreate, CredentialState, TenantRole},
    identity::BindingId,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

#[cfg(all(unix, any(feature = "otel-http", feature = "otel-grpc")))]
async fn measured_issue(
    fixture: &Fixture,
    path: &std::path::Path,
    input: CredentialCreate,
    label: &str,
) -> Result<CredentialIssued> {
    use crate::storage::write::measurement;
    let budget = fixture.service.storage_budget();
    ensure!(measurement::reservation(&budget)? == 0);
    let before = measurement::wal_baseline(&fixture.db, path).await?;
    let gate = fixture.service.cleanup.hold(Phase::Committing);
    let receive = fixture.enqueue(Command::Issue(fixture.tenant.clone(), input))?;
    let pending = async {
        tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await?;
        Ok::<_, anyhow::Error>((
            measurement::files(path)?,
            measurement::reservation(&budget)?,
        ))
    }
    .await;
    fixture.service.cleanup.release_gate();
    let response = issued(
        tokio::time::timeout(Duration::from_secs(3), receive)
            .await??
            .map_err(failure)?,
    )?;
    let (files, charged) = pending?;
    ensure!(measurement::reservation(&budget)? == 0);
    measurement::report(&fixture.db, path, label, &before, files, charged).await?;
    Ok(response)
}

#[cfg(all(unix, any(feature = "otel-http", feature = "otel-grpc")))]
#[tokio::test]
async fn actual_issue_and_rotation_estimates_cover_sampled_file_growth() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("management-calibration.sqlite");
    let url = format!("sqlite://{}", path.display());
    let owner = ownership::acquire(&url)?;
    let db = crate::db::connect_tenancy(&url, &owner).await?;
    let fixture = Fixture::open(db, Some(owner)).await?;
    let result: Result<()> = async {
        let mut input = issuance(&fixture, "calibration-issue").await?;
        let first = measured_issue(&fixture, &path, input.clone(), "management/issue").await?;
        input.operation_id = "calibration-rotation".try_into()?;
        input.expected_authority_revision = fixture.service.current().map_err(failure)?.revision;
        input.rotate_credential_id = Some(first.credential_id.clone());
        let next = measured_issue(&fixture, &path, input.clone(), "management/rotation").await?;
        ensure!(next.secret.is_some());
        let old = store::credential(&fixture.db, first.credential_id.as_str())
            .await?
            .context("old credential")?;
        ensure!(old.state == CredentialState::Revoked);
        let replay = issued(
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Issue(fixture.tenant.clone(), input),
                )
                .await
                .map_err(failure)?,
        )?;
        ensure!(replay.secret.is_none() && replay.credential_id == next.credential_id);
        ensure!(
            crate::storage::write::measurement::reservation(&fixture.service.storage_budget())?
                == 0
        );
        Ok(())
    }
    .await;
    let stopped = fixture.service.shutdown().await;
    let closed = fixture.db.clone().close().await;
    drop(fixture);
    let removed = directory.close();
    result?;
    stopped?;
    closed?;
    removed?;
    Ok(())
}

fn issued(response: StoredResponse) -> Result<CredentialIssued> {
    match response {
        StoredResponse::Issued(value) => Ok(value),
        _ => anyhow::bail!("expected credential issuance"),
    }
}

async fn issuance(fixture: &Fixture, operation: &str) -> Result<CredentialCreate> {
    let binding: BindingId = "scoped-command-binding".try_into()?;
    applied(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Binding(
                    fixture.tenant.clone(),
                    binding.clone(),
                    BindingPut {
                        operation_id: "create-scoped-command-binding".try_into()?,
                        expected_revision: None,
                        principal_id: fixture.credential.principal_id.clone(),
                        role: TenantRole::TenantAdmin,
                        scope: crate::tenancy::mutation::tenant_scope(),
                        expires_at: fixture.credential.expires_at.clone(),
                        expected_management_owner: None,
                        installation_id: fixture.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    Ok(CredentialCreate {
        operation_id: operation.try_into()?,
        expected_authority_revision: fixture.service.current().map_err(failure)?.revision,
        principal_id: fixture.credential.principal_id.clone(),
        permissions: TenantRole::TenantAdmin.permissions().to_vec(),
        scope: crate::tenancy::mutation::tenant_scope(),
        expires_at: fixture.credential.expires_at.clone(),
        rotate_credential_id: None,
        binding_id: binding,
        installation_id: fixture.installation.clone(),
    })
}

fn configuration(fixture: &Fixture, operation: &str) -> Result<ConfigurationPut> {
    Ok(ConfigurationPut {
        operation_id: operation.try_into()?,
        expected_authority_revision: fixture.service.current().map_err(failure)?.revision,
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
    })
}

#[tokio::test]
async fn replay_after_cleanup_fence_cannot_return_success_or_begin_a_write() -> Result<()> {
    let fixture = Fixture::open(crate::db::connect("sqlite::memory:").await?, None).await?;
    applied(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                hardening::principal(&fixture, "replay-cleanup-fence")?,
            )
            .await
            .map_err(failure)?,
    )?;
    let (begins, rollbacks) = fixture.service.cleanup.sql_counts_for_test();
    let gate = fixture.service.cleanup.hold(Phase::RollingBack);
    let mut response = fixture.enqueue(hardening::principal(&fixture, "replay-cleanup-fence")?)?;
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await?;
    ensure!(matches!(
        response.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    tokio::time::pause();
    tokio::time::advance(CLEANUP_TIMEOUT + Duration::from_millis(1)).await;
    tokio::time::timeout(
        Duration::from_secs(1),
        fixture.service.cleanup.wait_fenced(),
    )
    .await?;
    tokio::time::resume();
    fixture.service.cleanup.release_gate();
    let result = tokio::time::timeout(Duration::from_secs(3), response).await??;
    ensure!(matches!(result, Err(error) if error.0.code == ErrorCode::AuthorityUnavailable));
    ensure!(fixture.service.current().is_err());
    ensure!(fixture.service.cleanup.disposition() == Disposition::RolledBack);
    ensure!(
        fixture.service.cleanup.sql_counts_for_test() == (begins + 1, rollbacks + 1),
        "replay must own exactly one read transaction, with no write BEGIN"
    );
    let saved = fixture
        .operation(&"replay-cleanup-fence".try_into()?)
        .await?
        .context("original operation missing")?;
    ensure!(saved.result.status == OperationStatus::Applied);
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn replay_rechecks_credential_expiry_after_preparation_rollback() -> Result<()> {
    let fixture = Fixture::open(crate::db::connect("sqlite::memory:").await?, None).await?;
    let mut input = issuance(&fixture, "expiring-replay-caller").await?;
    let expiry = Utc::now() + chrono::Duration::seconds(3);
    input.expires_at = expiry
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        .try_into()?;
    let credential = issued(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Issue(fixture.tenant.clone(), input),
            )
            .await
            .map_err(failure)?,
    )?;
    let caller = fixture
        .service
        .authenticate(credential.secret.as_deref().context("secret missing")?)
        .await
        .map_err(failure)?;
    let draft = configuration(&fixture, "expiring-caller-draft")?;
    ensure!(matches!(
        fixture
            .service
            .execute(
                caller.clone(),
                Command::Configuration(fixture.tenant.clone(), draft.clone()),
            )
            .await
            .map_err(failure)?,
        StoredResponse::Configuration(_)
    ));
    let before = fixture.service.current().map_err(failure)?;
    let (begins, rollbacks) = fixture.service.cleanup.sql_counts_for_test();
    let gate = fixture.service.cleanup.hold(Phase::RollingBack);
    let (reply, receive) = oneshot::channel();
    fixture
        .service
        .commands
        .try_send(
            Message {
                caller,
                command: Command::Configuration(fixture.tenant.clone(), draft),
                reply,
            }
            .into(),
        )
        .map_err(|_| anyhow::anyhow!("test actor admission failed"))?;
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await?;
    ensure!(Utc::now() < expiry, "caller expired before the replay gate");
    // Utc expiry and Tokio cleanup use different clocks. Freeze the watchdog
    // while this current-thread test crosses the real credential expiry, so a
    // cleanup fence cannot mask the missing post-rollback authorization check.
    tokio::time::pause();
    std::thread::sleep((expiry - Utc::now()).to_std()? + Duration::from_millis(10));
    tokio::time::resume();
    fixture.service.cleanup.release_gate();
    let result = tokio::time::timeout(Duration::from_secs(3), receive).await??;
    ensure!(matches!(result, Err(error) if error.0.code == ErrorCode::Forbidden));
    ensure!(fixture.service.cleanup.disposition() == Disposition::RolledBack);
    ensure!(fixture.service.cleanup.sql_counts_for_test() == (begins + 1, rollbacks + 1));
    let after = fixture.service.current().map_err(failure)?;
    ensure!(after.revision == before.revision && after.operation_ids == before.operation_ids);
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn stopped_preparation_rolls_back_without_later_queries_and_preserves_safety() -> Result<()> {
    let mut db = crate::db::connect("sqlite::memory:").await?;
    let queries = Arc::new(AtomicUsize::new(0));
    let observed = queries.clone();
    db.set_metric_callback(move |info| {
        if info.statement.sql.trim_start().starts_with("SELECT") {
            observed.fetch_add(1, Ordering::SeqCst);
        }
    });
    let fixture = Fixture::open(db, None).await?;
    let before = fixture.service.current().map_err(failure)?;
    let (begins, rollbacks) = fixture.service.cleanup.sql_counts_for_test();
    let read_count = queries.load(Ordering::SeqCst);
    ensure!(
        read_count > 0,
        "SQL observation must include real fixture reads"
    );
    let gate = fixture.service.cleanup.hold(Phase::Active);
    let response = fixture.enqueue(hardening::principal(&fixture, "cancel-scoped-preparation")?)?;
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified()).await?;
    tokio::time::pause();
    tokio::time::advance(crate::tenancy::MANAGEMENT_TIMEOUT).await;
    tokio::time::timeout(
        Duration::from_secs(1),
        fixture.service.cleanup.read_stopped.notified(),
    )
    .await?;
    tokio::time::resume();
    fixture.service.cleanup.release_gate();
    let result = tokio::time::timeout(Duration::from_secs(3), response).await??;
    ensure!(matches!(result, Err(error) if error.0.code == ErrorCode::AuthorityUnavailable));
    ensure!(
        queries.load(Ordering::SeqCst) == read_count,
        "a stopped preparation must not issue its first or any subsequent SELECT"
    );
    ensure!(fixture.service.cleanup.sql_counts_for_test() == (begins + 1, rollbacks + 1));
    ensure!(fixture.service.cleanup.disposition() == Disposition::RolledBack);
    let after = fixture.service.current().map_err(failure)?;
    ensure!(after.revision == before.revision && after.operation_ids == before.operation_ids);
    ensure!(
        fixture
            .operation(&"cancel-scoped-preparation".try_into()?)
            .await?
            .is_none()
    );
    applied(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                fixture.revoke("safety-after-scoped-cancellation")?,
            )
            .await
            .map_err(failure)?,
    )?;
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn read_connection_discards_a_completed_read_after_stop_and_skips_next_query() -> Result<()> {
    for query_all in [false, true] {
        let mut db = crate::db::connect("sqlite::memory:").await?;
        let live = Arc::new(AtomicBool::new(true));
        let proof = Proof::new(live.clone());
        let control = Arc::new(Control::new(proof.clone()));
        let queries = Arc::new(AtomicUsize::new(0));
        let observed = queries.clone();
        let stopped = control.clone();
        // SeaORM invokes this callback after the real SQLite SELECT settles, but
        // before ReadConnection receives the result. No extra production hook.
        db.set_metric_callback(move |info| {
            if info.statement.sql.trim_start().starts_with("SELECT") {
                observed.fetch_add(1, Ordering::SeqCst);
                stopped.stop_read();
            }
        });
        control.prepare_read();
        let tx = control.begin(&db).await?;
        {
            let read = ReadConnection::new(&tx, &control);
            let statement =
                || sea_orm::Statement::from_string(sea_orm::DatabaseBackend::Sqlite, "SELECT 1");
            ensure!(read.execute(statement()).await.is_err());
            ensure!(read.execute_unprepared("SELECT 1").await.is_err());
            ensure!(queries.load(Ordering::SeqCst) == 0);
            let completed = if query_all {
                read.query_all(statement()).await.map(|_| ())
            } else {
                read.query_one(statement()).await.map(|_| ())
            };
            ensure!(
                completed.is_err(),
                "a result completed after stop was exposed"
            );
            ensure!(queries.load(Ordering::SeqCst) == 1);
            ensure!(read.query_all(statement()).await.is_err());
            ensure!(read.query_one(statement()).await.is_err());
            ensure!(
                queries.load(Ordering::SeqCst) == 1,
                "later SQL escaped cancellation"
            );
        }
        control.rollback(tx).await?;
        ensure!(control.finish_preparation().is_err());
        ensure!(proof.sql_counts_for_test() == (1, 1));
        ensure!(proof.disposition() == Disposition::RolledBack);
        ensure!(live.load(Ordering::Acquire));
    }
    Ok(())
}

#[tokio::test]
async fn revoked_rotation_predecessor_rejects_new_operation_but_exact_replay_stays_read_only()
-> Result<()> {
    let fixture = Fixture::open(crate::db::connect("sqlite::memory:").await?, None).await?;
    let mut input = issuance(&fixture, "rotation-original").await?;
    let original = issued(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Issue(fixture.tenant.clone(), input.clone()),
            )
            .await
            .map_err(failure)?,
    )?;
    input.operation_id = "rotation-replacement".try_into()?;
    input.expected_authority_revision = fixture.service.current().map_err(failure)?.revision;
    input.rotate_credential_id = Some(original.credential_id.clone());
    let replacement = issued(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Issue(fixture.tenant.clone(), input.clone()),
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(replacement.secret.is_some());
    let revoked = store::credential(&fixture.db, original.credential_id.as_str())
        .await?
        .context("revoked predecessor missing")?;
    ensure!(revoked.state == CredentialState::Revoked);
    let before = fixture.service.current().map_err(failure)?;
    let (begins, rollbacks) = fixture.service.cleanup.sql_counts_for_test();
    let mut repeated = input.clone();
    repeated.operation_id = "rotation-from-revoked".try_into()?;
    repeated.expected_authority_revision = before.revision;
    let rejected = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Issue(fixture.tenant.clone(), repeated.clone()),
        )
        .await;
    ensure!(matches!(rejected, Err(error) if error.0.code == ErrorCode::NotFound));
    ensure!(fixture.service.cleanup.sql_counts_for_test() == (begins + 1, rollbacks + 1));
    let after = fixture.service.current().map_err(failure)?;
    ensure!(after.revision == before.revision && after.operation_ids == before.operation_ids);
    ensure!(fixture.operation(&repeated.operation_id).await?.is_none());
    let unchanged = store::credential(&fixture.db, original.credential_id.as_str())
        .await?
        .context("revoked predecessor disappeared")?;
    ensure!(unchanged.state == CredentialState::Revoked && unchanged.revision == revoked.revision);

    let (begins, rollbacks) = fixture.service.cleanup.sql_counts_for_test();
    let replay = issued(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Issue(fixture.tenant.clone(), input),
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(replay.secret.is_none());
    ensure!(replay.credential_id == replacement.credential_id);
    ensure!(replay.credential_revision == replacement.credential_revision);
    ensure!(replay.authority_revision == replacement.authority_revision);
    ensure!(fixture.service.cleanup.sql_counts_for_test() == (begins + 1, rollbacks + 1));
    let after_replay = fixture.service.current().map_err(failure)?;
    ensure!(
        after_replay.revision == before.revision
            && after_replay.operation_ids == before.operation_ids
    );
    ensure!(
        fixture
            .service
            .authenticate(
                original
                    .secret
                    .as_deref()
                    .context("original secret missing")?
            )
            .await
            .is_err()
    );
    fixture
        .service
        .authenticate(
            replacement
                .secret
                .as_deref()
                .context("replacement secret missing")?,
        )
        .await
        .map_err(failure)?;
    fixture.service.shutdown().await?;
    Ok(())
}
