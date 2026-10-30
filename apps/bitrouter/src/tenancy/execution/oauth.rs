//! Exact retained OAuth consumer gates; no public caller can construct these proofs.
use super::{TenantExecution, authorize_route};
use crate::gateway::pipeline::types::RoutingTarget;
use crate::tenancy::oauth::{
    self, ModelStamp,
    adapter::BoundOAuthStore,
    exchange::{Exchange, Job},
    storage::{Metadata, State, Work},
};
use bitrouter_ai::tenancy::operation::ErrorCode;
use std::sync::Arc;

impl TenantExecution {
    pub(crate) fn uses_oauth(&self, target: &RoutingTarget) -> Result<bool, ErrorCode> {
        self.oauth_profile(target).map(|profile| profile.is_some())
    }

    pub(in crate::tenancy) fn oauth_profile(
        &self,
        target: &RoutingTarget,
    ) -> Result<Option<Metadata>, ErrorCode> {
        let reference = self.route.match_candidate(target)?;
        self.service
            .with_dispatch_authority(&self.submission, |state| {
                authorize_route(state, &self.submission, self.route.dependencies())?;
                let profile = self.service.oauth.get(self.installation_id(), &reference)?;
                if let Some(metadata) = &profile {
                    self.route.require_oauth_messages(target)?;
                    if metadata.profile.provider != target.provider_name
                        || metadata.profile.account
                            != target.account_label.as_deref().unwrap_or("default")
                    {
                        return Err(ErrorCode::Forbidden);
                    }
                }
                Ok(profile)
            })
    }
    pub(crate) async fn oauth(
        self: &Arc<Self>,
        target: &RoutingTarget,
    ) -> Result<Option<Arc<BoundOAuthStore>>, ErrorCode> {
        let Some(metadata) = self.oauth_profile(target)? else {
            return Ok(None);
        };
        if !matches!(metadata.state, State::Ready) {
            return Err(ErrorCode::CapacityExceeded);
        }
        let _permit = self
            .service
            .read_permit()
            .map_err(|failure| failure.0.code)?;
        let row = tokio::time::timeout_at(
            self.submission
                .deadline
                .min(tokio::time::Instant::now() + std::time::Duration::from_secs(5)),
            oauth::storage::load(
                &self.service.db,
                self.installation_id(),
                &metadata.reference,
            ),
        )
        .await
        .map_err(|_| ErrorCode::CapacityExceeded)?
        .map_err(|_| ErrorCode::AuthorityUnavailable)?
        .ok_or(ErrorCode::AuthorityUnavailable)?;
        #[cfg(test)]
        self.service
            .receipt_credential_read_for_test(self.id())
            .await;
        row.token(&self.oauth_crypto()?)
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        if self.oauth_profile(target)?.as_ref() != Some(&row.metadata) {
            return Err(ErrorCode::Forbidden);
        }
        Ok(Some(BoundOAuthStore::new(Exchange::new(
            self.clone(),
            target.clone(),
            row,
        )?)))
    }
    pub(in crate::tenancy) fn oauth_crypto(&self) -> Result<oauth::crypto::Crypto, ErrorCode> {
        self.service
            .resources
            .oauth_crypto()
            .map_err(|_| ErrorCode::AuthorityUnavailable)
    }
    pub(in crate::tenancy) fn oauth_work(
        &self,
        id: &str,
        generation: u64,
    ) -> Result<Work, ErrorCode> {
        Ok(Work {
            id: id.to_owned(),
            execution: self.id.clone(),
            tenant: self.submission.tenant.clone(),
            principal: self.submission.principal.clone(),
            expected_generation: generation,
            started_at: chrono::Utc::now()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                .try_into()
                .map_err(|_| ErrorCode::AuthorityUnavailable)?,
        })
    }
    pub(in crate::tenancy) fn enqueue_oauth(&self, job: Job) -> Result<(), ErrorCode> {
        self.service
            .completions
            .try_send(crate::tenancy::actor::Job::OAuth(Box::new(job)))
            .map_err(|_| ErrorCode::CapacityExceeded)
    }
    pub(in crate::tenancy) fn arm_oauth(
        &self,
        exchange: &Exchange,
        request: &reqwest::Request,
    ) -> Result<(), ErrorCode> {
        self.service
            .with_dispatch_authority(&self.submission, |state| {
                authorize_route(state, &self.submission, self.route.dependencies())?;
                self.route.require_oauth_messages(&exchange.target)?;
                let metadata = self
                    .service
                    .oauth
                    .get(
                        self.installation_id(),
                        &exchange.original.metadata.reference,
                    )?
                    .ok_or(ErrorCode::AuthorityUnavailable)?;
                if exchange.execution.id() != self.id()
                    || exchange.start.get().map(|row| &row.metadata) != Some(&metadata)
                    || !matches!(metadata.state, State::RefreshStarted(_))
                    || request.method() != reqwest::Method::POST
                    || request.url().as_str() != metadata.profile.token_endpoint
                {
                    return Err(ErrorCode::Forbidden);
                }
                exchange.arm(oauth::refresh::request_digest(request)?)
            })
    }
    pub(in crate::tenancy) fn validate_oauth_model(
        &self,
        target: &RoutingTarget,
        stamp: Option<&ModelStamp>,
    ) -> Result<(), ErrorCode> {
        let reference = self.route.match_candidate(target)?;
        let metadata = self.service.oauth.get(self.installation_id(), &reference)?;
        match (metadata, stamp) {
            (None, None) => Ok(()),
            (Some(metadata), Some(stamp)) if stamp.matches(self, &metadata) => {
                self.route.require_oauth_messages(target)
            }
            _ => Err(ErrorCode::Forbidden),
        }
    }
}
