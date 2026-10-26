//! Private checked association for the authenticated local ACP host.
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
        let [route_ref] = retained.document().routes.as_slice() else {
            return Err(ErrorCode::CapabilityMismatch.into());
        };
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
        let dependencies = route.dependencies().to_vec();
        if dependencies.is_empty() {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        authority
            .with_authority(&dependencies, capture_permissions)
            .map_err(Failure::from)?;
        let mode = retained.document().learning.mode;
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
        let association = Association::new(
            scope,
            provenance,
            uuid::Uuid::new_v4().to_string(),
            source.to_owned(),
            authority.revision().to_string(),
            model.clone(),
            mode,
        )
        .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
        drop((route, retained));
        let binding = Arc::new(Binding {
            authority,
            dependencies,
            association,
            closing: AtomicBool::new(false),
            ingress_rejected: AtomicBool::new(false),
            stop,
            model,
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
