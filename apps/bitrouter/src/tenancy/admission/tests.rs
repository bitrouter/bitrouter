use std::{sync::Arc, task::Poll, time::Duration};

use anyhow::{Result, ensure};
use futures::{FutureExt, poll};

use super::{Admission, Lease, Limits, Profile, TenantId};
use bitrouter_ai::tenancy::operation::ErrorCode;

fn limits(active: u16, queued: u16) -> Limits {
    Limits {
        active_executions: active,
        queued_work: queued,
        learning_jobs: 0,
    }
}

fn narrow(
    tenant_active: usize,
    tenant_queue: usize,
    active: usize,
    queued: usize,
) -> Arc<Admission> {
    Admission::with_profile(Profile {
        tenant_active,
        tenant_queue,
        instance_active: active,
        instance_queue: queued,
    })
}

fn failure(code: ErrorCode) -> anyhow::Error {
    anyhow::anyhow!("admission failed: {code:?}")
}

fn totals(admission: &Admission) -> (usize, usize, usize) {
    let state = admission.lock();
    (state.active, state.queue.len(), state.tenants.len())
}

fn tenant_totals(admission: &Admission, tenant: &TenantId) -> (usize, usize) {
    let state = admission.lock();
    state
        .tenants
        .get(tenant)
        .map_or((0, 0), |counts| (counts.active, counts.queued))
}

#[test]
fn lease_and_controller_are_send_sync() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Lease>();
    send_sync::<Admission>();
}

#[tokio::test(start_paused = true)]
async fn request_deadline_precedes_queue_timeout_and_free_capacity() -> Result<()> {
    let admission = narrow(1, 2, 1, 2);
    let tenant: TenantId = "short-request".try_into()?;
    let requested = limits(1, 2);
    let expired = tokio::time::Instant::now();
    ensure!(matches!(
        admission.acquire_until(&tenant, &requested, expired).await,
        Err(ErrorCode::CapacityExceeded)
    ));
    ensure!(totals(&admission) == (0, 0, 0));

    for release_before_poll in [false, true] {
        let active = admission
            .acquire(&tenant, &requested)
            .await
            .map_err(failure)?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
        let mut waiting = Box::pin(admission.acquire_until(&tenant, &requested, deadline));
        ensure!(matches!(poll!(waiting.as_mut()), Poll::Pending));
        ensure!(totals(&admission) == (1, 1, 1));
        tokio::time::advance(Duration::from_millis(250)).await;
        // A released/notified slot cannot revive an already expired request.
        let active = if release_before_poll {
            drop(active);
            None
        } else {
            Some(active)
        };
        ensure!(matches!(waiting.await, Err(ErrorCode::CapacityExceeded)));
        ensure!(tenant_totals(&admission, &tenant) == (usize::from(active.is_some()), 0));
        drop(active);
        admission.wait_drained().await;
        ensure!(totals(&admission) == (0, 0, 0));
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn fixed_profile_bounds_tenant_and_instance_totals() -> Result<()> {
    let admission = Admission::new();
    let requested = limits(128, 256);
    let no_queue = limits(128, 0);
    let tenants = (0..8)
        .map(|index| format!("tenant-{index}").try_into())
        .collect::<std::result::Result<Vec<TenantId>, _>>()?;
    let mut active = Vec::new();
    for tenant in &tenants {
        for _ in 0..16 {
            active.push(
                admission
                    .acquire(tenant, &requested)
                    .await
                    .map_err(failure)?,
            );
        }
        ensure!(matches!(
            admission.acquire(tenant, &no_queue).await,
            Err(ErrorCode::CapacityExceeded)
        ));
    }
    ensure!(totals(&admission) == (128, 0, 8));
    let mut queued = Vec::new();
    for tenant in &tenants {
        for _ in 0..32 {
            let mut waiting = Box::pin(admission.acquire(tenant, &requested));
            ensure!(matches!(poll!(waiting.as_mut()), Poll::Pending));
            queued.push(waiting);
        }
        ensure!(matches!(
            admission.acquire(tenant, &requested).await,
            Err(ErrorCode::CapacityExceeded)
        ));
    }
    let extra: TenantId = "tenant-over-instance-bound".try_into()?;
    ensure!(matches!(
        admission.acquire(&extra, &requested).await,
        Err(ErrorCode::CapacityExceeded)
    ));
    ensure!(totals(&admission) == (128, 256, 8));
    admission.close();
    for waiting in queued {
        ensure!(matches!(
            waiting.await,
            Err(ErrorCode::AuthorityUnavailable)
        ));
    }
    ensure!(totals(&admission) == (128, 0, 8));
    drop(active);
    admission.wait_drained().await;
    ensure!(totals(&admission) == (0, 0, 0));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn requested_limits_share_one_tenant_counter_across_configurations() -> Result<()> {
    let admission = narrow(2, 3, 3, 4);
    let tenant: TenantId = "same-owner".try_into()?;
    let old_configuration = limits(2, 3);
    let new_configuration = limits(1, 1);
    let first = admission
        .acquire(&tenant, &old_configuration)
        .await
        .map_err(failure)?;
    let second = admission
        .acquire(&tenant, &old_configuration)
        .await
        .map_err(failure)?;
    let mut waiting = Box::pin(admission.acquire(&tenant, &new_configuration));
    ensure!(matches!(poll!(waiting.as_mut()), Poll::Pending));
    ensure!(tenant_totals(&admission, &tenant) == (2, 1));
    ensure!(matches!(
        admission.acquire(&tenant, &new_configuration).await,
        Err(ErrorCode::CapacityExceeded)
    ));
    drop(first);
    ensure!(matches!(poll!(waiting.as_mut()), Poll::Pending));
    ensure!(tenant_totals(&admission, &tenant) == (1, 1));
    drop(second);
    let lease = waiting.await.map_err(failure)?;
    ensure!(tenant_totals(&admission, &tenant) == (1, 0));
    drop(lease);
    ensure!(totals(&admission) == (0, 0, 0));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn blocked_tenant_does_not_block_others_or_lose_its_fifo_order() -> Result<()> {
    let admission = narrow(2, 2, 3, 4);
    let a: TenantId = "fifo-a".try_into()?;
    let b: TenantId = "fifo-b".try_into()?;
    let one = limits(1, 2);
    let two = limits(2, 2);
    let a_active = admission.acquire(&a, &one).await.map_err(failure)?;
    let mut older = Box::pin(admission.acquire(&a, &one));
    let mut newer = Box::pin(admission.acquire(&a, &two));
    ensure!(matches!(poll!(older.as_mut()), Poll::Pending));
    // The wider configuration cannot bypass the older same-tenant waiter.
    ensure!(matches!(poll!(newer.as_mut()), Poll::Pending));
    let b_first = admission.acquire(&b, &two).await.map_err(failure)?;
    let b_second = admission.acquire(&b, &two).await.map_err(failure)?;
    ensure!(totals(&admission) == (3, 2, 2));
    drop(a_active);
    ensure!(matches!(poll!(newer.as_mut()), Poll::Pending));
    let a_first = older.await.map_err(failure)?;
    ensure!(matches!(poll!(newer.as_mut()), Poll::Pending));
    drop(b_first);
    let a_second = newer.await.map_err(failure)?;
    ensure!(tenant_totals(&admission, &a) == (2, 0));
    drop((a_first, a_second, b_second));
    ensure!(totals(&admission) == (0, 0, 0));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn bounded_queues_and_zero_limits_fail_without_retaining_extra_tenants() -> Result<()> {
    let admission = narrow(1, 1, 1, 2);
    let a: TenantId = "bounded-a".try_into()?;
    let b: TenantId = "bounded-b".try_into()?;
    let c: TenantId = "bounded-c".try_into()?;
    let requested = limits(1, 1);
    let disabled = limits(0, 1);
    let no_queue = limits(1, 0);
    ensure!(matches!(
        admission.acquire(&a, &disabled).now_or_never(),
        Some(Err(ErrorCode::CapacityExceeded))
    ));
    ensure!(totals(&admission) == (0, 0, 0));
    let active = admission.acquire(&a, &no_queue).await.map_err(failure)?;
    ensure!(matches!(
        admission.acquire(&b, &no_queue).now_or_never(),
        Some(Err(ErrorCode::CapacityExceeded))
    ));
    let mut a_wait = Box::pin(admission.acquire(&a, &requested));
    let mut b_wait = Box::pin(admission.acquire(&b, &requested));
    ensure!(matches!(poll!(a_wait.as_mut()), Poll::Pending));
    ensure!(matches!(poll!(b_wait.as_mut()), Poll::Pending));
    ensure!(matches!(
        admission.acquire(&a, &requested).now_or_never(),
        Some(Err(ErrorCode::CapacityExceeded))
    ));
    ensure!(matches!(
        admission.acquire(&c, &requested).now_or_never(),
        Some(Err(ErrorCode::CapacityExceeded))
    ));
    ensure!(totals(&admission) == (1, 2, 2));
    drop(b_wait);
    ensure!(totals(&admission) == (1, 1, 1));
    let mut c_wait = Box::pin(admission.acquire(&c, &requested));
    ensure!(matches!(poll!(c_wait.as_mut()), Poll::Pending));
    ensure!(totals(&admission) == (1, 2, 2));
    drop((a_wait, c_wait, active));
    ensure!(totals(&admission) == (0, 0, 0));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn dropping_notified_waiter_wakes_the_next_without_reserving_capacity() -> Result<()> {
    let admission = narrow(1, 1, 1, 2);
    let a: TenantId = "cancel-a".try_into()?;
    let b: TenantId = "cancel-b".try_into()?;
    let c: TenantId = "cancel-c".try_into()?;
    let requested = limits(1, 1);
    let active = admission.acquire(&a, &requested).await.map_err(failure)?;
    let mut first = Box::pin(admission.acquire(&b, &requested));
    let mut second = Box::pin(admission.acquire(&c, &requested));
    ensure!(matches!(poll!(first.as_mut()), Poll::Pending));
    ensure!(matches!(poll!(second.as_mut()), Poll::Pending));
    drop(active);
    ensure!(totals(&admission) == (0, 2, 2));
    // Notification has happened, but its recipient has not polled again.
    drop(first);
    ensure!(totals(&admission) == (0, 1, 1));
    let lease = second.await.map_err(failure)?;
    ensure!(totals(&admission) == (1, 0, 1));
    drop(lease);
    admission.wait_drained().await;
    ensure!(totals(&admission) == (0, 0, 0));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn queued_acquisition_expires_at_five_seconds_without_releasing_an_active_lease() -> Result<()>
{
    let admission = narrow(1, 1, 1, 1);
    let tenant: TenantId = "queue-deadline".try_into()?;
    let requested = limits(1, 1);
    let active = admission
        .acquire(&tenant, &requested)
        .await
        .map_err(failure)?;
    let mut waiting = Box::pin(admission.acquire(&tenant, &requested));
    ensure!(matches!(poll!(waiting.as_mut()), Poll::Pending));
    tokio::time::advance(Duration::from_millis(4999)).await;
    ensure!(matches!(poll!(waiting.as_mut()), Poll::Pending));
    tokio::time::advance(Duration::from_millis(1)).await;
    ensure!(matches!(waiting.await, Err(ErrorCode::CapacityExceeded)));
    ensure!(totals(&admission) == (1, 0, 1));
    drop(active);
    ensure!(totals(&admission) == (0, 0, 0));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn expired_unpolled_waiter_does_not_block_later_admission() -> Result<()> {
    let admission = narrow(1, 1, 1, 1);
    let a: TenantId = "expired-a".try_into()?;
    let b: TenantId = "expired-b".try_into()?;
    let requested = limits(1, 1);
    let active = admission.acquire(&a, &requested).await.map_err(failure)?;
    let mut waiting = Box::pin(admission.acquire(&b, &requested));
    ensure!(matches!(poll!(waiting.as_mut()), Poll::Pending));
    tokio::time::advance(Duration::from_secs(5)).await;
    drop(active);
    let next = admission.acquire(&a, &requested).await.map_err(failure)?;
    ensure!(matches!(waiting.await, Err(ErrorCode::CapacityExceeded)));
    ensure!(totals(&admission) == (1, 0, 1));
    drop(next);
    ensure!(totals(&admission) == (0, 0, 0));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn close_wakes_queued_callers_and_all_drainers_wait_for_active_leases() -> Result<()> {
    let admission = narrow(1, 1, 1, 1);
    let tenant: TenantId = "shutdown-owner".try_into()?;
    let requested = limits(1, 1);
    let active = admission
        .acquire(&tenant, &requested)
        .await
        .map_err(failure)?;
    let mut waiting = Box::pin(admission.acquire(&tenant, &requested));
    let mut first_drain = Box::pin(admission.wait_drained());
    let mut second_drain = Box::pin(admission.wait_drained());
    ensure!(matches!(poll!(waiting.as_mut()), Poll::Pending));
    ensure!(matches!(poll!(first_drain.as_mut()), Poll::Pending));
    ensure!(matches!(poll!(second_drain.as_mut()), Poll::Pending));
    admission.close();
    admission.close();
    ensure!(totals(&admission) == (1, 0, 1));
    ensure!(matches!(
        waiting.await,
        Err(ErrorCode::AuthorityUnavailable)
    ));
    ensure!(matches!(
        admission.acquire(&tenant, &requested).await,
        Err(ErrorCode::AuthorityUnavailable)
    ));
    ensure!(matches!(poll!(first_drain.as_mut()), Poll::Pending));
    drop(active);
    first_drain.await;
    second_drain.await;
    ensure!(totals(&admission) == (0, 0, 0));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn multiple_releases_before_repoll_preserve_fifo_and_fill_all_slots() -> Result<()> {
    let admission = narrow(2, 2, 2, 2);
    let a: TenantId = "release-a".try_into()?;
    let b: TenantId = "release-b".try_into()?;
    let requested = limits(2, 2);
    let first_active = admission.acquire(&a, &requested).await.map_err(failure)?;
    let second_active = admission.acquire(&a, &requested).await.map_err(failure)?;
    let mut first = Box::pin(admission.acquire(&b, &requested));
    let mut second = Box::pin(admission.acquire(&b, &requested));
    ensure!(matches!(poll!(first.as_mut()), Poll::Pending));
    ensure!(matches!(poll!(second.as_mut()), Poll::Pending));
    drop((first_active, second_active));
    ensure!(matches!(poll!(second.as_mut()), Poll::Pending));
    let first_lease = first.await.map_err(failure)?;
    let second_lease = second.await.map_err(failure)?;
    ensure!(totals(&admission) == (2, 0, 1));
    drop((first_lease, second_lease));
    ensure!(totals(&admission) == (0, 0, 0));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn close_after_release_before_repoll_cannot_turn_a_wakeup_into_a_lease() -> Result<()> {
    let admission = narrow(1, 1, 1, 1);
    let tenant: TenantId = "close-race".try_into()?;
    let requested = limits(1, 1);
    let active = admission
        .acquire(&tenant, &requested)
        .await
        .map_err(failure)?;
    let mut waiting = Box::pin(admission.acquire(&tenant, &requested));
    ensure!(matches!(poll!(waiting.as_mut()), Poll::Pending));
    drop(active);
    admission.close();
    // Draining must not depend on the already-notified future being polled.
    admission.wait_drained().await;
    ensure!(totals(&admission) == (0, 0, 0));
    ensure!(matches!(
        waiting.await,
        Err(ErrorCode::AuthorityUnavailable)
    ));
    ensure!(totals(&admission) == (0, 0, 0));
    Ok(())
}
