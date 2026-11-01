//! Generic tenant routing over an already-authorized retained candidate set.
//! No credentials, provider catalog expansion, billing, or retry authority live here.

mod profile;
pub(crate) mod state;

use bitrouter_ai::{
    routing::SortOrder,
    tenancy::{configuration::ResourceRef, identity::InstallationId},
};
use serde::{Deserialize, Serialize};

use crate::{
    config::{Config, pattern::Pattern},
    gateway::{error::BitrouterError, pipeline::types::RoutingTarget},
};

/// Execution-only metadata derived from authenticated retained configuration.
/// It does not alter the encrypted target projection or old envelope bytes.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Metadata {
    pub(crate) provider_bucket: Option<String>,
    pub(crate) provider_rpm: Option<u32>,
    pub(crate) model_rpm: Option<u32>,
    /// Caller-supplied technical relative weight, with no currency or billing meaning.
    pub(crate) relative_weight: Option<u64>,
}

impl Metadata {
    pub(crate) fn from_config(config: &Config, target: &RoutingTarget) -> Self {
        let Some(provider) = config.providers.get(&target.provider_name) else {
            return Self::default();
        };
        let model = provider.model_config(&target.service_id);
        let matched = provider.rate_limits.resolve_entry(&target.service_id);
        let (provider_bucket, provider_rpm) = match matched {
            Some((pattern, limit)) if limit.requests_per_minute.is_some() => {
                let name = match pattern {
                    Pattern::Wildcard => "*".to_owned(),
                    Pattern::Prefix(prefix) => format!("{prefix}*"),
                    Pattern::Exact(exact) => exact.clone(),
                };
                (Some(name), limit.requests_per_minute)
            }
            _ => (None, None),
        };
        Self {
            provider_bucket,
            provider_rpm,
            model_rpm: model
                .and_then(|model| model.rate_limits.as_ref())
                .and_then(|limit| limit.requests_per_minute),
            relative_weight: model.and_then(|model| model.routing_weight),
        }
    }

    pub(crate) fn limits(&self) -> state::Limits {
        state::Limits {
            provider: self.provider_bucket.clone().zip(self.provider_rpm),
            model: self.model_rpm,
        }
    }
}

/// Includes the full credential owner/kind/id/revision. An operator credential
/// intentionally shared by grants has the same namespace across tenant callers.
fn key(
    installation: &InstallationId,
    credential: &ResourceRef,
    target: &RoutingTarget,
) -> Result<state::Key, BitrouterError> {
    let namespace = serde_json::to_string(&(installation, credential))
        .map_err(|_| BitrouterError::internal("routing credential identity unavailable"))?;
    Ok(state::Key {
        namespace,
        provider: target.provider_name.clone(),
        model: target.service_id.clone(),
        account: target.account_label.clone(),
        endpoint: target.api_base.clone(),
    })
}

fn order(sort: SortOrder) -> state::Order {
    match sort {
        SortOrder::Alphabetical => state::Order::Retained,
        SortOrder::Latency => state::Order::Latency,
        SortOrder::Cost => state::Order::Cost,
        SortOrder::Throughput => state::Order::Throughput,
    }
}

fn refusal(error: state::Refusal) -> BitrouterError {
    match error {
        state::Refusal::RateLimited => BitrouterError::UpstreamRateLimited {
            retry_after: Some(61),
            detail: None,
        },
        state::Refusal::CircuitOpen => BitrouterError::UpstreamUnavailable,
        state::Refusal::Capacity | state::Refusal::Unavailable => {
            BitrouterError::internal("tenant routing capacity unavailable")
        }
    }
}

/// Install request-local ownership and ordering in the hosted pipeline.
pub(crate) fn install(
    builder: &mut crate::gateway::pipeline::builder::PipelineBuilder,
    execution: &std::sync::Arc<crate::tenancy::execution::TenantExecution>,
    runtime: std::sync::Arc<state::Runtime>,
) {
    use bitrouter_ai::types::OperationScope;
    let profile = profile::Profile::new(execution, runtime);
    builder
        .pre_resolution_hook_for(profile.clone(), OperationScope::Both)
        .route_hook_for(profile.clone(), OperationScope::Both)
        .execution_hook_for(profile.clone(), OperationScope::Both)
        .observe_hook_for(profile, OperationScope::Both);
}

/// Call at the actual send boundary, before arming the receipt. A missing
/// profile for a tenant execution is a closed gate, never an ambient fallback.
pub(crate) fn admit(
    ctx: &crate::gateway::pipeline::context::PipelineContext,
    target: &RoutingTarget,
) -> crate::gateway::error::Result<profile::SendGuard> {
    ctx.extension::<profile::Profile>()
        .ok_or_else(|| BitrouterError::Forbidden("tenant routing custody unavailable".into()))?
        .admit(ctx, target)
}

/// Record an explicit provider response before an executor-internal continuation
/// (for example an authenticated OAuth refresh). This is not retry authorization.
pub(crate) fn response_failed(
    ctx: &crate::gateway::pipeline::context::PipelineContext,
    target: &RoutingTarget,
    error: &BitrouterError,
) {
    if let Some(profile) = ctx.extension::<profile::Profile>() {
        profile.response_failed(target, error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_ai::tenancy::authority::{OwnerKind, ResourceKind};

    #[test]
    fn namespaces_use_exact_credential_ownership_and_revision() -> anyhow::Result<()> {
        let source = serde_json::json!({"providers": {"example": {
            "api_base": "https://example.com/v1", "models": [{"id": "model"}]
        }}});
        let config: Config = serde_json::from_value(source)?;
        let targets = crate::config::routing_table::resolve_clean_route_chain(
            &config,
            "example:model",
            &Default::default(),
        )?;
        let target = targets
            .first()
            .ok_or_else(|| anyhow::anyhow!("missing target"))?;
        let installation: InstallationId = "installation".to_owned().try_into()?;
        let mut credential = ResourceRef {
            owner_kind: OwnerKind::Tenant,
            owner_id: "tenant-a".to_owned().try_into()?,
            resource_kind: ResourceKind::Credential,
            resource_id: "same-name".to_owned().try_into()?,
            revision_id: "v1".to_owned().try_into()?,
        };
        let first = key(&installation, &credential, target)?;
        credential.owner_id = "tenant-b".to_owned().try_into()?;
        assert!(first != key(&installation, &credential, target)?);
        credential.owner_id = "tenant-a".to_owned().try_into()?;
        credential.revision_id = "v2".to_owned().try_into()?;
        assert!(first != key(&installation, &credential, target)?);
        credential.revision_id = "v1".to_owned().try_into()?;
        credential.owner_kind = OwnerKind::OperatorPool;
        // No caller tenant is appended to an explicitly granted operator pool.
        let shared = key(&installation, &credential, target)?;
        assert!(shared == key(&installation, &credential, target)?);
        assert!(shared != first);
        Ok(())
    }
}
