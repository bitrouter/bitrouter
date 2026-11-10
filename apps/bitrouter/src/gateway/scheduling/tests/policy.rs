use super::*;
use crate::config::inference_routing::Policy;
use crate::gateway::scheduling::{
    bridge::Prepared,
    queue::{Identity, Request},
};
use crate::gateway::{
    caller::CallerContext,
    pipeline::{context::PipelineContext, types::PipelineRequest},
};
use tokio_util::sync::CancellationToken;

fn fixture(policy: Policy) -> Result<(Arc<Scheduler>, Vec<RoutingTarget>)> {
    let first = target("https://fixture.invalid")?;
    let mut second = first.clone();
    second.provider_name = "backup".into();
    let mut config = settings(&first.api_base, 16);
    config.policy = policy;
    config.destinations[0].max_active = 8;
    let mut backup = config.destinations[0].clone();
    backup.id = "backup".into();
    backup.provider = "backup".into();
    config.destinations.push(backup);
    Ok((Scheduler::new(&config, 1)?, vec![first, second]))
}

fn prepare(
    scheduler: &Scheduler,
    chain: &Vec<RoutingTarget>,
    stream: bool,
) -> Result<Arc<Prepared>> {
    let mut prompt = prompt(stream)?;
    prompt.system = Some("shared prefix ".repeat(128));
    let mut ctx = PipelineContext::new(PipelineRequest::new(
        "model",
        CallerContext::local(),
        prompt,
    ));
    scheduler.prepare(chain, &mut ctx, vec![true; chain.len()])?;
    ctx.extension::<Prepared>().context("prepared")
}

fn waiting(
    scheduler: &Scheduler,
    chain: &[RoutingTarget],
    plan: Arc<Prepared>,
    position: usize,
) -> Result<crate::gateway::scheduling::queue::Waiting> {
    let ordinal = plan
        .order
        .get()
        .and_then(|o| o.get(position))
        .copied()
        .unwrap_or(position);
    Ok(scheduler.enqueue(
        Identity::Local,
        &chain[ordinal],
        Request {
            candidate: plan.candidates[ordinal],
            work: plan.work,
            shape: plan.shape,
            deadline: Instant::now() + Duration::from_secs(30),
            wait_until: Instant::now() + Duration::from_secs(20),
            retained_bytes: 4096,
            prefix: plan.prefixes.get(ordinal).cloned().flatten(),
            planning: plan.order.get().is_none().then_some(plan),
        },
    )?)
}

#[tokio::test(start_paused = true)]
async fn adaptive_queue_books_with_fresh_pressure_and_freezes_fallback_sequence() -> Result<()> {
    let (scheduler, chain) = fixture(Policy::LoadAware)?;
    let plan = prepare(&scheduler, &chain, false)?;
    let blocker = scheduler.book(
        plan.candidates[0],
        &chain[0],
        plan.work,
        plan.shape,
        plan.deadline,
    )?;
    let reservation = waiting(&scheduler, &chain, plan.clone(), 0)?
        .receive(CancellationToken::new())
        .await?;
    ensure!(reservation.observation.candidate.ordinal == 1);
    ensure!(plan.order.get().context("frozen")? == &[1, 0]);
    drop(blocker);
    reservation.observer().headers(429); // unsent headers cannot fake a rejection
    drop(reservation);
    let fallback = waiting(&scheduler, &chain, plan.clone(), 1)?
        .receive(CancellationToken::new())
        .await?;
    ensure!(
        fallback.observation.candidate.ordinal == 0
            && plan.order.get().context("order")? == &[1, 0]
    );
    drop(fallback);
    scheduler.shutdown().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn adaptive_prefix_prefers_warm_but_escapes_load_and_expires_cold() -> Result<()> {
    let (scheduler, chain) = fixture(Policy::PrefixAffinity)?;
    let warm = prepare(&scheduler, &chain, false)?;
    let binding = warm.prefixes[1].as_ref().context("projection")?.clone();
    {
        let mut state = scheduler.inner.lock();
        let index = state.prefix.as_mut().context("index")?;
        index.observe(
            100,
            &binding,
            crate::gateway::scheduling::prefix::Event::Sent,
            Instant::now(),
        );
        index.observe(
            100,
            &binding,
            crate::gateway::scheduling::prefix::Event::Succeeded,
            Instant::now(),
        );
    }
    let selected = waiting(&scheduler, &chain, warm, 0)?
        .receive(CancellationToken::new())
        .await?;
    ensure!(selected.observation.candidate.ordinal == 1);
    let escape = prepare(&scheduler, &chain, false)?;
    let next = waiting(&scheduler, &chain, escape, 0)?
        .receive(CancellationToken::new())
        .await?;
    ensure!(next.observation.candidate.ordinal == 0);
    drop(next);
    drop(selected);
    tokio::time::advance(Duration::from_secs(121)).await;
    let cold = prepare(&scheduler, &chain, false)?;
    let next = waiting(&scheduler, &chain, cold, 0)?
        .receive(CancellationToken::new())
        .await?;
    ensure!(next.observation.candidate.ordinal == 0);
    drop(next);
    scheduler.shutdown().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn adaptive_queued_choice_refreshes_and_rejects_foreign_generation() -> Result<()> {
    let (scheduler, chain) = fixture(Policy::LoadAware)?;
    let first = prepare(&scheduler, &chain, false)?;
    let second = prepare(&scheduler, &chain, false)?;
    let a = waiting(&scheduler, &chain, first, 0)?;
    let b = waiting(&scheduler, &chain, second, 0)?;
    let a = a.receive(CancellationToken::new()).await?;
    let b = b.receive(CancellationToken::new()).await?;
    ensure!(a.observation.candidate.ordinal == 0 && b.observation.candidate.ordinal == 1);
    let (foreign, _) = fixture(Policy::LoadAware)?;
    let stale = prepare(&scheduler, &chain, false)?;
    ensure!(waiting(&foreign, &chain, stale, 0).is_err());
    drop(a);
    drop(b);
    foreign.shutdown().await?;
    scheduler.shutdown().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn adaptive_lane_bypasses_same_tenant_pinned_saturation() -> Result<()> {
    let (scheduler, chain) = fixture(Policy::LoadAware)?;
    let plan = prepare(&scheduler, &chain, false)?;
    let mut held = Vec::new();
    for _ in 0..8 {
        held.push(scheduler.book(
            plan.candidates[0],
            &chain[0],
            plan.work,
            plan.shape,
            plan.deadline,
        )?);
    }
    let pinned = scheduler.enqueue(
        Identity::Local,
        &chain[0],
        Request {
            candidate: plan.candidates[0],
            work: plan.work,
            shape: plan.shape,
            deadline: plan.deadline,
            wait_until: plan.deadline,
            retained_bytes: 4096,
            prefix: None,
            planning: None,
        },
    )?;
    let adaptive = waiting(&scheduler, &chain, plan, 0)?
        .receive(CancellationToken::new())
        .await?;
    ensure!(adaptive.observation.candidate.ordinal == 1);
    ensure!(scheduler.inspect_queue().queued == 1);
    drop(pinned);
    drop(adaptive);
    drop(held);
    scheduler.shutdown().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn adaptive_latency_uses_matching_mode_bucket_and_fresh_successes_only() -> Result<()> {
    let (scheduler, chain) = fixture(Policy::LoadAware)?;
    let plain = prepare(&scheduler, &chain, false)?;
    {
        let mut state = scheduler.inner.lock();
        for (index, destination) in state.destinations.iter_mut().enumerate() {
            for _ in 0..3 {
                destination
                    .samples
                    .push_back(crate::gateway::scheduling::observation::Sample {
                        shape: plain.shape,
                        at: Instant::now(),
                        millis: if index == 0 { 50 } else { 10 },
                    });
            }
        }
        state.entries += 6;
        state.bytes += 6 * crate::gateway::scheduling::observation::SAMPLE_BYTES;
    }
    let chosen = waiting(&scheduler, &chain, plain, 0)?
        .receive(CancellationToken::new())
        .await?;
    ensure!(chosen.observation.candidate.ordinal == 1);
    drop(chosen);
    let stream = prepare(&scheduler, &chain, true)?;
    let chosen = waiting(&scheduler, &chain, stream, 0)?
        .receive(CancellationToken::new())
        .await?;
    ensure!(chosen.observation.candidate.ordinal == 0);
    drop(chosen);
    let mut small = prompt(false)?;
    small.params.max_tokens = Some(1024);
    let mut ctx =
        PipelineContext::new(PipelineRequest::new("model", CallerContext::local(), small));
    scheduler.prepare(&chain, &mut ctx, vec![true; chain.len()])?;
    let small = ctx.extension::<Prepared>().context("small")?;
    let chosen = waiting(&scheduler, &chain, small, 0)?
        .receive(CancellationToken::new())
        .await?;
    ensure!(chosen.observation.candidate.ordinal == 0);
    drop(chosen);
    tokio::time::advance(Duration::from_secs(301)).await;
    let stale = prepare(&scheduler, &chain, false)?;
    let chosen = waiting(&scheduler, &chain, stale, 0)?
        .receive(CancellationToken::new())
        .await?;
    ensure!(chosen.observation.candidate.ordinal == 0);
    drop(chosen);
    scheduler.shutdown().await?;
    Ok(())
}
