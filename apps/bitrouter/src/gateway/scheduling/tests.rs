use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::types::{Prompt, StreamPart};
use tokio::{
    sync::Barrier,
    time::{Duration, Instant},
};

use super::{Scheduler, observation::Shape};
use crate::{
    config::{
        Config,
        inference_routing::{DestinationConfig, InferenceRoutingConfig},
        routing_table::resolve_route_chain,
    },
    gateway::pipeline::{routing::RoutingPrefs, types::RoutingTarget},
};

mod execution;
mod payload;
mod policy;
mod queue;
mod reload;

#[tokio::test(start_paused = true)]
async fn deadline_reaper_prevents_late_send_and_bounds_retained_owner_slots() -> Result<()> {
    let target = target("https://fixture.invalid")?;
    let mut config = settings(&target.api_base, 1);
    config.limits.max_state_entries = 3;
    let scheduler = Scheduler::new(&config, 1)?;
    let candidate = scheduler.candidate(&target, 0)?;
    let (shape, work) = Shape::from_prompt(&prompt(false)?)?;
    let reservation = scheduler.book(
        candidate,
        &target,
        work,
        shape,
        Instant::now() + Duration::from_millis(5),
    )?;
    let observer = reservation.observer();
    tokio::time::advance(Duration::from_millis(6)).await;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    ensure!(observer.begin_send(&target).is_err());
    ensure!(scheduler.inspect(shape)[0].active == 0);
    ensure!(
        scheduler
            .book(
                candidate,
                &target,
                work,
                shape,
                Instant::now() + Duration::from_secs(5)
            )
            .is_err()
    );
    drop(reservation);
    drop(scheduler.book(
        candidate,
        &target,
        work,
        shape,
        Instant::now() + Duration::from_secs(5),
    )?);
    scheduler.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn global_and_destination_bounds_are_independent_across_pools() -> Result<()> {
    for destination_limited in [false, true] {
        let first = Arc::new(target("https://fixture.invalid")?);
        let mut other = (*first).clone();
        other.provider_name = "backup".into();
        let other = Arc::new(other);
        let mut config = settings(&first.api_base, if destination_limited { 4 } else { 2 });
        let capacity = if destination_limited { 1 } else { 2 };
        config.destinations[0].max_active = capacity;
        let mut second = config.destinations[0].clone();
        second.id = "backup".into();
        second.provider = "backup".into();
        config.destinations.push(second);
        let scheduler = Scheduler::new(&config, 1)?;
        let (shape, work) = Shape::from_prompt(&prompt(false)?)?;
        let start = Arc::new(Barrier::new(17));
        let booked = Arc::new(Barrier::new(17));
        let release = Arc::new(Barrier::new(17));
        let mut tasks = Vec::new();
        for index in 0..16 {
            let scheduler = scheduler.clone();
            let target = if index % 2 == 0 {
                first.clone()
            } else {
                other.clone()
            };
            let candidate = scheduler.candidate(&target, index)?;
            let (start, booked, release) = (start.clone(), booked.clone(), release.clone());
            tasks.push(tokio::spawn(async move {
                start.wait().await;
                let reservation = scheduler.book(
                    candidate,
                    &target,
                    work,
                    shape,
                    Instant::now() + Duration::from_secs(5),
                );
                booked.wait().await;
                release.wait().await;
                reservation.is_ok()
            }));
        }
        start.wait().await;
        booked.wait().await;
        let view = scheduler.inspect(shape);
        ensure!(view.iter().map(|d| d.active).sum::<u32>() == 2);
        if destination_limited {
            ensure!(view.iter().all(|d| d.active == 1));
        }
        release.wait().await;
        for task in tasks {
            task.await?;
        }
        scheduler.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn changed_destination_or_protocol_cannot_reuse_a_frozen_handle() -> Result<()> {
    let mut target = target("https://fixture.invalid")?;
    let scheduler = Scheduler::new(&settings(&target.api_base, 1), 1)?;
    let candidate = scheduler.candidate(&target, 0)?;
    let (shape, work) = Shape::from_prompt(&prompt(false)?)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    target.service_id = "different-model".into();
    ensure!(
        scheduler
            .book(candidate, &target, work, shape, deadline)
            .is_err()
    );
    target.service_id = "model".into();
    target.api_protocol = bitrouter_ai::types::ApiProtocol::Messages;
    ensure!(
        scheduler
            .book(candidate, &target, work, shape, deadline)
            .is_err()
    );
    target.api_protocol = bitrouter_ai::types::ApiProtocol::ChatCompletions;
    let reservation = scheduler.book(candidate, &target, work, shape, deadline)?;
    target.api_base = "https://different.invalid".into();
    ensure!(reservation.observer().begin_send(&target).is_err());
    drop(reservation);
    ensure!(scheduler.inspect(shape)[0].work == 0);
    scheduler.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn late_rejection_headers_release_an_unknown_attempt_once() -> Result<()> {
    let target = target("https://fixture.invalid")?;
    let scheduler = Scheduler::new(&settings(&target.api_base, 1), 1)?;
    let (shape, work) = Shape::from_prompt(&prompt(false)?)?;
    let reservation = scheduler.book(
        scheduler.candidate(&target, 0)?,
        &target,
        work,
        shape,
        Instant::now() + Duration::from_secs(2),
    )?;
    let observer = reservation.observer();
    observer.begin_send(&target)?;
    drop(reservation);
    observer.headers(429);
    observer.headers(429);
    let view = scheduler.inspect(shape);
    ensure!(view[0].uncertain == 0 && view[0].work == 0 && view[0].counters.rejected == 1);
    ensure!(!observer.blocks_fallback()); // Permission still belongs to gateway policy and receipts.
    scheduler.shutdown().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn only_first_nonempty_semantic_output_measures_stream_latency() -> Result<()> {
    let target = target("https://fixture.invalid")?;
    let scheduler = Scheduler::new(&settings(&target.api_base, 1), 1)?;
    let (shape, work) = Shape::from_prompt(&prompt(true)?)?;
    let candidate = scheduler.candidate(&target, 0)?;
    for part in [
        StreamPart::ReasoningDelta {
            text: "thought".into(),
            source_kind: None,
        },
        StreamPart::ToolCallDelta {
            id: "call".into(),
            name: Some("tool".into()),
            arguments: String::new(),
            provider_metadata: Default::default(),
        },
        StreamPart::ToolCallDelta {
            id: "call".into(),
            name: None,
            arguments: "{}".into(),
            provider_metadata: Default::default(),
        },
    ] {
        let reservation = scheduler.book(
            candidate,
            &target,
            work,
            shape,
            Instant::now() + Duration::from_secs(5),
        )?;
        let observer = reservation.observer();
        observer.begin_send(&target)?;
        observer.headers(200);
        observer.part(&StreamPart::TextStart {
            id: "role-only".into(),
        });
        observer.part(&StreamPart::TextDelta {
            text: String::new(),
        });
        observer.part(&StreamPart::ReasoningEnd {
            id: "opaque".into(),
            signature: Some("opaque-signature".into()),
            native: None,
        });
        observer.part(&StreamPart::Usage {
            usage: Default::default(),
        });
        ensure!(
            scheduler
                .inner
                .lock()
                .records
                .values()
                .all(|record| record.first_output.is_none())
        );
        tokio::time::advance(Duration::from_millis(7)).await;
        observer.part(&part);
        tokio::time::advance(Duration::from_millis(11)).await;
        observer.part(&part);
        observer.completed(true);
        drop(reservation);
    }
    ensure!(scheduler.inspect(shape)[0].mean_success_millis == Some(7));
    scheduler.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn entry_and_byte_exhaustion_never_evict_live_reservations() -> Result<()> {
    let target = target("https://fixture.invalid")?;
    let (shape, work) = Shape::from_prompt(&prompt(false)?)?;
    for bytes in [false, true] {
        let mut config = settings(&target.api_base, 4);
        if bytes {
            let baseline = Scheduler::new(&config, 1)?;
            config.limits.max_state_bytes = u32::try_from(
                baseline.inner.lock().bytes + super::ATTEMPT_BYTES + super::OWNER_BYTES,
            )?;
            baseline.shutdown().await?;
        } else {
            config.limits.max_state_entries = 3;
        }
        let scheduler = Scheduler::new(&config, 1)?;
        let candidate = scheduler.candidate(&target, 0)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let reservation = scheduler.book(candidate, &target, work, shape, deadline)?;
        ensure!(
            scheduler
                .book(candidate, &target, work, shape, deadline)
                .is_err()
        );
        ensure!(
            scheduler.inspect(shape)[0].active == 1 && scheduler.inspect(shape)[0].work == work
        );
        drop(reservation);
        drop(scheduler.book(candidate, &target, work, shape, deadline)?);
        scheduler.shutdown().await?;
    }
    Ok(())
}

pub(super) fn prompt(stream: bool) -> Result<Prompt> {
    Ok(serde_json::from_value(
        serde_json::json!({"model":"model", "messages":[], "stream":stream}),
    )?)
}

pub(super) fn target(endpoint: &str) -> Result<RoutingTarget> {
    let config: Config = serde_json::from_value(serde_json::json!({
        "inherit_defaults":false, "registry":{"enabled":false}, "providers":{"fixture":{
            "api_base":endpoint, "api_key":"test-key", "models":[{"id":"model"}],
            "api_protocol":[{"*":"chat_completions"}]
        }}
    }))?;
    resolve_route_chain(&config, "fixture:model", &RoutingPrefs::default())?
        .into_iter()
        .next()
        .context("missing fixture target")
}

pub(super) fn settings(endpoint: &str, capacity: u32) -> InferenceRoutingConfig {
    let mut config = InferenceRoutingConfig::default();
    config.limits.max_active = capacity;
    config.limits.max_uncertain = capacity;
    config.limits.uncertainty_ttl_ms = 100;
    config.destinations.push(DestinationConfig {
        id: "fixture".into(),
        provider: "fixture".into(),
        model: "model".into(),
        endpoint: endpoint.into(),
        account: None,
        max_active: capacity,
    });
    config
}

#[tokio::test]
async fn concurrent_bookings_observe_all_assigned_work() -> Result<()> {
    let target = Arc::new(target("https://fixture.invalid")?);
    let scheduler = Scheduler::new(&settings(&target.api_base, 2), 1)?;
    let (shape, work) = Shape::from_prompt(&prompt(false)?)?;
    let candidate = scheduler.candidate(&target, 0)?;
    let start = Arc::new(Barrier::new(17));
    let booked = Arc::new(Barrier::new(17));
    let release = Arc::new(Barrier::new(17));
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let (scheduler, target, start, booked, release) = (
            scheduler.clone(),
            target.clone(),
            start.clone(),
            booked.clone(),
            release.clone(),
        );
        tasks.push(tokio::spawn(async move {
            start.wait().await;
            let reservation = scheduler.book(
                candidate,
                &target,
                work,
                shape,
                Instant::now() + Duration::from_secs(5),
            );
            booked.wait().await;
            release.wait().await;
            reservation.is_ok()
        }));
    }
    start.wait().await;
    booked.wait().await;
    let view = scheduler.inspect(shape);
    ensure!(view[0].active == 2 && view[0].work == 2 * work && view[0].uncertain == 0);
    release.wait().await;
    let mut accepted = 0;
    for task in tasks {
        accepted += usize::from(task.await?);
    }
    ensure!(accepted == 2 && scheduler.inspect(shape)[0].work == 0);
    scheduler.shutdown().await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn uncertainty_credit_expiry_and_late_terminal_never_double_release() -> Result<()> {
    let target = target("https://fixture.invalid")?;
    let mut config = settings(&target.api_base, 4);
    config.limits.max_uncertain = 1;
    let scheduler = Scheduler::new(&config, 1)?;
    let (shape, work) = Shape::from_prompt(&prompt(true)?)?;
    let candidate = scheduler.candidate(&target, 0)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let reservation = scheduler.book(candidate, &target, work, shape, deadline)?;
    let observer = reservation.observer();
    observer.begin_send(&target)?;
    observer.headers(200);
    observer.part(&StreamPart::TextDelta {
        text: "ready".into(),
    });
    ensure!(scheduler.inspect(shape)[0].active == 1);
    drop(reservation);
    let before = scheduler.inspect(shape);
    ensure!(before[0].active == 0 && before[0].uncertain == 1 && before[0].work == work);
    ensure!(
        scheduler
            .book(candidate, &target, work, shape, deadline)
            .is_err()
    );
    // An inspection cannot manufacture completion, reclaim storage or renew TTL.
    let charged = {
        let state = scheduler.inner.lock();
        (state.entries, state.bytes, state.next)
    };
    for _ in 0..8 {
        ensure!(scheduler.inspect(shape)[0].uncertain == 1);
    }
    ensure!(
        charged == {
            let state = scheduler.inner.lock();
            (state.entries, state.bytes, state.next)
        }
    );
    tokio::time::advance(Duration::from_millis(101)).await;
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    ensure!(scheduler.inspect(shape)[0].work == 0 && observer.blocks_fallback());
    let reservation = scheduler.book(candidate, &target, work, shape, deadline)?;
    observer.completed(true); // Expired evidence cannot close the new attempt.
    ensure!(scheduler.inspect(shape)[0].active == 1 && scheduler.inspect(shape)[0].work == work);
    drop(reservation);
    ensure!(scheduler.inspect(shape)[0].work == 0);
    ensure!(scheduler.inspect(shape)[0].counters.expired == 1);
    scheduler.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn late_known_completion_closes_uncertainty_once() -> Result<()> {
    let target = target("https://fixture.invalid")?;
    let scheduler = Scheduler::new(&settings(&target.api_base, 1), 1)?;
    let (shape, work) = Shape::from_prompt(&prompt(false)?)?;
    let reservation = scheduler.book(
        scheduler.candidate(&target, 0)?,
        &target,
        work,
        shape,
        Instant::now() + Duration::from_secs(2),
    )?;
    let observer = reservation.observer();
    observer.begin_send(&target)?;
    observer.headers(200);
    drop(reservation);
    observer.completed(true);
    observer.completed(true);
    let view = scheduler.inspect(shape);
    ensure!(
        view[0].work == 0
            && view[0].uncertain == 0
            && view[0].counters.completed == 1
            && view[0].counters.unknown == 1
    );
    ensure!(observer.blocks_fallback());
    scheduler.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn identities_budgets_shutdown_and_restarts_preserve_custody() -> Result<()> {
    let target = target("https://fixture.invalid")?;
    let config = settings(&target.api_base, 1);
    let scheduler = Scheduler::new(&config, 1)?;
    let next = Scheduler::new(&config, 2)?;
    let candidate = scheduler.candidate(&target, 0)?;
    let (shape, work) = Shape::from_prompt(&prompt(false)?)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    ensure!(
        next.book(candidate, &target, work, shape, deadline)
            .is_err()
    );
    ensure!(
        scheduler
            .book(candidate, &target.clone(), work, shape, deadline)
            .is_err()
    );
    let reservation = scheduler.book(candidate, &target, work, shape, deadline)?;
    let observer = reservation.observer();
    scheduler.close();
    ensure!(observer.begin_send(&target).is_err());
    ensure!(scheduler.shutdown().await.is_err());
    drop(reservation);
    scheduler.shutdown().await?;
    ensure!(scheduler.inspect(shape)[0].counters.unsent == 1);
    next.shutdown().await?;
    let mut too_small = config.clone();
    too_small.limits.max_state_entries = 1;
    ensure!(Scheduler::new(&too_small, 1).is_err());
    too_small.limits.max_state_entries = 2;
    too_small.limits.max_state_bytes = 1;
    ensure!(Scheduler::new(&too_small, 1).is_err());
    ensure!(Scheduler::from_config(&config)?.is_none());
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn samples_are_success_only_shape_scoped_fresh_and_bounded() -> Result<()> {
    let target = target("https://fixture.invalid")?;
    let scheduler = Scheduler::new(&settings(&target.api_base, 1), 1)?;
    let (shape, work) = Shape::from_prompt(&prompt(false)?)?;
    let (stream_shape, _) = Shape::from_prompt(&prompt(true)?)?;
    let candidate = scheduler.candidate(&target, 0)?;
    for count in 0..40 {
        let reservation = scheduler.book(
            candidate,
            &target,
            work,
            shape,
            Instant::now() + Duration::from_secs(10),
        )?;
        let observer = reservation.observer();
        observer.begin_send(&target)?;
        observer.headers(200);
        tokio::time::advance(Duration::from_millis(25)).await;
        observer.completed(true);
        drop(reservation);
        ensure!(
            scheduler.inspect(shape)[0].mean_success_millis
                == if count >= 2 { Some(25) } else { None }
        );
    }
    ensure!(scheduler.inner.lock().destinations[0].samples.len() == 32);
    ensure!(
        scheduler.inspect(stream_shape)[0]
            .mean_success_millis
            .is_none()
    );
    let reservation = scheduler.book(
        candidate,
        &target,
        work,
        shape,
        Instant::now() + Duration::from_secs(10),
    )?;
    reservation.observer().begin_send(&target)?;
    reservation.observer().headers(429);
    drop(reservation);
    ensure!(scheduler.inner.lock().destinations[0].samples.len() == 32);
    tokio::time::advance(Duration::from_secs(301)).await;
    ensure!(scheduler.inspect(shape)[0].mean_success_millis.is_none());
    scheduler.shutdown().await?;
    Ok(())
}
