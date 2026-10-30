use super::*;

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
    lease.confirm_receipt(101, None).map_err(failure)?;
    ensure!(totals(&admission).0 == 1);
    drop(lease);
    admission.wait_drained().await;
    ensure!(totals(&admission) == (0, 0, 0));
    ensure!(!admission.receipts_ready());
    Ok(())
}
