//! Authority-to-runtime linearization for an actual selected executor entry.

use std::{sync::Arc, time::Instant};

use bitrouter_ai::tenancy::{admission::AdmissionMode, operation::ErrorCode};

use super::{TenantExecution, authorize_effect, authorize_route};
use crate::{
    gateway::{error::BitrouterError, pipeline::types::RoutingTarget},
    tenancy::operational::{Hop, state::Denial},
};

impl TenantExecution {
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    pub(crate) fn observation_scope(
        &self,
    ) -> (
        &bitrouter_ai::tenancy::identity::InstallationId,
        &bitrouter_ai::tenancy::identity::TenantId,
        &bitrouter_ai::tenancy::identity::RevisionId,
    ) {
        (
            self.installation_id(),
            &self.submission.tenant,
            &self.submission.binding.execution_revision,
        )
    }

    pub(crate) fn profiled(&self) -> bool {
        self.route.admission_mode() == AdmissionMode::ProfiledV1
    }

    pub(crate) fn reached_hop(
        &self,
        rank: usize,
        target: &RoutingTarget,
    ) -> Result<(), BitrouterError> {
        if !self.profiled() {
            let ordinal = self
                .route
                .ordered_ordinals()
                .and_then(|ordinals| ordinals.get(rank))
                .copied();
            if let Ok(mut active) = self.hop.lock() {
                *active = Some(Arc::new(Hop::new(
                    ordinal,
                    rank,
                    None,
                    bitrouter_ai::tenancy::admission::FallbackRule::Stop,
                    Instant::now(),
                )));
            }
            return Ok(());
        }
        self.revalidate()
            .map_err(|code| self.reject_reached(rank, target, code))?;
        let refusal =
            self.service
                .with_dispatch_authority(&self.submission, |state| {
                    authorize_effect(state, &self.submission, &self.route)?;
                    let (ordinal, pinned) = self.route.reached_profile(rank, target)?;
                    let mut current = pinned.clone();
                    current.revision_id = state
                        .resource_lifecycle
                        .head(&pinned.key())
                        .ok_or(ErrorCode::Forbidden)?
                        .clone();
                    authorize_route(state, &self.submission, std::slice::from_ref(&current))?;
                    let rules = state
                        .resource_admission
                        .profile(&current)
                        .ok_or(ErrorCode::Forbidden)?;
                    let mut active = self
                        .hop
                        .lock()
                        .map_err(|_| ErrorCode::AuthorityUnavailable)?;
                    if active.as_ref().is_some_and(|previous| {
                        previous.rank >= rank || !previous.permits_fallback()
                    }) || self.receipt_blocks_fallback()
                    {
                        return Err(ErrorCode::Forbidden);
                    }
                    let now = Instant::now();
                    let hop = Arc::new(Hop::new(
                        Some(ordinal),
                        rank,
                        Some(pinned.clone()),
                        rules.fallback,
                        now,
                    ));
                    let admission =
                        self.service
                            .operational
                            .admit(self.installation_id(), &current, rules);
                    let denial = match admission {
                        Ok(ticket) => {
                            hop.admitted(ticket)?;
                            None
                        }
                        Err(denial) => {
                            hop.refused(denial);
                            Some(denial)
                        }
                    };
                    *active = Some(hop);
                    Ok(denial)
                })
                .map_err(|code| self.reject_reached(rank, target, code))?;
        match refusal {
            None => Ok(()),
            Some(Denial::Rate) => Err(BitrouterError::RateLimited { retry_after: None }),
            Some(Denial::Circuit) => Err(BitrouterError::UpstreamUnavailable),
            Some(Denial::Capacity) => Err(self.reject(ErrorCode::CapacityExceeded)),
            Some(Denial::Unavailable) => Err(self.reject(ErrorCode::AuthorityUnavailable)),
        }
    }

    pub(crate) fn current_hop(&self) -> Option<Arc<Hop>> {
        self.hop.lock().ok().and_then(|hop| hop.clone())
    }

    fn reject_reached(
        &self,
        rank: usize,
        target: &RoutingTarget,
        code: ErrorCode,
    ) -> BitrouterError {
        if let Ok((ordinal, profile)) = self.route.reached_profile(rank, target)
            && let Ok(mut active) = self.hop.lock()
            && active.as_ref().is_none_or(|previous| previous.rank != rank)
        {
            let hop = Arc::new(Hop::new(
                Some(ordinal),
                rank,
                Some(profile.clone()),
                bitrouter_ai::tenancy::admission::FallbackRule::Stop,
                Instant::now(),
            ));
            if self.submission.deadline.expired() {
                hop.budget_exhausted();
            } else if self.submission.cancelled.is_cancelled() {
                hop.dropped();
            } else {
                hop.refused(if code == ErrorCode::CapacityExceeded {
                    Denial::Capacity
                } else {
                    Denial::Unavailable
                });
            }
            *active = Some(hop);
        }
        self.reject(code)
    }

    pub(super) fn bound_hop(&self, target: &RoutingTarget) -> Result<Option<Arc<Hop>>, ErrorCode> {
        if !self.profiled() {
            return Ok(self.current_hop());
        }
        let hop = self
            .hop
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?
            .clone()
            .ok_or(ErrorCode::Forbidden)?;
        let (ordinal, profile) = self.route.reached_profile(hop.rank, target)?;
        if Some(ordinal) != hop.ordinal || hop.profile.as_ref() != Some(profile) {
            return Err(ErrorCode::Forbidden);
        }
        Ok(Some(hop))
    }

    pub(crate) fn actual_ordinal(&self, target: &RoutingTarget) -> Result<Option<u16>, ErrorCode> {
        if self.profiled() {
            Ok(self.bound_hop(target)?.ok_or(ErrorCode::Forbidden)?.ordinal)
        } else {
            self.route.candidate_ordinal(target)
        }
    }

    pub(crate) fn operational_fallback(&self) -> Option<bool> {
        self.profiled().then(|| {
            self.revalidate().is_ok()
                && self.current_hop().is_some_and(|hop| hop.permits_fallback())
        })
    }

    pub(crate) fn operational_failed(&self, error: &BitrouterError) {
        if let Some(hop) = self.current_hop() {
            if self.submission.deadline.expired() {
                hop.budget_exhausted();
                return;
            }
            hop.failed(
                error,
                self.submission.cancelled.is_cancelled()
                    || matches!(error, BitrouterError::Cancelled),
            );
        }
    }

    pub(crate) fn operational_transport_failure(&self, timeout: bool) {
        if let Some(hop) = self.current_hop() {
            if self.submission.deadline.expired() {
                hop.budget_exhausted();
            } else if self.submission.cancelled.is_cancelled() {
                hop.dropped();
            } else {
                hop.transport_failed(timeout);
            }
        }
    }
}
