//! Immutable, selected-route execution views. Membership never caches grants.

mod compact;
mod native_v1;
mod retained;
mod target;
mod weight;

use std::{collections::HashMap, sync::Arc};

use anyhow::Result;
use async_trait::async_trait;
use bitrouter_ai::{
    routing::{ModelInfo, SortOrder},
    tenancy::{
        configuration::{Configuration, GenerationDefaults, ResourceRef},
        identity::{AuthorityRevision, InstallationId, RevisionId, TenantId},
        operation::ErrorCode,
    },
    types::{ApiProtocol, ModelOperation},
};
use serde_json::Value;

use crate::{
    config::Config,
    gateway::{
        caller::CallerContext,
        error::{BitrouterError, Result as GatewayResult},
        pipeline::{
            routing::{ModelResolution, PromptOverrides, RoutingPrefs, RoutingTable},
            types::RoutingTarget,
        },
    },
    policy_lock::PolicyLock,
    tenancy::cache::Pin as CachePin,
};

use super::{Envelope, compiler};
use native_v1::NativeChains;

/// A validated retained revision. No mutable source or authority handles live here.
pub(crate) struct RetainedExecutionRevision {
    installation: InstallationId,
    tenant: TenantId,
    revision: RevisionId,
    authority_revision: AuthorityRevision,
    document: Configuration,
    routes: HashMap<String, Arc<RouteData>>,
}

/// The result of the single authenticated decode, consumed without rereading JSON.
pub(super) struct ValidatedEnvelope {
    document: Configuration,
    routes: HashMap<String, Arc<RouteData>>,
}

pub(super) fn validate(envelope: Envelope) -> Result<ValidatedEnvelope> {
    retained::decode(envelope)
}

impl ValidatedEnvelope {
    /// Create/activate/restore qualify every exposed route. Integrity-only
    /// reads keep the same authenticated decode without enabling execution.
    pub(super) fn require_capabilities(&self) -> std::result::Result<(), ErrorCode> {
        for route in self.routes.values() {
            route.require_capabilities()?;
        }
        Ok(())
    }
}

impl RetainedExecutionRevision {
    pub(super) fn new(
        installation: InstallationId,
        tenant: TenantId,
        revision: RevisionId,
        authority_revision: AuthorityRevision,
        validated: ValidatedEnvelope,
    ) -> std::result::Result<Self, ErrorCode> {
        let mut value = Self {
            installation,
            tenant,
            revision,
            authority_revision,
            document: validated.document,
            routes: validated.routes,
        };
        compact::identifiers(&mut value)?;
        Ok(value)
    }

    pub(crate) fn installation_id(&self) -> &InstallationId {
        &self.installation
    }
    pub(crate) fn tenant_id(&self) -> &TenantId {
        &self.tenant
    }
    pub(crate) fn revision_id(&self) -> &RevisionId {
        &self.revision
    }
    pub(crate) fn document(&self) -> &Configuration {
        &self.document
    }

    pub(crate) fn authority_revision(&self) -> AuthorityRevision {
        self.authority_revision
    }

    pub(crate) fn retained_heap_bytes(&self) -> std::result::Result<usize, ErrorCode> {
        weight::retained_heap_bytes(self)
    }

    /// Match the literal public resource ID, without trimming or selector parsing.
    fn selected_data(
        &self,
        raw_model: &str,
        inbound: &ApiProtocol,
    ) -> std::result::Result<RetainedRoute, ErrorCode> {
        if matches!(inbound, ApiProtocol::Custom(_)) {
            return Err(ErrorCode::InvalidRequest);
        }
        let data = self.routes.get(raw_model).ok_or(ErrorCode::NotFound)?;
        if !data
            .chains
            .iter()
            .any(|chain| chain.selector == data.selector && chain.inbound == *inbound)
        {
            return Err(ErrorCode::InvalidRequest);
        }
        Ok(RetainedRoute {
            data: data.clone(),
            selection: RouteSelection::Wire(inbound.clone()),
            generation_defaults: self.document.generation_defaults.clone(),
            _revision: None,
        })
    }

    fn selected_native_data(
        &self,
        raw_model: &str,
        operation: ModelOperation,
    ) -> std::result::Result<RetainedRoute, ErrorCode> {
        let data = self.routes.get(raw_model).ok_or(ErrorCode::NotFound)?;
        let chains = data.native.chains()?;
        if !chains
            .iter()
            .any(|chain| chain.selector == data.selector && chain.operation == operation)
        {
            return Err(ErrorCode::CapabilityMismatch);
        }
        Ok(RetainedRoute {
            data: data.clone(),
            selection: RouteSelection::Native(operation),
            generation_defaults: self.document.generation_defaults.clone(),
            _revision: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn select_native(
        &self,
        raw_model: &str,
        operation: ModelOperation,
    ) -> std::result::Result<Arc<RetainedRoute>, ErrorCode> {
        self.selected_native_data(raw_model, operation)
            .map(Arc::new)
    }

    /// Uncached decoder fixtures do not participate in a live service budget.
    #[cfg(test)]
    pub(crate) fn select(
        &self,
        raw_model: &str,
        inbound: &ApiProtocol,
    ) -> std::result::Result<Arc<RetainedRoute>, ErrorCode> {
        self.selected_data(raw_model, inbound).map(Arc::new)
    }
}

impl CachePin<RetainedExecutionRevision> {
    /// Canonical requests have an operation but no inbound wire preference.
    pub(crate) fn select_native(
        &self,
        raw_model: &str,
        operation: ModelOperation,
    ) -> std::result::Result<Arc<RetainedRoute>, ErrorCode> {
        let mut route = self.selected_native_data(raw_model, operation)?;
        route._revision = Some(self.clone());
        Ok(Arc::new(route))
    }

    /// Every escaping route retains the entire charged revision, including its
    /// unselected routes. A selected route cannot outlive cache accounting.
    pub(crate) fn select(
        &self,
        raw_model: &str,
        inbound: &ApiProtocol,
    ) -> std::result::Result<Arc<RetainedRoute>, ErrorCode> {
        let mut route = self.selected_data(raw_model, inbound)?;
        route._revision = Some(self.clone());
        Ok(Arc::new(route))
    }
}

enum RouteSelection {
    Wire(ApiProtocol),
    Native(ModelOperation),
}

impl RouteSelection {
    fn operation(&self) -> ModelOperation {
        match self {
            Self::Wire(inbound) => inbound.operation(),
            Self::Native(operation) => *operation,
        }
    }

    fn accepts(&self, prefs: &RoutingPrefs) -> bool {
        prefs.operation == self.operation()
            && match self {
                Self::Wire(inbound) => prefs.inbound_protocol.as_ref().is_none_or(|p| p == inbound),
                Self::Native(_) => prefs.inbound_protocol.is_none(),
            }
    }
}

/// One request's selection-bound view of exactly one qualified route.
pub(crate) struct RetainedRoute {
    data: Arc<RouteData>,
    selection: RouteSelection,
    generation_defaults: GenerationDefaults,
    _revision: Option<CachePin<RetainedExecutionRevision>>,
}

struct RouteData {
    reference: ResourceRef,
    dependencies: Vec<ResourceRef>,
    selector: String,
    overrides: PromptOverrides,
    policy: Option<RetainedPolicy>,
    chains: Vec<Chain>,
    native: NativeChains,
    candidates: Vec<Candidate>,
}

impl RouteData {
    fn require_capabilities(&self) -> std::result::Result<(), ErrorCode> {
        // A retained routing artifact owns routing, never provider-global state.
        // Keep old encrypted bytes readable, but reject execution qualification
        // even when another inbound wire would hide these supplemental extras.
        if ["previous_response_id", "conversation"]
            .iter()
            .any(|field| {
                self.overrides
                    .params
                    .get(*field)
                    .is_some_and(|value| !value.is_null())
            })
        {
            return Err(ErrorCode::InvalidRequest);
        }
        match &self.policy {
            Some(policy) => policy.require_capabilities(),
            None => Ok(()),
        }
    }
}

struct Chain {
    selector: String,
    inbound: ApiProtocol,
    candidates: Vec<usize>,
}

struct Candidate {
    target: RoutingTarget,
    projection: Vec<u8>,
    credential: ResourceRef,
}

/// Immutable policy artifact plus the explicit routing source it was bound to.
pub(crate) struct RetainedPolicy {
    reference: ResourceRef,
    name: String,
    document: Arc<PolicyLock>,
    routing_source: Arc<Value>,
}

impl RetainedPolicy {
    fn require_capabilities(&self) -> std::result::Result<(), ErrorCode> {
        let config = self.configuration()?;
        crate::tenancy::trajectory_profile::guarded(&config, self.document(), self.name())
            .map(|_| ())
            .map_err(|_| ErrorCode::CapabilityMismatch)
    }

    pub(crate) fn reference(&self) -> &ResourceRef {
        &self.reference
    }
    pub(crate) fn name(&self) -> &str {
        &self.name
    }
    pub(crate) fn document(&self) -> &PolicyLock {
        &self.document
    }

    /// Parse only the retained explicit source for PolicyRuntime::from_document.
    /// No environment substitution, registry, credential backend or file is read;
    /// executable candidates still come exclusively from the retained chains.
    pub(crate) fn configuration(&self) -> std::result::Result<Config, ErrorCode> {
        compiler::routing_config(&self.routing_source)
            .map(|(config, _)| config)
            .map_err(|_| ErrorCode::AuthorityUnavailable)
    }
}

impl RetainedRoute {
    /// Admission checks only the literal selected route, before acquiring a
    /// lease. Unused policy documents do not confer or require capabilities.
    pub(crate) fn require_capabilities(&self) -> std::result::Result<(), ErrorCode> {
        self.data.require_capabilities()
    }

    /// Configuration defaults enter canonical fields before routing, accounting
    /// or rendering. Explicit request values win; Decisions has no generation
    /// fields. The compiler-v1 retained bytes remain compatible on disk.
    pub(crate) fn apply_generation_defaults(
        &self,
        input: &mut crate::gateway::pipeline::types::PipelineInput,
    ) {
        if let Some(prompt) = input.generation_prompt_mut() {
            prompt
                .params
                .max_tokens
                .get_or_insert(self.generation_defaults.max_output_tokens);
            prompt
                .params
                .temperature
                .get_or_insert(self.generation_defaults.temperature);
        }
    }

    /// Exact private candidate identity, used to bind authentication to the same
    /// approved candidate at send time. These bytes contain no credential secret.
    pub(crate) fn candidate_key(
        &self,
        candidate: &RoutingTarget,
    ) -> std::result::Result<Vec<u8>, ErrorCode> {
        Ok(self.candidate(candidate)?.projection.clone())
    }

    pub(crate) fn reference(&self) -> &ResourceRef {
        &self.data.reference
    }
    pub(crate) fn dependencies(&self) -> &[ResourceRef] {
        &self.data.dependencies
    }
    pub(crate) fn policy(&self) -> Option<&RetainedPolicy> {
        self.data.policy.as_ref()
    }

    /// Candidate membership only. The caller must check live CredentialUse and
    /// execution authority separately, including after all waits and refreshes.
    pub(crate) fn match_candidate(
        &self,
        candidate: &RoutingTarget,
    ) -> std::result::Result<ResourceRef, ErrorCode> {
        Ok(self.candidate(candidate)?.credential.clone())
    }

    /// A fixed native count profile is selected before embedding callbacks.
    pub(in crate::tenancy) fn count_candidate(
        &self,
    ) -> std::result::Result<Option<&RoutingTarget>, ErrorCode> {
        if !matches!(self.selection, RouteSelection::Native(_)) {
            return Ok(None);
        }
        let mut candidate = None;
        let mut required = false;
        let mut fixed = true;
        for (_, indices) in self.selected_chains() {
            if indices.len() != 1 {
                fixed = false;
            }
            for index in indices {
                let target = &self
                    .data
                    .candidates
                    .get(*index)
                    .ok_or(ErrorCode::Forbidden)?
                    .target;
                required |= target.model_constraints.input_token_counting.is_some();
                if candidate.is_some_and(|prior| prior != *index) {
                    fixed = false;
                }
                candidate = Some(*index);
            }
        }
        if !required {
            return Ok(None);
        }
        let target = &self
            .data
            .candidates
            .get(candidate.ok_or(ErrorCode::Forbidden)?)
            .ok_or(ErrorCode::Forbidden)?
            .target;
        if !fixed
            || self.data.policy.is_some()
            || !matches!(
                self.selection,
                RouteSelection::Native(ModelOperation::Generation)
            )
            || target.api_protocol != ApiProtocol::Responses
            || target.model_constraints.input_token_counting
                != Some(bitrouter_ai::core::evidence::InputTokenCounting::Responses)
            || target.headers.iter().any(|rule| rule.passthrough())
        {
            return Err(ErrorCode::CapabilityMismatch);
        }
        Ok(Some(target))
    }

    pub(in crate::tenancy) fn require_oauth_messages(
        &self,
        target: &RoutingTarget,
    ) -> std::result::Result<(), ErrorCode> {
        if !matches!(self.selection, RouteSelection::Wire(ApiProtocol::Messages))
            || target.api_protocol != ApiProtocol::Messages
            || target.provider_name != "claude-code"
            || self.data.policy.is_some()
            || self
                .selected_chains()
                .any(|(_, indices)| indices.len() != 1)
            || target.headers.iter().any(|header| {
                header.passthrough()
                    || matches!(
                        header.name().as_str(),
                        "authorization"
                            | "x-api-key"
                            | "anthropic-version"
                            | "anthropic-beta"
                            | "user-agent"
                            | "x-app"
                    )
            })
        {
            return Err(ErrorCode::CapabilityMismatch);
        }
        self.candidate(target)?;
        Ok(())
    }

    fn candidate(&self, candidate: &RoutingTarget) -> std::result::Result<&Candidate, ErrorCode> {
        let projection = target::project(candidate).map_err(|_| ErrorCode::Forbidden)?;
        self.selected_chains()
            .flat_map(|(_, indices)| indices)
            .filter_map(|index| self.data.candidates.get(*index))
            .find(|retained| retained.projection == projection)
            .ok_or(ErrorCode::Forbidden)
    }

    fn selected_chains(&self) -> impl Iterator<Item = (&str, &[usize])> {
        let wire = self
            .data
            .chains
            .iter()
            .filter_map(|chain| match &self.selection {
                RouteSelection::Wire(inbound) if chain.inbound == *inbound => {
                    Some((chain.selector.as_str(), chain.candidates.as_slice()))
                }
                _ => None,
            });
        let native =
            self.data
                .native
                .chains()
                .into_iter()
                .flatten()
                .filter_map(|chain| match &self.selection {
                    RouteSelection::Native(operation) if chain.operation == *operation => {
                        Some((chain.selector.as_str(), chain.candidates.as_slice()))
                    }
                    _ => None,
                });
        wire.chain(native)
    }

    fn selector<'a>(&'a self, model: &'a str, raw: bool) -> GatewayResult<&'a str> {
        let selector = if raw && model == self.data.reference.resource_id.as_str() {
            self.data.selector.as_str()
        } else {
            model
        };
        if self
            .selected_chains()
            .any(|(selected, _)| selected == selector)
        {
            Ok(selector)
        } else {
            Err(BitrouterError::NotFound(
                "model is outside the selected retained route".into(),
            ))
        }
    }

    fn overrides(&self) -> PromptOverrides {
        if self.selection.operation() == ModelOperation::Decisions {
            PromptOverrides::default()
        } else {
            let mut overrides = self.data.overrides.clone();
            // Every token-limit spelling and temperature is supplied by the
            // canonical configuration/request fields, not a wire-shaped extra.
            for field in [
                "max_tokens",
                "max_completion_tokens",
                "max_output_tokens",
                "temperature",
            ] {
                overrides.params.remove(field);
            }
            overrides
        }
    }

    fn chain(
        &self,
        model: &str,
        prefs: &RoutingPrefs,
        raw: bool,
    ) -> GatewayResult<Vec<RoutingTarget>> {
        if !self.selection.accepts(prefs)
            || !prefs.require_tags.is_empty()
            || prefs.sort != SortOrder::default()
        {
            return Err(BitrouterError::bad_request(
                "unsupported retained routing preferences",
            ));
        }
        let selector = self.selector(model, raw)?;
        let (_, indices) = self
            .selected_chains()
            .find(|(selected, _)| *selected == selector)
            .ok_or_else(|| BitrouterError::NotFound("retained chain unavailable".into()))?;
        let mut targets = Vec::new();
        for index in indices {
            let target = &self
                .data
                .candidates
                .get(*index)
                .ok_or_else(|| BitrouterError::internal("invalid retained target index"))?
                .target;
            if (!prefs.only.is_empty() && !prefs.only.contains(&target.provider_name))
                || prefs.ignore.contains(&target.provider_name)
            {
                continue;
            }
            // Match ordinary routing: capabilities are positive observations,
            // never an exhaustive denylist. Missing facts remain unknown; actual
            // protocol/request feasibility is checked by the execution pipeline.
            let mut owned = target.clone();
            // Public RoutingTarget cannot carry the private cache pin. Header
            // clones otherwise retain shared Bytes allocations after eviction.
            // The remaining target fields clone into independently owned data.
            owned.headers = target
                .headers
                .iter()
                .map(crate::gateway::pipeline::types::OutboundHeaderRule::detached)
                .collect::<GatewayResult<_>>()?;
            targets.push(owned);
        }
        if targets.is_empty() {
            return Err(BitrouterError::NotFound(
                "no retained target satisfies the request".into(),
            ));
        }
        Ok(targets)
    }
}

#[async_trait]
impl RoutingTable for RetainedRoute {
    async fn resolve_model(&self, model: &str) -> GatewayResult<ModelResolution> {
        let selector = self.selector(model, true)?;
        let mut resolved = ModelResolution::passthrough(selector);
        if let RouteSelection::Native(operation) = &self.selection {
            resolved.prefs.operation = *operation;
        }
        if selector == self.data.selector {
            resolved.overrides = self.overrides();
            resolved.policy = self.data.policy.as_ref().map(|policy| policy.name.clone());
        }
        Ok(resolved)
    }

    async fn route_chain(
        &self,
        model: &str,
        prefs: &RoutingPrefs,
        _caller: &CallerContext,
    ) -> GatewayResult<Vec<RoutingTarget>> {
        self.chain(model, prefs, true)
    }

    async fn route_resolved(
        &self,
        model: &str,
        prefs: &RoutingPrefs,
        _caller: &CallerContext,
    ) -> GatewayResult<Vec<RoutingTarget>> {
        self.chain(model, prefs, false)
    }

    fn canonical_model_id(&self, model: &str, target: &RoutingTarget) -> Option<String> {
        (self.selector(model, true).is_ok() && self.match_candidate(target).is_ok())
            .then(|| self.data.reference.resource_id.to_string())
    }

    fn list_models(&self) -> Vec<ModelInfo> {
        self.model_info(self.data.reference.resource_id.as_str())
            .into_iter()
            .collect()
    }

    fn model_info(&self, model: &str) -> Option<ModelInfo> {
        if model != self.data.reference.resource_id.as_str() {
            return None;
        }
        let mut providers = Vec::new();
        for candidate in self
            .selected_chains()
            .flat_map(|(_, indices)| indices)
            .filter_map(|index| self.data.candidates.get(*index))
        {
            if !providers.contains(&candidate.target.provider_name) {
                providers.push(candidate.target.provider_name.clone());
            }
        }
        Some(ModelInfo {
            id: model.to_owned(),
            providers,
        })
    }

    async fn reload(&self) -> GatewayResult<()> {
        Err(BitrouterError::bad_request(
            "retained routing cannot be reloaded",
        ))
    }

    async fn preset_overrides(&self, model: &str) -> GatewayResult<PromptOverrides> {
        let selector = self.selector(model, true)?;
        Ok(if selector == self.data.selector {
            self.overrides()
        } else {
            PromptOverrides::default()
        })
    }
}
