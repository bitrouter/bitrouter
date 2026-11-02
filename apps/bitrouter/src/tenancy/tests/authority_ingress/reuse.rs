use super::*;
use crate::tenancy::authority::live::DependencyIntent;
use std::sync::atomic::AtomicUsize;

/// Observe actual SQL through the existing driver's callback; count admitted
/// authority jobs and acknowledged transactions independently. No provider I/O.
#[tokio::test]
async fn warm_exact_route_uses_zero_selects_zero_transactions_and_one_authority_job() -> Result<()>
{
    let queries = Arc::new(AtomicUsize::new(0));
    let ready = Ready::with_queries(false, Some(queries.clone())).await?;
    let service = &ready.fixture.service;
    let first = ready.execution().await?;
    let first_owner = first.receipt_owner()?.clone();
    tokio::time::timeout(TIMEOUT, first_owner.finish(Outcome::Failed))
        .await?
        .map_err(code)?;
    first_owner.release_active().map_err(code)?;
    let before_queries = queries.load(Ordering::Relaxed);
    ensure!(
        before_queries > 0,
        "the callback must observe real cold SQL"
    );
    let before_tx = service.cleanup.sql_counts_for_test();
    let before_jobs = service.authority.read_jobs.load(Ordering::Relaxed);
    let warm = ready.execution().await?;
    ensure!(
        queries.load(Ordering::Relaxed) == before_queries,
        "warm authority performed SELECT"
    );
    ensure!(
        service.cleanup.sql_counts_for_test() == before_tx,
        "warm authority opened SQL custody"
    );
    ensure!(service.authority.read_jobs.load(Ordering::Relaxed) == before_jobs + 1);
    // Final authorization remains live even though the facts came from retention.
    first.revalidate().map_err(code)?;
    warm.revalidate().map_err(code)?;
    eprintln!("warm_authority SELECT=0 BEGIN=0 ROLLBACK=0 JOB=1 (driver/Proof/actor counters)");
    let warm_owner = warm.receipt_owner()?.clone();
    tokio::time::timeout(TIMEOUT, warm_owner.finish(Outcome::Failed))
        .await?
        .map_err(code)?;
    warm_owner.release_active().map_err(code)?;

    // A different fresh intent must really load B and replace inherited A.
    let secret = ready
        .credential
        .secret
        .as_deref()
        .context("issuance secret")?;
    let identity = service
        .authenticate_with_reuse(secret, true)
        .await
        .map_err(failure)?;
    let selected = ResourceRef {
        owner_kind: OwnerKind::Tenant,
        owner_id: ready.fixture.tenant.as_str().try_into()?,
        resource_kind: ResourceKind::Route,
        resource_id: "other-fresh-route".try_into()?,
        revision_id: "r1".try_into()?,
    };
    let before_queries = queries.load(Ordering::Relaxed);
    let before_tx = service.cleanup.sql_counts_for_test();
    let before_jobs = service.authority.read_jobs.load(Ordering::Relaxed);
    service
        .authority_read(AuthoritySubject::Extend {
            caller: identity.caller.clone(),
            lease: identity.lease.clone(),
            dependencies: vec![selected.clone()],
            intent: DependencyIntent::Fresh,
        })
        .await
        .map_err(failure)?;
    ensure!(queries.load(Ordering::Relaxed) > before_queries);
    ensure!(service.cleanup.sql_counts_for_test() == (before_tx.0 + 1, before_tx.1 + 1));
    ensure!(service.authority.read_jobs.load(Ordering::Relaxed) == before_jobs + 1);
    service
        .authority
        .read()
        .map_err(code)?
        .check(&identity.lease, |view| {
            assert!(view.covers(std::slice::from_ref(&selected), DependencyIntent::Fresh)?);
            Ok(())
        })
        .map_err(code)?;
    drop((identity, first, warm, first_owner, warm_owner));
    tokio::time::timeout(TIMEOUT, service.shutdown()).await??;
    Ok(())
}

#[tokio::test]
async fn retained_token_does_not_bypass_read_admission_revoke_or_stop() -> Result<()> {
    let ready = Ready::new(false).await?;
    let service = &ready.fixture.service;
    let execution = ready.execution().await?;
    let owner = execution.receipt_owner()?.clone();
    tokio::time::timeout(TIMEOUT, owner.finish(Outcome::Failed))
        .await?
        .map_err(code)?;
    owner.release_active().map_err(code)?;
    let secret = ready
        .credential
        .secret
        .as_deref()
        .context("issuance secret")?;
    let reads = service.reads.available_permits();
    let held = service
        .reads
        .clone()
        .try_acquire_many_owned(u32::try_from(reads)?)?;
    let before = service.authority.read_jobs.load(Ordering::Relaxed);
    ensure!(
        matches!(service.authenticate_with_reuse(secret, true).await, Err(error) if error.0.code == ErrorCode::CapacityExceeded)
    );
    ensure!(service.authority.read_jobs.load(Ordering::Relaxed) == before);
    drop(held);
    let current = service
        .authenticate_with_reuse(secret, true)
        .await
        .map_err(failure)?;
    result(
        service
            .execute(
                ready.fixture.operator.clone(),
                Command::Revoke(
                    ready.fixture.tenant.clone(),
                    ready.credential.credential_id.clone(),
                    Mutation {
                        operation_id: "revoke-reused-caller".try_into()?,
                        expected_revision: ready.credential.credential_revision.clone(),
                        installation_id: ready.fixture.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    ensure!(
        matches!(service.authenticate_with_reuse(secret, true).await, Err(error) if error.0.code == ErrorCode::Unauthorized)
    );
    ensure!(execution.revalidate() == Err(ErrorCode::Unauthorized));
    ensure!(
        service
            .authority
            .read()
            .map_err(code)?
            .check(&current.lease, |view| view
                .facts()
                .principal(&current.caller, chrono::Utc::now())
                .map(|_| ()))
            == Err(ErrorCode::Unauthorized)
    );
    drop((current, execution, owner));
    tokio::time::timeout(TIMEOUT, service.shutdown()).await??;
    ensure!(
        matches!(service.authenticate_with_reuse(secret, true).await, Err(error) if error.0.code == ErrorCode::AuthorityUnavailable)
    );
    Ok(())
}
