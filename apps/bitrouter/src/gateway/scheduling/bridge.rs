//! Gateway-owned frozen membership and the single reservation/stream handoff.

use futures::Stream;
use std::future::Future;
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::time::{Instant, Sleep};

use super::{CandidateId, Reservation, Scheduler, observation::Shape};
use crate::gateway::{
    error::{BitrouterError, Result},
    pipeline::{
        context::PipelineContext, execution::Pipeline, executor::StreamPartStream,
        types::RoutingTarget,
    },
};

pub(crate) struct Prepared {
    pub(super) policy: crate::config::inference_routing::Policy,
    pub(super) candidates: Vec<CandidateId>,
    pub(super) prefixes: Vec<Option<super::prefix::Binding>>,
    pub(super) order: std::sync::OnceLock<Vec<usize>>,
    pub(super) adaptive: bool,
    pub(super) eligible: Vec<bool>,
    pub(super) shape: Shape,
    pub(super) work: u64,
    queue_bytes: u64,
    wait_until: std::sync::OnceLock<Instant>,
    pub(crate) deadline: Instant,
}

pub(crate) fn validate_root(ctx: &PipelineContext, native: bool) -> Result<()> {
    #[cfg(feature = "multi-instance")]
    if ctx
        .extension::<crate::gateway::shared::admitted::SharedAdmission>()
        .is_some()
    {
        return Err(BitrouterError::bad_request(
            "shared provider scheduling is unsupported",
        ));
    }
    let prompt = ctx.require_generation_prompt()?;
    if native
        || crate::gateway::pipeline::native_continuation::has_continuation(prompt)
        || ctx
            .extension::<crate::gateway::pipeline::native::NativeManagedRequest>()
            .is_some()
        || prompt.params.extra.contains_key("previous_response_id")
        || prompt
            .params
            .supplemental_extra
            .contains_key("previous_response_id")
    {
        return Err(BitrouterError::bad_request(
            "unsupported scheduled operation root",
        ));
    }
    Ok(())
}

impl Scheduler {
    pub(crate) fn prepare(
        &self,
        chain: &Vec<RoutingTarget>,
        ctx: &mut PipelineContext,
        eligible: Vec<bool>,
    ) -> Result<()> {
        if eligible.len() != chain.len() {
            return Err(super::unavailable());
        }
        ctx.enable_scheduling_accounting();
        let queue_bytes = ctx
            .prepare_scheduling_payload()?
            .checked_add(
                super::payload::targets(chain)?
                    .checked_mul(3)
                    .ok_or_else(super::unavailable)?,
            )
            .ok_or_else(super::unavailable)?;
        let prompt = ctx.require_generation_prompt()?;
        if chain.len() > 4096
            || crate::gateway::pipeline::native_continuation::has_continuation(prompt)
            || ctx
                .extension::<crate::gateway::pipeline::native::NativeManagedRequest>()
                .is_some()
            || prompt.params.extra.contains_key("previous_response_id")
            || prompt
                .params
                .supplemental_extra
                .contains_key("previous_response_id")
        {
            return Err(BitrouterError::bad_request(
                "unsupported scheduled operation root",
            ));
        }
        let (shape, work) = Shape::from_prompt(prompt)?;
        let deadline = ctx.scheduling_started_at() + self.request_timeout;
        let deadline =
            crate::gateway::pipeline::deadline::at(ctx).map_or(deadline, |at| at.min(deadline));
        let deadline = ctx
            .extension::<crate::tenancy::execution::TenantExecution>()
            .and_then(|execution| execution.scheduling_deadline())
            .map_or(deadline, |at| at.min(deadline));
        if Instant::now() >= deadline {
            return Err(BitrouterError::UpstreamTimeout);
        }
        let candidates = chain
            .iter()
            .enumerate()
            .map(|(ordinal, target)| self.candidate(target, ordinal))
            .collect::<Result<Vec<_>>>()?;
        // Inspection is diagnostic only: no booking, sampling or warming.
        if tracing::enabled!(tracing::Level::TRACE) {
            for view in self.inspect(shape) {
                tracing::trace!(destination = view.destination, active = view.active,
                    uncertain = view.uncertain, work = view.work,
                    mean_success_millis = view.mean_success_millis, counters = ?view.counters,
                    "local provider reservation state");
            }
        }
        let prefixes = self
            .projector
            .as_ref()
            .map_or_else(Vec::new, |projector| projector.prepare(chain, ctx));
        ctx.insert_accounted_extension(
            Arc::new(Prepared {
                policy: self.policy,
                prefixes,
                candidates,
                order: std::sync::OnceLock::new(),
                adaptive: ctx
                    .extension::<crate::tenancy::execution::TenantExecution>()
                    .is_none_or(|execution| !execution.route().is_ordered()),
                eligible,
                shape,
                work,
                deadline,
                wait_until: std::sync::OnceLock::new(),
                queue_bytes,
            }),
            |value| {
                Ok(super::prefix::prepared_bytes(&value.prefixes)
                    + (std::mem::size_of::<Prepared>()
                        + value.candidates.capacity() * std::mem::size_of::<CandidateId>()
                        + value.candidates.len() * std::mem::size_of::<usize>()
                        + value.eligible.capacity()
                        + 64) as u64)
            },
        );
        Ok(())
    }
}

impl Pipeline {
    pub(crate) fn scheduling_position(
        &self,
        ctx: &PipelineContext,
        position: usize,
    ) -> Result<(usize, bool)> {
        if ctx.extension::<Scheduler>().is_none() {
            return Ok((position, false));
        }
        let prepared = ctx.extension::<Prepared>().ok_or_else(super::unavailable)?;
        let ordinal = prepared
            .order
            .get()
            .and_then(|order| order.get(position))
            .copied()
            .unwrap_or(position);
        Ok((
            ordinal,
            prepared.adaptive
                && prepared.order.get().is_none()
                && prepared.eligible.iter().any(|&eligible| eligible),
        ))
    }
    pub(crate) async fn reserve_scheduled(
        &self,
        ctx: &PipelineContext,
        position: usize,
        chain: &[RoutingTarget],
        errors: &mut [BitrouterError],
    ) -> Result<(usize, Option<Reservation>)> {
        let Some(scheduler) = ctx.extension::<Scheduler>() else {
            return Ok((position, None));
        };
        let prepared = ctx
            .extension::<Prepared>()
            .ok_or_else(|| BitrouterError::internal("scheduling membership was not frozen"))?;
        let ordinal = prepared
            .order
            .get()
            .and_then(|order| order.get(position))
            .copied()
            .unwrap_or(position);
        let target = chain.get(ordinal).ok_or_else(super::unavailable)?;
        let candidate = *prepared
            .candidates
            .get(ordinal)
            .ok_or_else(|| BitrouterError::internal("scheduled ordinal missing"))?;
        let tenant = ctx.extension::<crate::tenancy::execution::TenantExecution>();
        let identity = match &tenant {
            Some(execution) => super::queue::Identity::Tenant(execution.scheduling_tenant()),
            None if !ctx.caller().is_local() && !ctx.caller().is_anonymous() => {
                super::queue::Identity::Tenant(ctx.caller().user_id())
            }
            None => super::queue::Identity::Local,
        };
        let cancellation = match &tenant {
            Some(execution) => execution.scheduling_cancellation()?,
            None => tokio_util::sync::CancellationToken::new(),
        };
        if cancellation.is_cancelled() {
            return Err(BitrouterError::Cancelled);
        }
        let accounted = ctx.scheduling_accounted_bytes()?;
        let error_bytes = super::payload::errors(errors)?;
        let retained = prepared
            .queue_bytes
            .checked_add(accounted)
            .and_then(|n| n.checked_add(error_bytes))
            .ok_or_else(super::saturated)?;
        if tracing::enabled!(tracing::Level::TRACE)
            && let Some(Some(binding)) = prepared.prefixes.get(ordinal)
        {
            let view = scheduler.inspect_prefix(binding, None);
            tracing::trace!(
                ?view,
                "private approximate prefix evidence; no digest export"
            );
        }
        let waiting = scheduler.enqueue(
            identity,
            target,
            super::queue::Request {
                candidate,
                work: prepared.work,
                shape: prepared.shape,
                deadline: prepared.deadline,
                wait_until: *prepared.wait_until.get_or_init(|| {
                    prepared
                        .deadline
                        .min(Instant::now() + scheduler.queue_timeout)
                }),
                retained_bytes: retained,
                prefix: prepared.prefixes.get(ordinal).cloned().flatten(),
                planning: (prepared.adaptive
                    && prepared.order.get().is_none()
                    && prepared.eligible.iter().any(|&eligible| eligible))
                .then(|| prepared.clone()),
            },
        )?;
        if tracing::enabled!(tracing::Level::TRACE) {
            let view = scheduler.inspect_queue();
            tracing::trace!(queued = view.queued, bytes = view.bytes, flows = view.flows, lanes = view.lanes,
                waiting_capacity = view.waiting_capacity, waiting_credit = view.waiting_credit, totals = ?view.totals, rounds = view.rounds, "local scheduling queue");
        }
        let reservation = waiting.receive(cancellation).await?;
        let ordinal = reservation.observer().candidate.ordinal;
        if !reservation
            .observer()
            .candidate
            .matches(chain.get(ordinal).ok_or_else(super::unavailable)?)
        {
            return Err(super::unavailable());
        }
        ctx.set_scheduling_attempt(reservation.observer());
        Ok((ordinal, Some(reservation)))
    }
}

pub(crate) fn finish_attempt<T>(reservation: Option<Reservation>, outcome: &Result<T>) {
    if let (Some(reservation), Err(error)) = (&reservation, outcome) {
        reservation.failed(error);
    }
    drop(reservation);
}

/// The reservation stays with the returned producer, including an unpolled
/// stream. A terminal part alone cannot release it; the executor validates EOF.
pub(crate) fn bind_stream(
    stream: StreamPartStream,
    reservation: Option<Reservation>,
) -> StreamPartStream {
    match reservation {
        Some(reservation) => Box::pin(ReservedStream {
            deadline: Box::pin(tokio::time::sleep_until(reservation.deadline())),
            reservation: Some(reservation),
            inner: Some(stream),
        }),
        None => stream,
    }
}

struct ReservedStream {
    inner: Option<StreamPartStream>,
    reservation: Option<Reservation>,
    deadline: Pin<Box<Sleep>>,
}

impl Stream for ReservedStream {
    type Item = Result<bitrouter_ai::types::StreamPart>;
    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let Some(inner) = this.inner.as_mut() else {
            return Poll::Ready(None);
        };
        if this.deadline.as_mut().poll(cx).is_ready() {
            this.inner.take();
            if let Some(reservation) = this.reservation.take() {
                reservation.failed(&BitrouterError::UpstreamTimeout);
            }
            return Poll::Ready(Some(Err(BitrouterError::UpstreamTimeout)));
        }
        let result = inner.as_mut().poll_next(cx);
        if let Poll::Ready(Some(Err(error))) = &result
            && let Some(reservation) = this.reservation.take()
        {
            reservation.failed(error);
        }
        if matches!(result, Poll::Ready(None) | Poll::Ready(Some(Err(_)))) {
            this.inner.take();
            this.reservation.take();
        }
        result
    }
}
