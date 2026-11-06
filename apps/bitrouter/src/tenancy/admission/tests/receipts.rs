use super::*;
use anyhow::Context;

fn readiness_with_accounting_held(admission: &Arc<Admission>) -> Result<bool> {
    std::thread::scope(|scope| {
        let held = admission
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("accounting lock unavailable"))?;
        let (reply, receive) = std::sync::mpsc::channel();
        let reader = scope.spawn(move || {
            reply
                .send(admission.receipts_ready())
                .context("readiness observer disconnected")
        });
        let observed = receive.recv_timeout(Duration::from_secs(5));
        // A blocking implementation must fail rather than deadlock test cleanup.
        drop(held);
        reader
            .join()
            .map_err(|_| anyhow::anyhow!("readiness reader thread failed"))??;
        observed.context("readiness waited for accounting")
    })
}

#[test]
fn receipt_readiness_ignores_contention_and_retains_an_explicit_fence() -> Result<()> {
    let admission = Admission::new();
    ensure!(readiness_with_accounting_held(&admission)?);
    admission.fence_receipts();
    ensure!(!readiness_with_accounting_held(&admission)?);
    admission.receipt_probe(u64::MAX);
    ensure!(!readiness_with_accounting_held(&admission)?);
    Ok(())
}

#[test]
#[cfg(unix)]
fn failed_volume_binding_publishes_a_permanent_receipt_fence() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let volume = directory.path().join("receipt-volume");
    let admission = Admission::new();
    ensure!(matches!(
        admission.bind_receipt_volume(Some(volume.clone())),
        Err(ErrorCode::StorageUnavailable)
    ));
    ensure!(!readiness_with_accounting_held(&admission)?);
    std::fs::create_dir(&volume)?;
    admission.receipt_probe(u64::MAX);
    ensure!(!readiness_with_accounting_held(&admission)?);
    Ok(())
}

#[test]
#[cfg(unix)]
fn failed_volume_refresh_publishes_a_permanent_receipt_fence() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let volume = directory.path().join("receipt-volume");
    std::fs::create_dir(&volume)?;
    let admission = Admission::new();
    admission
        .bind_receipt_volume(Some(volume.clone()))
        .map_err(failure)?;
    ensure!(readiness_with_accounting_held(&admission)?);
    std::fs::remove_dir(&volume)?;
    admission.receipt_probe(u64::MAX);
    ensure!(!readiness_with_accounting_held(&admission)?);
    std::fs::create_dir(&volume)?;
    admission.receipt_probe(u64::MAX);
    ensure!(!readiness_with_accounting_held(&admission)?);
    Ok(())
}

#[test]
fn receipt_readiness_denies_poison_before_the_fence_can_be_published() -> Result<()> {
    let admission = Admission::new();
    let owner = admission.clone();
    let poisoned = std::thread::spawn(move || {
        owner.update(|state| {
            state.receipts.fenced = true;
            std::panic::resume_unwind(Box::new("intentional accounting poison"));
        });
    })
    .join();
    ensure!(poisoned.is_err());
    ensure!(admission.state.is_poisoned());
    ensure!(
        !admission
            .receipt_fenced
            .load(std::sync::atomic::Ordering::Acquire),
        "unwinding unexpectedly published the receipt fence"
    );
    ensure!(!admission.receipts_ready());
    match admission.state.lock() {
        Err(guard) => ensure!(!guard.into_inner().closed, "readiness recovered poison"),
        Ok(_) => anyhow::bail!("expected poisoned accounting"),
    }
    Ok(())
}

#[tokio::test]
#[cfg(unix)]
async fn real_volume_admission_does_not_trust_a_stale_free_space_value() -> Result<()> {
    let volume = tempfile::tempdir()?;
    let mut config = crate::config::tenancy::capacity::CapacityConfig::default();
    config.receipts.admission_free_bytes = i64::MAX as u64;
    config.validate()?;
    let admission = Admission::configured(&config);
    admission
        .bind_receipt_volume(Some(volume.path().to_owned()))
        .map_err(failure)?;
    admission.receipt_probe(u64::MAX);
    let tenant: TenantId = "physical-budget".try_into()?;
    ensure!(matches!(
        admission.acquire(&tenant, &limits(1, 0)).await,
        Err(ErrorCode::CapacityExceeded)
    ));
    ensure!(admission.lock().receipts.free < i64::MAX as u64);
    ensure!(totals(&admission) == (0, 0, 0));
    Ok(())
}

#[tokio::test]
#[cfg(unix)]
async fn bound_volume_pin_and_grant_check_space_without_an_external_probe() -> Result<()> {
    let volume = tempfile::tempdir()?;
    let mut config = crate::config::tenancy::capacity::CapacityConfig::default();
    config.receipts.admission_free_bytes = i64::MAX as u64;
    config.validate()?;
    let admission = Admission::configured(&config);
    admission
        .bind_receipt_volume(Some(volume.path().to_owned()))
        .map_err(failure)?;
    let tenant: TenantId = "bound-without-external-probe".try_into()?;
    ensure!(matches!(
        admission.pin_unknown(&tenant, false),
        Err(ErrorCode::CapacityExceeded)
    ));
    ensure!(matches!(
        admission.acquire(&tenant, &limits(1, 0)).await,
        Err(ErrorCode::CapacityExceeded)
    ));
    ensure!(
        admission.receipts_ready(),
        "low space is not a storage fence"
    );
    ensure!(admission.receipt_capacity_for_test().map_err(failure)?.0 == 0);
    ensure!(totals(&admission) == (0, 0, 0));
    Ok(())
}

#[tokio::test]
#[cfg(unix)]
async fn bound_volume_loss_at_pin_or_grant_permanently_fences_without_external_probe() -> Result<()>
{
    for fail_at_pin in [true, false] {
        let directory = tempfile::tempdir()?;
        let volume = directory.path().join("bound-volume");
        std::fs::create_dir(&volume)?;
        let admission = Admission::new();
        admission
            .bind_receipt_volume(Some(volume.clone()))
            .map_err(failure)?;
        let tenant: TenantId = "lost-bound-volume".try_into()?;
        let pin = admission
            .pin_unknown(&tenant, false)
            .map_err(failure)?
            .context("zero unknown count must pin without SQL")?;
        std::fs::remove_dir(&volume)?;
        if fail_at_pin {
            ensure!(matches!(
                admission.pin_unknown(&tenant, false),
                Err(ErrorCode::AuthorityUnavailable)
            ));
        } else {
            ensure!(matches!(
                admission.acquire(&tenant, &limits(1, 0)).await,
                Err(ErrorCode::CapacityExceeded)
            ));
        }
        ensure!(!admission.receipts_ready());
        ensure!(admission.receipt_capacity_for_test().map_err(failure)?.0 == 0);
        std::fs::create_dir(&volume)?;
        ensure!(matches!(
            admission.pin_unknown(&tenant, false),
            Err(ErrorCode::AuthorityUnavailable)
        ));
        ensure!(matches!(
            admission.acquire(&tenant, &limits(1, 0)).await,
            Err(ErrorCode::CapacityExceeded)
        ));
        ensure!(
            !admission.receipts_ready(),
            "successful probe cleared fence"
        );
        drop(pin);
        ensure!(totals(&admission) == (0, 0, 0));
    }
    Ok(())
}

#[test]
fn ingress_byte_and_count_budgets_release_exactly_once_before_any_execution() -> Result<()> {
    let admission = Admission::new();
    let mut bodies = Vec::new();
    for _ in 0..4 {
        bodies.push(admission.ingress(16 * 1024 * 1024).map_err(failure)?);
    }
    ensure!(matches!(
        admission.ingress(1),
        Err(ErrorCode::CapacityExceeded)
    ));
    ensure!(matches!(
        admission.ingress(16 * 1024 * 1024 + 1),
        Err(ErrorCode::PayloadTooLarge)
    ));
    ensure!(totals(&admission) == (0, 0, 0));
    drop(bodies.pop());
    let replacement = admission.ingress(16 * 1024 * 1024).map_err(failure)?;
    ensure!(admission.lock().ingress_bytes == 64 * 1024 * 1024);
    drop((bodies, replacement));
    {
        let state = admission.lock();
        ensure!(state.ingress_bytes == 0 && state.ingress == 0);
    }
    let mut empty = Vec::new();
    for _ in 0..128 {
        empty.push(admission.ingress(0).map_err(failure)?);
    }
    ensure!(matches!(
        admission.ingress(0),
        Err(ErrorCode::CapacityExceeded)
    ));
    drop(empty);
    ensure!(admission.lock().ingress == 0);
    admission.close();
    ensure!(matches!(
        admission.ingress(0),
        Err(ErrorCode::AuthorityUnavailable)
    ));
    Ok(())
}

#[tokio::test]
async fn receipt_fence_does_not_free_an_active_or_pending_completion_reservation() -> Result<()> {
    let admission = Admission::new();
    let tenant: TenantId = "receipt-owner".try_into()?;
    let lease = admission
        .acquire(&tenant, &limits(1, 0))
        .await
        .map_err(failure)?;
    admission.fence_receipts();
    ensure!(totals(&admission).0 == 1);
    ensure!(matches!(
        admission.acquire(&tenant, &limits(1, 0)).await,
        Err(ErrorCode::CapacityExceeded)
    ));
    // Only the admitted owner can confirm its terminal evidence before releasing
    // the ordinary active lease. Fencing cannot manufacture that confirmation.
    ensure!(lease.confirm_receipt(101, None).is_err());
    ensure!(totals(&admission).0 == 1);
    drop(lease);
    admission.wait_drained().await;
    ensure!(totals(&admission) == (0, 0, 0));
    ensure!(!admission.receipts_ready());
    Ok(())
}
