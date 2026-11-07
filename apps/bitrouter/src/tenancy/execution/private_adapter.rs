//! Attempt handoff to an explicitly paired operator destination.

use std::sync::Arc;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bitrouter_ai::tenancy::{
    configuration::ResourceRef,
    identity::AttemptId,
    operation::ErrorCode,
    private_adapter::{
        ATTEMPT_DOMAIN, ATTEMPT_HEADER, Attempt, Disposition, MAX_ATTEMPT_LIFETIME_MS,
        MAX_HEADER_BYTES, OUTCOME_DOMAIN, OUTCOME_HEADER, Outcome,
    },
};
use hmac::{Hmac, KeyInit, Mac};
use reqwest::{Method, Request, Response, Url};
use sha2::{Digest, Sha256};

use super::TenantExecution;
use crate::{
    config::tenancy::PrivateAdapterConfig,
    gateway::{
        error::BitrouterError,
        model::client::{HttpTimeouts, ModelClient},
        pipeline::types::RoutingTarget,
    },
};

pub(in crate::tenancy) struct Peer {
    audience: String,
    endpoints: Vec<Url>,
    key: [u8; 32],
    client: reqwest::Client,
}

impl Peer {
    #[cfg(test)]
    pub(in crate::tenancy) fn for_test(endpoint: Url, key: [u8; 32]) -> anyhow::Result<Self> {
        Self::new(
            "fixture-peer".into(),
            vec![endpoint],
            key,
            HttpTimeouts::default().read,
        )
    }

    #[cfg(all(test, unix, feature = "http2"))]
    pub(in crate::tenancy) fn for_test_h2(endpoint: Url, key: [u8; 32]) -> anyhow::Result<Self> {
        let mut peer = Self::for_test(endpoint, key)?;
        peer.client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .http2_prior_knowledge()
            .build()?;
        Ok(peer)
    }

    pub(in crate::tenancy) fn load(
        config: &PrivateAdapterConfig,
        read_timeout: std::time::Duration,
    ) -> anyhow::Result<Self> {
        let endpoints = config.checked_endpoints()?;
        let encoded = std::env::var(&config.key_env)
            .map_err(|_| anyhow::anyhow!("tenancy private adapter peer key is unavailable"))?;
        let invalid =
            || anyhow::anyhow!("tenancy private adapter peer key must be 32 base64url bytes");
        let key = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| invalid())?;
        Self::new(
            config.audience.clone(),
            endpoints,
            key.try_into().map_err(|_| invalid())?,
            read_timeout,
        )
    }

    fn new(
        audience: String,
        endpoints: Vec<Url>,
        key: [u8; 32],
        read_timeout: std::time::Duration,
    ) -> anyhow::Result<Self> {
        let timeouts = HttpTimeouts::default();
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(timeouts.connect)
            .read_timeout(read_timeout)
            .pool_idle_timeout(timeouts.pool_idle)
            .tcp_keepalive(timeouts.tcp_keepalive)
            .build()?;
        Ok(Self {
            audience,
            endpoints,
            key,
            client,
        })
    }

    fn attempt_mac(&self, payload: &[u8]) -> Result<[u8; 32], ErrorCode> {
        let mut mac = self.mac(ATTEMPT_DOMAIN)?;
        mac.update(payload);
        Ok(mac.finalize().into_bytes().into())
    }

    fn mac(&self, domain: &[u8]) -> Result<Hmac<Sha256>, ErrorCode> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.key)
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        mac.update(domain);
        Ok(mac)
    }
}

pub(crate) struct Exchange {
    peer: Arc<Peer>,
    execution: Arc<TenantExecution>,
    hop: Arc<crate::tenancy::operational::Hop>,
    attempt_mac: [u8; 32],
    stream: bool,
}

impl Exchange {
    fn failed(&self) -> BitrouterError {
        self.hop.adapter_incomplete(false);
        self.execution.reject(ErrorCode::AuthorityUnavailable)
    }

    pub(crate) async fn send(
        &self,
        request: Request,
        cancelled: &tokio_util::sync::CancellationToken,
    ) -> Result<Response, BitrouterError> {
        let response = ModelClient::send_with(&self.peer.client, request, cancelled)
            .await
            .map_err(|_| self.failed())?;
        self.check(&response)?;
        Ok(response)
    }

    /// Check before the generic executor can record a provider status or permit
    /// fallback. An unsigned HTTP rejection is an ambiguous attempt.
    fn check(&self, response: &Response) -> Result<(), BitrouterError> {
        let status = response.status();
        let outcome = self
            .verify(status, response.headers())
            .map_err(|()| self.failed())?;
        match outcome.disposition {
            Disposition::Completed if status.is_success() && !self.stream => Ok(()),
            Disposition::Streaming if status.is_success() && self.stream => Ok(()),
            Disposition::ProviderRejected
                if status.is_client_error() || status.is_server_error() =>
            {
                Ok(())
            }
            Disposition::Refused if status.is_client_error() || status.is_server_error() => {
                self.hop.adapter_incomplete(true);
                Err(self.execution.reject(ErrorCode::AuthorityUnavailable))
            }
            _ => Err(self.failed()),
        }
    }

    fn verify(
        &self,
        status: reqwest::StatusCode,
        headers: &reqwest::header::HeaderMap,
    ) -> Result<Outcome, ()> {
        let wire = single_header(headers, OUTCOME_HEADER)?.ok_or(())?;
        if wire.len() > MAX_HEADER_BYTES {
            return Err(());
        }
        let (payload, tag) = wire.split_once('.').ok_or(())?;
        let payload = URL_SAFE_NO_PAD.decode(payload).map_err(|_| ())?;
        let tag = URL_SAFE_NO_PAD.decode(tag).map_err(|_| ())?;
        let mut mac = self.peer.mac(OUTCOME_DOMAIN).map_err(|_| ())?;
        mac.update(&self.attempt_mac);
        mac.update(&payload);
        mac.verify_slice(&tag).map_err(|_| ())?;
        let outcome: Outcome = serde_json::from_slice(&payload).map_err(|_| ())?;
        let retry_after = single_header(headers, "retry-after")?
            .map(|value| {
                let delay = value.parse::<u64>().map_err(|_| ())?;
                (value == delay.to_string()).then_some(delay).ok_or(())
            })
            .transpose()?;
        if outcome.status != status.as_u16() || outcome.retry_after != retry_after {
            return Err(());
        }
        Ok(outcome)
    }
}

fn single_header<'a>(
    headers: &'a reqwest::header::HeaderMap,
    name: &str,
) -> Result<Option<&'a str>, ()> {
    let all = headers.get_all(name);
    let mut values = all.iter();
    let value = values.next();
    if values.next().is_some() {
        return Err(());
    }
    value
        .map(|value| value.to_str().map_err(|_| ()))
        .transpose()
}

impl TenantExecution {
    pub(crate) fn is_private_adapter_endpoint(&self, url: &Url) -> bool {
        self.service
            .private_adapter
            .as_ref()
            .is_some_and(|peer| peer.endpoints.contains(url))
    }

    pub(crate) fn check_private_adapter_request(
        &self,
        request: &Request,
        stream: Option<bool>,
    ) -> Result<(), BitrouterError> {
        if stream.is_none()
            || !self.route.is_ordered()
            || !self.profiled()
            || request.method() != Method::POST
            || request.headers().contains_key(ATTEMPT_HEADER)
            || request.headers().contains_key(OUTCOME_HEADER)
        {
            return Err(self.reject(ErrorCode::Forbidden));
        }
        Ok(())
    }

    /// Mint after the exact attempt has been armed. Each physical send has its
    /// own exchange, including two requests with identical bodies.
    pub(crate) fn handoff_private_adapter(
        self: &Arc<Self>,
        request: &mut Request,
        target: &RoutingTarget,
        credential: &ResourceRef,
        attempt_id: AttemptId,
        stream: bool,
    ) -> Result<Exchange, BitrouterError> {
        self.handoff(request, target, credential, attempt_id, stream)
            .map_err(|code| self.reject(code))
    }

    fn handoff(
        self: &Arc<Self>,
        request: &mut Request,
        target: &RoutingTarget,
        credential: &ResourceRef,
        attempt_id: AttemptId,
        stream: bool,
    ) -> Result<Exchange, ErrorCode> {
        let peer = self
            .service
            .private_adapter
            .as_ref()
            .filter(|peer| peer.endpoints.contains(request.url()))
            .ok_or(ErrorCode::Forbidden)?;
        let hop = self.bound_hop(target)?.ok_or(ErrorCode::Forbidden)?;
        let profile = hop.profile.as_ref().ok_or(ErrorCode::Forbidden)?;
        let ordinal = hop.ordinal.ok_or(ErrorCode::Forbidden)?;
        let binding = &self.submission.binding;
        let bytes = request
            .body()
            .and_then(reqwest::Body::as_bytes)
            .ok_or(ErrorCode::InvalidRequest)?;
        let max_lifetime = std::time::Duration::from_millis(MAX_ATTEMPT_LIFETIME_MS);
        let lifetime = self.remaining().unwrap_or(max_lifetime).min(max_lifetime);
        let lifetime = u64::try_from(lifetime.as_millis())
            .ok()
            .filter(|lifetime| *lifetime > 0)
            .ok_or(ErrorCode::CapacityExceeded)?;
        let now = u64::try_from(chrono::Utc::now().timestamp_millis())
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        let claim = Attempt {
            audience: peer.audience.clone(),
            installation_id: binding.installation_id.clone(),
            instance_id: binding.instance_id.clone(),
            tenant_id: self.submission.tenant.clone(),
            principal_id: self.submission.principal.clone(),
            execution_revision: binding.execution_revision.clone(),
            execution_id: self.id.clone(),
            correlation_id: binding.correlation_id.clone(),
            attempt_id,
            route: self.route.reference().clone(),
            profile: profile.clone(),
            credential: credential.clone(),
            ordinal,
            rank: u16::try_from(hop.rank).map_err(|_| ErrorCode::Forbidden)?,
            method: request.method().to_string(),
            endpoint: request.url().to_string(),
            body_sha256: URL_SAFE_NO_PAD.encode(Sha256::digest(bytes)),
            stream,
            expires_at_ms: now.saturating_add(lifetime),
        };
        let payload = serde_json::to_vec(&claim).map_err(|_| ErrorCode::AuthorityUnavailable)?;
        let attempt_mac = peer.attempt_mac(&payload)?;
        let wire = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(payload),
            URL_SAFE_NO_PAD.encode(attempt_mac)
        );
        if wire.len() > MAX_HEADER_BYTES {
            return Err(ErrorCode::InvalidRequest);
        }
        let mut header = reqwest::header::HeaderValue::from_str(&wire)
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        header.set_sensitive(true);
        request.headers_mut().insert(ATTEMPT_HEADER, header);
        Ok(Exchange {
            peer: Arc::clone(peer),
            execution: Arc::clone(self),
            hop,
            attempt_mac,
            stream,
        })
    }
}
