//! Private checked association for the authenticated local ACP host.
pub(crate) mod human;
pub(crate) mod port;
pub(in crate::tenancy) mod queue;

use super::{Service, authority::Effective, error::Failure, execution::host::Authority};
use crate::{
    harness::{acp::controller::ProviderEndpointPlan, trajectory::owned::Association},
    storage::scope::Scope,
};
use agent_client_protocol::schema::v1::LlmProtocol;
use bitrouter_ai::{
    tenancy::{
        authority::{GrantScope, Permission},
        configuration::ResourceRef,
        operation::ErrorCode,
    },
    types::ApiProtocol,
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub(crate) struct Binding {
    authority: Arc<Authority>,
    dependencies: Vec<ResourceRef>,
    association: Association,
    closing: AtomicBool,
    ingress_rejected: AtomicBool,
    stop: tokio_util::sync::CancellationToken,
    model: String,
    capture_head: std::sync::atomic::AtomicI64,
    block_generation: std::sync::atomic::AtomicU64,
    assessment: Option<super::learning::Profile>,
    block: Option<Arc<super::resources::execution::RetainedRoute>>,
}
pub(crate) struct Prepared {
    pub(crate) binding: Arc<Binding>,
    pub(crate) endpoint: ProviderEndpointPlan,
}

impl Service {
    pub(crate) async fn prepare_capture(
        &self,
        headers: &http::HeaderMap,
        source: &str,
        listener: &str,
        stop: tokio_util::sync::CancellationToken,
    ) -> Result<Prepared, Failure> {
        let authority = self.host_authority(headers)?;
        if source.is_empty() || source.len() > 128 || source.chars().any(char::is_control) {
            return Err(ErrorCode::InvalidRequest.into());
        }
        // Original and current scopes must both explicitly cover the tenant.
        // A later broadening cannot upgrade a resource-scoped host opener.
        let (credential, recipient) = authority.scopes();
        if !matches!(credential, GrantScope::Tenant { .. })
            || !matches!(recipient, GrantScope::Tenant { .. })
        {
            return Err(ErrorCode::Forbidden.into());
        }
        let retained = authority.retained().await?;
        let route_ref = retained.capture_model().map_err(Failure::from)?;
        let model = route_ref.resource_id.to_string();
        let (route, protocol) = [
            (ApiProtocol::ChatCompletions, LlmProtocol::OpenAi),
            (ApiProtocol::Responses, LlmProtocol::OpenAi),
            (ApiProtocol::Messages, LlmProtocol::Anthropic),
        ]
        .into_iter()
        .find_map(|(api, protocol)| {
            retained
                .select(&model, &api)
                .ok()
                .map(|route| (route, protocol))
        })
        .ok_or_else(|| Failure::from(ErrorCode::CapabilityMismatch))?;
        route.require_capabilities().map_err(Failure::from)?;
        if route.policy().is_some_and(|policy| {
            policy
                .document()
                .policies
                .get(policy.name())
                .and_then(|definition| definition.optimization.as_ref())
                .and_then(|optimization| optimization.active.as_ref())
                .is_some()
        }) {
            return Err(ErrorCode::CapabilityMismatch.into());
        }
        let block = route.block_profile().map(|_| route.clone());
        if route
            .block_profile()
            .is_some_and(|profile| profile.definition.source != source)
            || (block.is_some()
                && retained.document().learning.mode
                    != bitrouter_ai::tenancy::configuration::LearningMode::Explore)
        {
            return Err(ErrorCode::CapabilityMismatch.into());
        }
        let dependencies = route.dependencies().to_vec();
        if dependencies.is_empty() {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        authority
            .with_authority(&dependencies, capture_permissions)
            .map_err(Failure::from)?;
        let mode = retained.document().learning.mode;
        let assessment = match (route.assessment_reference(), mode) {
            (Some(reference), mode)
                if mode != bitrouter_ai::tenancy::configuration::LearningMode::Off =>
            {
                let child = retained
                    .select_native(
                        reference.resource_id.as_str(),
                        bitrouter_ai::types::ModelOperation::Generation,
                    )
                    .map_err(Failure::from)?;
                if child.reference() != reference {
                    return Err(ErrorCode::AuthorityUnavailable.into());
                }
                child.require_capabilities().map_err(Failure::from)?;
                Some(super::learning::Profile {
                    route: reference.clone(),
                    dependencies: child.dependencies().to_vec(),
                    maximum: retained.document().learning.max_evaluations,
                    concurrent: u32::from(retained.document().limits.learning_jobs),
                })
            }
            _ => None,
        };
        // The listener is a trusted same-App embedding input, never a tenant URL.
        // Reject paths/credentials/query overrides and non-loopback destinations.
        let url =
            reqwest::Url::parse(listener).map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
        let loopback = match url.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        if !loopback
            || !matches!(url.scheme(), "http" | "https")
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(ErrorCode::InvalidRequest.into());
        }
        let mut endpoint_headers = BTreeMap::new();
        for name in [
            "authorization",
            "bitrouter-tenancy-version",
            "bitrouter-installation-id",
            "bitrouter-instance-id",
            "bitrouter-capability-revision",
            "bitrouter-execution-revision",
            "bitrouter-correlation-id",
            "bitrouter-receipt-mode",
        ] {
            let value = headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| Failure::from(ErrorCode::InvalidRequest))?;
            endpoint_headers.insert(name.to_owned(), value.to_owned());
        }
        let (scope, provenance) = Scope::for_host(&authority);
        let mut association = Association::new(
            scope,
            provenance,
            uuid::Uuid::new_v4().to_string(),
            source.to_owned(),
            authority.revision().to_string(),
            model.clone(),
            mode,
        )
        .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
        if block.is_some() {
            let nonce = hex::encode(rand::random::<[u8; 32]>());
            let commitment = crate::storage::evolution::digest(&("owned-block-link-v1", &nonce))
                .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
            association = association
                .with_block_link(crate::harness::trajectory::owned::ProviderLink {
                    version: 1,
                    commitment,
                    credential: authority.credential_id().to_owned(),
                })
                .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
            endpoint_headers.insert(super::execution::BLOCK_LINK_HEADER.into(), nonce);
        }
        drop((route, retained));
        let binding = Arc::new(Binding {
            authority,
            dependencies,
            association,
            closing: AtomicBool::new(false),
            ingress_rejected: AtomicBool::new(false),
            stop,
            model,
            capture_head: std::sync::atomic::AtomicI64::new(0),
            block_generation: std::sync::atomic::AtomicU64::new(0),
            assessment,
            block,
        });
        binding.check()?;
        let base_url = if protocol == LlmProtocol::OpenAi {
            format!("{}v1", url.as_str())
        } else {
            url.to_string()
        };
        Ok(Prepared {
            binding,
            endpoint: ProviderEndpointPlan {
                provider_id: "bitrouter".into(),
                protocol,
                base_url,
                headers: endpoint_headers,
            },
        })
    }
}

fn capture_permissions(effective: &Effective<'_>) -> Result<(), ErrorCode> {
    if !effective.covers(&GrantScope::Tenant {
        operator_pools: vec![],
    }) || ![
        Permission::ResourceRead,
        Permission::EvaluationSubmit,
        Permission::CredentialUse,
    ]
    .into_iter()
    .all(|permission| effective.permits(permission))
    {
        return Err(ErrorCode::Forbidden);
    }
    Ok(())
}

impl Binding {
    pub(in crate::tenancy) fn block_generation(&self) -> u64 {
        self.block_generation.load(Ordering::Acquire)
    }
    pub(in crate::tenancy) fn publish_block_generation(&self, generation: u64) {
        self.block_generation.store(generation, Ordering::Release);
    }
    pub(in crate::tenancy) fn observed_head(&self, head: i64) {
        self.capture_head.store(head, Ordering::Release);
    }
    pub(in crate::tenancy) fn current_head(&self) -> i64 {
        self.capture_head.load(Ordering::Acquire)
    }
    pub(in crate::tenancy) fn block_route(&self) -> Option<&ResourceRef> {
        self.block.as_ref().map(|route| route.reference())
    }
    pub(in crate::tenancy) fn block_receipt_target(
        &self,
        selector: &str,
        target: &bitrouter_ai::tenancy::execution::Target,
    ) -> bool {
        self.block
            .as_ref()
            .is_some_and(|route| route.block_receipt_target(selector, target))
    }
    pub(in crate::tenancy) fn block_token_cost(
        &self,
        selector: &str,
        attempt: &bitrouter_ai::tenancy::execution::Attempt,
    ) -> Option<u64> {
        self.block.as_ref()?.block_token_cost(selector, attempt)
    }
    pub(in crate::tenancy) fn begin_block_update(&self) -> Result<u64, Failure> {
        let _guard = self
            .service()
            .snapshot
            .write()
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        Ok(self.block_generation.swap(0, Ordering::AcqRel))
    }
    pub(in crate::tenancy) fn begin_observation(&self) -> Result<i64, Failure> {
        let _guard = self
            .service()
            .snapshot
            .write()
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        Ok(self.capture_head.swap(-1, Ordering::AcqRel))
    }
    pub(crate) fn block_profile(&self) -> Option<&super::resources::block::Profile> {
        self.block.as_ref().and_then(|route| route.block_profile())
    }
    pub(in crate::tenancy) fn matches_block_link(&self, link: &str) -> bool {
        link.len() == 64
            && link.bytes().all(|b| b.is_ascii_hexdigit())
            && self
                .association
                .descriptor()
                .block_link
                .as_ref()
                .is_some_and(|stored| {
                    crate::storage::evolution::digest(&("owned-block-link-v1", link))
                        .is_ok_and(|digest| digest == stored.commitment)
                })
    }
    pub(in crate::tenancy) fn block_in(
        &self,
        state: &super::model::Snapshot,
    ) -> Result<(), ErrorCode> {
        if self.block.is_none()
            || self.closing.load(Ordering::Acquire)
            || self.ingress_rejected()
            || self.stop.is_cancelled()
            || self.service().host_stop().is_cancelled()
        {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        self.authority
            .in_snapshot(state, &self.dependencies, capture_permissions)
    }
    pub(in crate::tenancy) fn assessment(&self) -> Option<&super::learning::Profile> {
        self.assessment.as_ref()
    }
    pub(in crate::tenancy) fn assessment_in(
        &self,
        state: &super::model::Snapshot,
    ) -> Result<(), ErrorCode> {
        if self.closing.load(Ordering::Acquire)
            || self.stop.is_cancelled()
            || self.service().host_stop().is_cancelled()
        {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        let profile = self
            .assessment
            .as_ref()
            .ok_or(ErrorCode::CapabilityMismatch)?;
        self.authority
            .in_snapshot(state, &self.dependencies, capture_permissions)?;
        self.authority
            .in_snapshot(state, &profile.dependencies, capture_permissions)
    }
    pub(in crate::tenancy) fn assessment_check(&self) -> Result<(), ErrorCode> {
        self.check().map_err(|error| error.0.code)?;
        let profile = self
            .assessment
            .as_ref()
            .ok_or(ErrorCode::CapabilityMismatch)?;
        if profile.maximum == 0 || profile.concurrent == 0 {
            return Err(ErrorCode::CapacityExceeded);
        }
        self.authority
            .with_authority(&profile.dependencies, capture_permissions)
    }

    pub(crate) fn authority(&self) -> &Authority {
        &self.authority
    }
    pub(crate) fn service(&self) -> &Service {
        self.authority.service()
    }
    pub(crate) fn association(&self) -> &Association {
        &self.association
    }
    pub(crate) fn model(&self) -> &str {
        &self.model
    }
    pub(crate) fn check(&self) -> Result<(), Failure> {
        self.with_start(|| ()).map_err(Failure::from)
    }
    /// Couple the synchronous permission check and actual child start to the
    /// current authority lock. No await is permitted inside work.
    pub(crate) fn with_start<T>(&self, work: impl FnOnce() -> T) -> Result<T, ErrorCode> {
        self.authority
            .with_authority(&self.dependencies, |effective| {
                capture_permissions(effective)?;
                if self.closing.load(Ordering::Acquire)
                    || self.stop.is_cancelled()
                    || self.service().host_stop().is_cancelled()
                {
                    return Err(ErrorCode::AuthorityUnavailable);
                }
                Ok(work())
            })
    }
    #[cfg(unix)]
    pub(crate) fn reject_ingress(&self) {
        // Serialize rejection with new-effect gates and already-admitted
        // delivery using the existing authority snapshot lock, not another registry.
        let _guard = match self.service().snapshot.write() {
            Ok(guard) => Some(guard),
            Err(_) => {
                self.service().host_cleanup_unknown();
                None
            }
        };
        self.ingress_rejected.store(true, Ordering::Release);
        self.stop.cancel();
    }
    pub(crate) fn deliver_observed<T>(&self, work: impl FnOnce() -> T) -> Result<T, ErrorCode> {
        let _guard = self
            .service()
            .snapshot
            .read()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        if self.ingress_rejected() {
            return Err(ErrorCode::InvalidRequest);
        }
        // This permits only delivery of an already-observed response/update.
        // Current authorization for a new effect still uses with_start.
        Ok(work())
    }
    pub(crate) fn ingress_rejected(&self) -> bool {
        self.ingress_rejected.load(Ordering::Acquire)
    }
    pub(crate) fn close(&self) -> Result<(), Failure> {
        let _guard = self
            .service()
            .snapshot
            .write()
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?;
        self.closing.store(true, Ordering::Release);
        Ok(())
    }
    pub(in crate::tenancy) fn origin(&self, proof: &Arc<super::transaction::Proof>) -> bool {
        self.authority.origin(proof)
    }
}

/// Narrow actual-controller cleanup consumer; the proof has no public constructor.
pub(crate) async fn retain_stopped(
    proof: crate::harness::acp::controlled::CleanupProof,
) -> anyhow::Result<()> {
    if proof.binding().assessment().is_none() || proof.binding().service().startup.is_none() {
        return Ok(());
    }
    super::learning::queue::capture_stopped(Arc::new(proof))
        .await
        .map_err(|_| anyhow::anyhow!("paid capture cleanup retention unconfirmed"))
}
