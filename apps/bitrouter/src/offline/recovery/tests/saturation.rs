//! Opt-in history crossing the old ledger limits through the normal SDK.
use super::*;
use bitrouter_ai::tenancy::{
    authority::{TenantPut, TenantState},
    identity::TenantId,
    operation::{ErrorCode, OperationStatus},
};
use bitrouter_sdk::{error::Error as SdkError, tenancy::TenancyClient};

// Historical checkpoints only: format 2 has no operation-count ceiling.
const LEGACY_ORDINARY: u32 = 99_488;
const LEGACY_SAFETY: u32 = 512;
const OPERATIONS: u32 = LEGACY_ORDINARY + LEGACY_SAFETY + 2;

pub(super) async fn fill(live: &fixture::Live, provision: &fixture::Provision) -> Result<()> {
    let client = live.operator()?;
    let tenant: TenantId = "tenant".try_into()?;
    let initial = client
        .operation(&provision.installation, &"tenant".try_into()?)
        .await?;
    let mut expected = initial
        .resource_revision
        .context("actual tenant revision")?;
    // Normal provisioning already accepted exactly four ordinary commands.
    ensure!(provision.operations.len() == 4);
    for ordinal in 4..LEGACY_ORDINARY {
        let result = client
            .put_tenant(
                &tenant,
                &TenantPut {
                    operation_id: format!("saturation-ordinary-{ordinal}")
                        .as_str()
                        .try_into()?,
                    expected_revision: Some(expected),
                    state: TenantState::Active,
                    installation_id: provision.installation.clone(),
                },
            )
            .await?;
        ensure!(result.status == OperationStatus::Applied);
        expected = result
            .resource_revision
            .context("actual next tenant revision")?;
    }
    let crossing = client
        .put_tenant(
            &tenant,
            &TenantPut {
                operation_id: "history-ordinary-crossing".try_into()?,
                expected_revision: Some(expected),
                state: TenantState::Active,
                installation_id: provision.installation.clone(),
            },
        )
        .await?;
    ensure!(crossing.status == OperationStatus::Applied);
    expected = crossing
        .resource_revision
        .clone()
        .context("ordinary crossing revision")?;
    let mut crossings = vec![crossing];
    // Stop commands remain usable past the old ordinary boundary. Without a
    // legacy saturation fence, they are retained as ordinary operation history.
    for ordinal in 0..LEGACY_SAFETY {
        let result = client
            .put_tenant(
                &tenant,
                &TenantPut {
                    operation_id: format!("saturation-safety-{ordinal}").as_str().try_into()?,
                    expected_revision: Some(expected),
                    state: if ordinal + 1 == LEGACY_SAFETY {
                        // The 100,001st write must admit ordinary Active work,
                        // not merely permit another stop command after a fence.
                        TenantState::Active
                    } else {
                        TenantState::Suspended
                    },
                    installation_id: provision.installation.clone(),
                },
            )
            .await?;
        ensure!(result.status == OperationStatus::Applied);
        expected = result
            .resource_revision
            .clone()
            .context("actual safety revision")?;
        if ordinal + 1 == LEGACY_SAFETY {
            crossings.push(result);
        }
    }
    let crossing = client
        .put_tenant(
            &tenant,
            &TenantPut {
                operation_id: "history-total-crossing".try_into()?,
                expected_revision: Some(expected),
                state: TenantState::Deleting,
                installation_id: provision.installation.clone(),
            },
        )
        .await?;
    ensure!(crossing.status == OperationStatus::Applied);
    crossings.push(crossing);
    for expected in crossings {
        let actual = client
            .operation(&provision.installation, &expected.operation_id)
            .await?;
        ensure!(serde_json::to_value(actual)? == serde_json::to_value(expected)?);
    }
    for name in [
        "tenant",
        "saturation-ordinary-99487",
        "history-ordinary-crossing",
        "saturation-safety-511",
        "history-total-crossing",
    ] {
        let row = client
            .operation(&provision.installation, &name.try_into()?)
            .await?;
        ensure!(row.status == OperationStatus::Applied && row.operation_id.as_str() == name);
    }
    Ok(())
}

fn digest(path: &Path) -> Result<(u64, String)> {
    use sha2::{Digest, Sha256};
    let file = std::fs::File::open(path)?;
    let expected = file.metadata()?.len();
    ensure!(
        expected <= format::MAX_FILE,
        "actual fixture exceeded product file bound"
    );
    let mut reader = file.take(format::MAX_FILE + 1);
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut count = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        count += u64::try_from(read)?;
        ensure!(count <= format::MAX_FILE);
        hash.update(&buffer[..read]);
    }
    ensure!(count == expected, "stopped fixture changed during read");
    Ok((count, hex::encode(hash.finalize())))
}

#[tokio::test]
#[ignore = "100,002 actual SDK operations and bro recovery; explicit binary/key required"]
async fn actual_history_crosses_legacy_limits_and_fresh_target_keeps_independent_history()
-> Result<()> {
    let binary = binary()?;
    tokio::time::timeout(Duration::from_secs(30 * 60), Box::pin(async {
        let fixture = Fixture::with_large_history_source(true).await?;
        let source = digest(&fixture.source)?;
        let target = digest(&fixture.target)?;
        let archive = digest(&fixture.bundle.join("snapshot.sqlite"))?;
        let evidence = files::read(&fixture.bundle.join("evidence.json"), format::MAX_EVIDENCE)?;
        let plan = fixture.directory.path().join("full-source-plan.json");
        fixture::private_json(&plan, &fixture.plan)?;
        let report = fixture.directory.path().join("full-source-verified");
        // Observe the existing 120-second recovery work budget plus startup and
        // cleanup margin. Product deadlines and the full-ledger assertions stay
        // unchanged; the ordinary small-fixture CLI observer remains 20 seconds.
        let started = Instant::now();
        eprintln!("full-ledger recovery CLI started");
        let result = invoke_with_observation(&binary, &fixture, &plan, &report, None, Duration::from_secs(140)).await?;
        eprintln!("full-ledger recovery CLI exited: elapsed_ms={}, status={}", started.elapsed().as_millis(), result.status);
        ensure!(result.status.code() == Some(0), "actual full-source CLI failed: {}", result.status);
        let output: Value = serde_json::from_slice(&result.stdout)?;
        ensure!(output["fresh_provisioning_verified"] == true
            && output["static_closure_authenticated"] == true
            && output["source"]["ordinary_operations"] == OPERATIONS
            && output["source"]["safety_operations"] == 0
            && output["source"]["saturated"] == false
            && output["target"]["ordinary_operations"] == 6
            && output["target"]["safety_operations"] == 0
            && output["target"]["saturated"] == false
            && output["complete"] == false && output["cutover_ready"] == false
            && output["external_writers_independently_excluded"] == false
            && output["history_transferred"] == false && output["upstream_effects_resolved"] == false);
        let bytes = files::read(&report.join("report.json"), format::MAX_EVIDENCE)?;
        fixture.secret_free(&bytes)?;
        ensure!(files::read(&report.join("VERIFIED"), 65)? == format!("{}\n", files::sha(&bytes)).as_bytes()
            && !report.join("INCOMPLETE").exists());
        ensure!(digest(&fixture.source)? == source && digest(&fixture.target)? == target
            && digest(&fixture.bundle.join("snapshot.sqlite"))? == archive
            && files::read(&fixture.bundle.join("evidence.json"), format::MAX_EVIDENCE)? == evidence);
        let live = fixture::Live::open(&fixture.target, None, Some(fixture::KEY_ENV)).await?;
        let checked = async {
            let client = TenancyClient::new(&live.base, &fixture.target_provision.secret)?;
            let expected: bitrouter_ai::tenancy::configuration::ConfigurationRevision =
                serde_json::from_value(fixture.provisioning["configurations"][0].clone())?;
            let read = client.configuration(&fixture.target_provision.installation, &expected.tenant_id, &expected.revision_id).await?;
            ensure!(serde_json::to_value(read)? == serde_json::to_value(expected)?);
            let operator = live.operator()?;
            let missing = operator.operation(&fixture.target_provision.installation, &"saturation-ordinary-99487".try_into()?).await
                .err().context("old source operation silently appeared in new installation")?;
            ensure!(matches!(missing, SdkError::Tenancy(failure) if failure.status == 404
                && failure.body.as_ref().is_some_and(|body| body.code == ErrorCode::NotFound)));
            let old_key = TenancyClient::new(&live.base, &fixture.source_provision.secret)?;
            ensure!(matches!(old_key.capabilities().await, Err(SdkError::Tenancy(failure)) if failure.status == 401));
            // New work belongs to the fresh target. It leaves the source history
            // intact and deliberately invalidates its fresh-state report.
            let principal: bitrouter_ai::tenancy::identity::PrincipalId = "new-target-principal".try_into()?;
            let created = operator.put_principal(&principal, &bitrouter_ai::tenancy::authority::PrincipalPut {
                operation_id: "target-after-full-source-recovery".try_into()?, expected_revision: None,
                issuer: "normal-target-client".into(), subject: "new-principal".into(),
                installation_id: fixture.target_provision.installation.clone(),
            }).await?;
            ensure!(created.status == OperationStatus::Applied);
            let retained = operator.operation(&fixture.target_provision.installation, &created.operation_id).await?;
            ensure!(serde_json::to_value(retained)? == serde_json::to_value(created)?);
            Ok::<_, anyhow::Error>(())
        }.await;
        let stopped = live.close(&fixture.target).await;
        checked?; stopped?;
        ensure!(digest(&fixture.source)? == source && digest(&fixture.bundle.join("snapshot.sqlite"))? == archive);
        Ok::<_, anyhow::Error>(())
    })).await.context("whole full-ledger recovery campaign exceeded its finite aggregate deadline")?
}
