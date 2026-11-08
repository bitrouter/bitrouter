use super::*;
use crate::gateway::{
    error::BitrouterError,
    scheduling::queue::{Identity, Request, Waiting},
};
use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use tokio_util::sync::CancellationToken;

fn wait(
    scheduler: &Scheduler,
    target: &RoutingTarget,
    name: Option<&str>,
    work: u64,
    bytes: u64,
) -> Result<Waiting> {
    let (shape, _) = Shape::from_prompt(&prompt(false)?)?;
    Ok(scheduler.enqueue(
        name.map_or(Identity::Local, Identity::Tenant),
        target,
        Request {
            planning: None,
            prefix: None,
            candidate: scheduler.candidate(target, 0)?,
            work,
            shape,
            retained_bytes: bytes,
            deadline: Instant::now() + Duration::from_secs(60),
            wait_until: Instant::now() + Duration::from_secs(30),
        },
    )?)
}

#[tokio::test(start_paused = true)]
async fn queue_weights_have_observable_service_shares() -> Result<()> {
    for weight in [1, 4] {
        let target = target("https://fixture.invalid")?;
        let mut config = settings(&target.api_base, 1);
        config.tenant_weights.insert("a".into(), weight);
        let scheduler = Scheduler::new(&config, 1)?;
        let (shape, work) = Shape::from_prompt(&prompt(false)?)?;
        let blocker = scheduler.book(
            scheduler.candidate(&target, 0)?,
            &target,
            work,
            shape,
            Instant::now() + Duration::from_secs(60),
        )?;
        let mut pending = FuturesUnordered::new();
        for name in ["a", "b"] {
            for index in 0..64 {
                let waiting = wait(&scheduler, &target, Some(name), 1024, 100)?;
                pending.push(
                    async move { (name, index, waiting.receive(CancellationToken::new()).await) }
                        .boxed(),
                );
            }
        }
        drop(blocker);
        let mut counts = [0, 0];
        let mut next = [0, 0];
        for _ in 0..20 {
            let (name, index, reservation) =
                pending.next().await.context("missing queued dispatch")?;
            let flow = usize::from(name == "b");
            ensure!(index == next[flow], "lane lost FIFO");
            next[flow] += 1;
            counts[flow] += 1;
            drop(reservation?);
        }
        ensure!(
            counts == if weight == 1 { [10, 10] } else { [16, 4] },
            "shares: {counts:?}"
        );
        drop(pending);
        ensure!(scheduler.inspect_queue().queued == 0);
        scheduler.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn queue_maximum_work_preserves_high_weight_quantum_remainders() -> Result<()> {
    let target = target("https://fixture.invalid")?;
    let mut config = settings(&target.api_base, 1);
    config.tenant_weights.insert("a".into(), 64);
    config.tenant_weights.insert("b".into(), 100);
    let scheduler = Scheduler::new(&config, 1)?;
    let mut pending = FuturesUnordered::new();
    for name in ["a", "b"] {
        for _ in 0..128 {
            let waiting = wait(&scheduler, &target, Some(name), 131072, 1)?;
            pending.push(
                async move { (name, waiting.receive(CancellationToken::new()).await) }.boxed(),
            );
        }
    }
    let mut counts = [0, 0];
    for _ in 0..164 {
        let (name, reservation) = pending.next().await.context("weighted dispatch")?;
        counts[usize::from(name == "b")] += 1;
        drop(reservation?);
    }
    ensure!(counts == [64, 100], "maximum-work shares: {counts:?}");
    drop(pending);
    scheduler.shutdown().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn queue_same_tenant_large_lane_cannot_lose_credit_to_busy_small_siblings() -> Result<()> {
    let a = target("https://a.invalid")?;
    let b = target("https://b.invalid")?;
    let mut config = settings(&a.api_base, 1);
    let mut second = config.destinations[0].clone();
    second.id = "b".into();
    second.endpoint = b.api_base.clone();
    config.destinations.push(second);
    let scheduler = Scheduler::new(&config, 1)?;
    let mut pending = FuturesUnordered::new();
    for _ in 0..64 {
        let small = wait(&scheduler, &a, Some("same"), 256, 1)?;
        pending.push(async move { (false, small.receive(CancellationToken::new()).await) }.boxed());
    }
    let large = wait(&scheduler, &b, Some("same"), 131072, 1)?;
    pending.push(async move { (true, large.receive(CancellationToken::new()).await) }.boxed());
    let mut small_count = 0;
    loop {
        let (large, reservation) = pending.next().await.context("same-tenant dispatch")?;
        drop(reservation?);
        if large {
            break;
        }
        small_count += 1;
        ensure!(small_count < 512, "busy lane starved large sibling");
        let small = wait(&scheduler, &a, Some("same"), 256, 1)?;
        pending.push(async move { (false, small.receive(CancellationToken::new()).await) }.boxed());
    }
    ensure!(scheduler.inspect_queue().rounds <= 130);
    drop(pending);
    scheduler.shutdown().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn queue_saturated_destination_does_not_block_a_sibling_lane() -> Result<()> {
    let a = target("https://a.invalid")?;
    let b = target("https://b.invalid")?;
    let mut config = settings(&a.api_base, 2);
    config.destinations[0].max_active = 1;
    let mut second = config.destinations[0].clone();
    second.id = "b".into();
    second.endpoint = b.api_base.clone();
    config.destinations.push(second);
    let scheduler = Scheduler::new(&config, 1)?;
    let (shape, work) = Shape::from_prompt(&prompt(false)?)?;
    let blocker = scheduler.book(
        scheduler.candidate(&a, 0)?,
        &a,
        work,
        shape,
        Instant::now() + Duration::from_secs(60),
    )?;
    let blocked = wait(&scheduler, &a, Some("same-tenant"), 1024, 1)?;
    let ready = wait(&scheduler, &b, Some("same-tenant"), 1024, 1)?;
    let reservation = tokio::time::timeout(
        Duration::from_secs(1),
        ready.receive(CancellationToken::new()),
    )
    .await??;
    ensure!(scheduler.inspect_queue().queued == 1);
    ensure!(scheduler.inspect(shape)[1].active == 1);
    drop(blocked);
    drop(reservation);
    drop(blocker);
    ensure!(scheduler.inspect_queue().flows == 0 && scheduler.inspect_queue().lanes == 0);
    scheduler.shutdown().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn queue_cursor_crosses_ticks_and_cancelled_earning_lanes_do_not_stall() -> Result<()> {
    let a = target("https://a.invalid")?;
    let b = target("https://b.invalid")?;
    let mut config = settings(&a.api_base, 2);
    config.destinations[0].max_active = 1;
    let mut second = config.destinations[0].clone();
    second.id = "b".into();
    second.endpoint = b.api_base.clone();
    config.destinations.push(second);
    let scheduler = Scheduler::new(&config, 1)?;
    let (shape, work) = Shape::from_prompt(&prompt(false)?)?;
    let blocker = scheduler.book(
        scheduler.candidate(&a, 0)?,
        &a,
        work,
        shape,
        Instant::now() + Duration::from_secs(60),
    )?;
    let mut blocked = Vec::new();
    for index in 0..130 {
        blocked.push(wait(
            &scheduler,
            &a,
            Some(&format!("tenant-{index}")),
            256,
            1,
        )?);
    }
    let ready = wait(&scheduler, &b, Some("later"), 256, 1)?;
    let granted = tokio::time::timeout(
        Duration::from_secs(1),
        ready.receive(CancellationToken::new()),
    )
    .await??;
    ensure!(scheduler.inspect_queue().queued == 130);
    drop(granted);
    drop(blocker);
    drop(blocked);
    // Removing a flow's selected lane transfers its earning turn to a sibling.
    let selected = wait(&scheduler, &a, Some("same"), 131072, 1)?;
    let sibling = wait(&scheduler, &b, Some("same"), 256, 1)?;
    drop(selected);
    drop(
        tokio::time::timeout(
            Duration::from_secs(1),
            sibling.receive(CancellationToken::new()),
        )
        .await??,
    );
    scheduler.shutdown().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn queue_large_head_earns_credit_without_arrivals_resetting_it() -> Result<()> {
    let target = target("https://fixture.invalid")?;
    let mut config = settings(&target.api_base, 1);
    config.queue.per_tenant_requests = 1024;
    let scheduler = Scheduler::new(&config, 1)?;
    let mut pending = FuturesUnordered::new();
    let large = wait(&scheduler, &target, Some("large"), 131072, 1)?;
    pending.push(async move { (true, large.receive(CancellationToken::new()).await) }.boxed());
    for _ in 0..512 {
        let small = wait(&scheduler, &target, Some("small"), 256, 1)?;
        pending.push(async move { (false, small.receive(CancellationToken::new()).await) }.boxed());
    }
    let mut small_count = 0;
    loop {
        let (large, reservation) = pending.next().await.context("missing large dispatch")?;
        drop(reservation?);
        if large {
            break;
        }
        small_count += 1;
        ensure!(small_count <= 512);
        // New work does not reset the old lane's credit or cursor.
        let small = wait(&scheduler, &target, Some("small"), 256, 1)?;
        pending.push(async move { (false, small.receive(CancellationToken::new()).await) }.boxed());
    }
    ensure!(small_count > 0 && scheduler.inspect_queue().rounds <= 128);
    drop(pending);
    scheduler.shutdown().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn queue_count_bytes_flow_limits_and_identity_budget_are_checked_before_retention()
-> Result<()> {
    let target = target("https://fixture.invalid")?;
    let mut config = settings(&target.api_base, 1);
    config.queue.max_requests = 3;
    config.queue.per_tenant_requests = 2;
    config.queue.max_bytes = 4096;
    config.queue.per_tenant_bytes = 2048;
    let scheduler = Scheduler::new(&config, 1)?;
    let first = wait(&scheduler, &target, Some("a"), 256, 1)?;
    let second = wait(&scheduler, &target, Some("a"), 256, 1)?;
    ensure!(wait(&scheduler, &target, Some("a"), 256, 1).is_err());
    let third = wait(&scheduler, &target, Some("b"), 256, 1)?;
    ensure!(wait(&scheduler, &target, Some("c"), 256, 1).is_err());
    drop(first);
    drop(second);
    drop(third);
    let before = scheduler.inspect_queue();
    ensure!(wait(&scheduler, &target, Some("a"), 256, 2048).is_err());
    ensure!(wait(&scheduler, &target, Some("a"), 256, u64::MAX).is_err());
    ensure!(scheduler.inspect_queue().bytes == before.bytes && before.bytes == 0);
    scheduler.shutdown().await?;

    let mut config = settings(&target.api_base, 1);
    config.limits.max_state_entries = 6;
    let scheduler = Scheduler::new(&config, 1)?;
    let first = wait(&scheduler, &target, Some("a"), 256, 1)?;
    ensure!(wait(&scheduler, &target, Some("new-identity"), 256, 1).is_err());
    drop(first);
    ensure!(scheduler.inspect_queue().flows == 0);
    drop(wait(&scheduler, &target, Some("new-identity"), 256, 1)?);
    scheduler.shutdown().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn queue_cancellation_grant_handoff_and_shutdown_release_once() -> Result<()> {
    let target = target("https://fixture.invalid")?;
    let scheduler = Scheduler::new(&settings(&target.api_base, 1), 1)?;
    let (shape, _) = Shape::from_prompt(&prompt(false)?)?;
    let waiting = wait(&scheduler, &target, None, 256, 1)?;
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    ensure!(matches!(
        waiting.receive(cancellation).await,
        Err(BitrouterError::Cancelled)
    ));
    ensure!(scheduler.inspect_queue().queued == 0 && scheduler.inspect(shape)[0].active == 0);

    let waiting = wait(&scheduler, &target, None, 256, 1)?;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    ensure!(scheduler.inspect_queue().queued == 0 && scheduler.inspect(shape)[0].active == 1);
    drop(waiting);
    ensure!(scheduler.inspect(shape)[0].active == 0);
    let pending = wait(&scheduler, &target, Some("tenant"), 256, 1)?;
    scheduler.close();
    ensure!(matches!(
        pending.receive(CancellationToken::new()).await,
        Err(BitrouterError::UpstreamUnavailable)
    ));
    ensure!(scheduler.inspect_queue().queued == 0);
    scheduler.shutdown().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn queue_deadlines_are_absolute_and_a_blocked_pass_sleeps() -> Result<()> {
    for total in [false, true] {
        let target = target("https://fixture.invalid")?;
        let scheduler = Scheduler::new(&settings(&target.api_base, 1), 1)?;
        let (shape, work) = Shape::from_prompt(&prompt(false)?)?;
        let blocker = scheduler.book(
            scheduler.candidate(&target, 0)?,
            &target,
            work,
            shape,
            Instant::now() + Duration::from_secs(60),
        )?;
        let deadline = Instant::now() + Duration::from_millis(if total { 50 } else { 500 });
        let wait_until = Instant::now() + Duration::from_millis(100);
        let candidate = scheduler.candidate(&target, 0)?;
        let queued = scheduler.enqueue(
            Identity::Local,
            &target,
            Request {
                planning: None,
                prefix: None,
                candidate,
                work,
                shape,
                deadline,
                wait_until,
                retained_bytes: 1,
            },
        )?;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        let rounds = scheduler.inspect_queue().rounds;
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        ensure!(
            scheduler.inspect_queue().rounds == rounds,
            "blocked queue busy polls"
        );
        tokio::time::advance(Duration::from_millis(if total { 50 } else { 100 })).await;
        let result = queued.receive(CancellationToken::new()).await;
        if total {
            ensure!(matches!(result, Err(BitrouterError::UpstreamTimeout)));
        } else {
            ensure!(matches!(result, Err(BitrouterError::RateLimited { .. })));
        }
        ensure!(scheduler.inspect_queue().queued == 0);
        // Reusing the request's frozen budgets cannot grant a renewed wait.
        ensure!(
            scheduler
                .enqueue(
                    Identity::Local,
                    &target,
                    Request {
                        planning: None,
                        prefix: None,
                        candidate,
                        work,
                        shape,
                        deadline,
                        wait_until,
                        retained_bytes: 1
                    }
                )
                .is_err()
        );
        drop(blocker);
        scheduler.shutdown().await?;
    }
    Ok(())
}
