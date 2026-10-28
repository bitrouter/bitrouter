use super::*;
use crate::tenancy::tests::issued;
use bitrouter_ai::tenancy::{
    configuration::Activation,
    identity::{RevisionId, TenantId},
};

async fn activate(ready: &Ready, revision: &RevisionId, operation: &str) -> Result<()> {
    let active = ready
        .fixture
        .service
        .active_revision(&ready.fixture.operator, &ready.fixture.tenant)
        .map_err(failure)?;
    ready
        .fixture
        .service
        .execute(
            ready.fixture.operator.clone(),
            Command::Activate(
                ready.fixture.tenant.clone(),
                Activation {
                    operation_id: operation.try_into()?,
                    expected_active_revision: active.revision_id,
                    revision_id: revision.clone(),
                    expected_authority_revision: active.authority_revision,
                    installation_id: ready.fixture.installation.clone(),
                },
            ),
        )
        .await
        .map_err(failure)?;
    Ok(())
}
#[tokio::test]
async fn reactivating_r1_and_rotating_the_caller_do_not_refund_its_accepted_job() -> Result<()> {
    let provider = Provider::start(false).await?;
    let judge = Judge::new(false).await?;
    let mut ready = Ready::new(&provider.url, &judge.url, true, LearningMode::Observe, 1).await?;
    let first = ready.port().await?;
    let mut ports = vec![first.clone()];
    let checked = async {
        activate(&ready, &ready.revision, "paid-activate-r1").await?;
        ensure!(first.checkpoint(native()).await?.is_some());
        drained(&ready).await?;
        let r2 = ready
            .configuration("paid-r2", LearningMode::Observe, 1)
            .await?;
        activate(&ready, &r2, "paid-activate-r2").await?;
        activate(&ready, &ready.revision, "paid-reactivate-r1").await?;
        let before = bodies(&ready.fixture, "owned_judge_budget").await?;
        ensure!(before.len() == 1 && before[0]["accepted"] == 1);
        let mut input = ready.fixture.issuance("paid-rotate")?;
        input.rotate_credential_id = Some(ready.credential.credential_id.clone());
        ready.credential = issued(
            ready
                .fixture
                .service
                .execute(
                    ready.fixture.operator.clone(),
                    Command::Issue(ready.fixture.tenant.clone(), input),
                )
                .await
                .map_err(failure)?,
        )?;
        ensure!(
            first.checkpoint(native()).await.is_err(),
            "revoked original caller reused its binding"
        );
        let second = ready.port().await?;
        ports.push(second.clone());
        ensure!(
            second.checkpoint(native()).await.is_err(),
            "new credential funded an exhausted R1"
        );
        drained(&ready).await?;
        ensure!(bodies(&ready.fixture, "owned_judge_budget").await? == before);
        ensure!(bodies(&ready.fixture, "owned_judge_job").await?.len() == 1);
        ensure!(judge.calls.load(Ordering::SeqCst) == 1 && provider.calls() == 0);
        // R2 owns a different immutable budget. Paying its first job must leave
        // the exhausted R1 counter intact, even with the same rotated caller.
        activate(&ready, &r2, "paid-activate-r2-after-rotation").await?;
        ready.revision = r2;
        let third = ready.port().await?;
        ports.push(third.clone());
        ensure!(third.checkpoint(native()).await?.is_some());
        drained(&ready).await?;
        let counters = bodies(&ready.fixture, "owned_judge_budget").await?;
        ensure!(counters.len() == 2 && counters.contains(&before[0]));
        ensure!(counters.iter().all(|counter| counter["accepted"] == 1));
        ensure!(bodies(&ready.fixture, "owned_judge_job").await?.len() == 2);
        ensure!(judge.calls.load(Ordering::SeqCst) == 2 && provider.calls() == 0);
        Ok(())
    }
    .await;
    cleanup(&ready, &ports, judge, provider, checked).await
}

pub(super) async fn tenant_b(a: &mut Ready, subject: &str, judge: &str) -> Result<Ready> {
    let tenant: TenantId = "tenant-b".try_into()?;
    let binding = a.fixture.binding.clone();
    result(
        a.fixture
            .service
            .execute(
                a.fixture.operator.clone(),
                Command::Tenant(
                    tenant.clone(),
                    TenantPut {
                        operation_id: "paid-create-tenant-b".try_into()?,
                        expected_revision: None,
                        state: TenantState::Active,
                        installation_id: a.fixture.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    let binding_result = result(
        a.fixture
            .service
            .execute(
                a.fixture.operator.clone(),
                Command::Binding(
                    tenant.clone(),
                    binding.clone(),
                    BindingPut {
                        operation_id: "paid-bind-tenant-b".try_into()?,
                        expected_revision: None,
                        principal_id: a.fixture.principal.clone(),
                        role: TenantRole::TenantAdmin,
                        scope: crate::tenancy::mutation::tenant_scope(),
                        expires_at: expiry()?,
                        expected_management_owner: None,
                        installation_id: a.fixture.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    let fixture = Fixture {
        service: a.fixture.service.clone(),
        db: a.fixture.db.clone(),
        operator: a.fixture.operator.clone(),
        installation: a.fixture.installation.clone(),
        tenant,
        principal: a.fixture.principal.clone(),
        binding,
        binding_result,
    };
    // No live producers exist while the fixture adds the second immutable catalog.
    let b = Ready::from_fixture(
        fixture,
        None,
        subject,
        judge,
        true,
        LearningMode::Observe,
        1,
    )
    .await?;
    a.fixture.service = b.fixture.service.clone();
    Ok(b)
}
#[tokio::test]
async fn same_database_literal_names_and_legacy_capture_do_not_share_jobs_or_parent_access()
-> Result<()> {
    let provider = Provider::start(false).await?;
    let judge = Judge::new(false).await?;
    let mut a = Ready::new(&provider.url, &judge.url, true, LearningMode::Observe, 1).await?;
    let b = tenant_b(&mut a, &provider.url, &judge.url).await?;
    let mut ports = Vec::new();
    let checked = async {
        ensure!(
            a.subject.resource_id == b.subject.resource_id
                && a.subject.revision_id == b.subject.revision_id
                && a.subject.owner_id != b.subject.owner_id
        );
        let (ab, ap) = a.port_pair().await?;
        ports.push(ap.clone());
        let (bb, bp) = b.empty_port().await?;
        ports.push(bp.clone());
        // The same raw native name exists only under A at this point. B's real
        // public Port refuses it before paid preparation; no ambient parent lookup.
        ensure!(bp.checkpoint(native()).await.is_err());
        ensure!(b.fixture.service.current().is_ok() && judge.calls.load(Ordering::SeqCst) == 0);
        for event in super::super::fixture::sequence(b.subject.resource_id.as_str()) {
            bp.record(event).await?;
        }
        let (ar, br) = tokio::join!(ap.checkpoint(native()), bp.checkpoint(native()));
        ensure!(ar?.is_some() && br?.is_some());
        drained(&a).await?;
        let akey = ab.association().session_key(native())?;
        let bkey = bb.association().session_key(native())?;
        ensure!(akey != bkey);
        let jobs = bodies(&a.fixture, "owned_judge_job").await?;
        ensure!(jobs.len() == 2 && jobs.iter().all(|job| job["status"] == "completed"));
        ensure!(
            jobs.iter().any(|job| job["session"] == akey)
                && jobs.iter().any(|job| job["session"] == bkey)
        );
        let counters = bodies(&a.fixture, "owned_judge_budget").await?;
        ensure!(counters.len() == 2 && counters.iter().all(|counter| counter["accepted"] == 1));
        let legacy = crate::harness::trajectory::Recorder::new(
            a.fixture.db.clone(),
            crate::harness::trajectory::RecordingScope {
                owner: a.fixture.principal.to_string(),
                source: "assessment-fixture".into(),
                controller_instance_id: None,
                route_scope_id: None,
            },
        )
        .await?;
        for event in super::super::fixture::sequence(a.subject.resource_id.as_str()) {
            legacy.record(event).await?;
        }
        let rows = crate::acp_trajectory::CanonicalStore::new(a.fixture.db.clone())
            .list(a.fixture.principal.as_str(), "assessment-fixture")
            .await?;
        ensure!(rows.len() == 1 && rows[0].session_key != akey && rows[0].session_key != bkey);
        ensure!(bodies(&a.fixture, "owned_judge_job").await? == jobs);
        ensure!(judge.calls.load(Ordering::SeqCst) == 2 && provider.calls() == 0);
        Ok(())
    }
    .await;
    cleanup(&a, &ports, judge, provider, checked).await
}
