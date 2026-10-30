//! Runtime-only native-selection-v1 over authenticated compiler-v1 source.
//!
//! Resolver/codec changes require v1 compatibility review against these goldens.
//! Native order comes from resolution with no wire preference; existing exact
//! same-selector approvals prove membership, never choose or reorder the chain.

use bitrouter_ai::{tenancy::operation::ErrorCode, types::ModelOperation};
use serde::Serialize;
use serde_json::Value;

use super::super::{
    ExecutionCapacityExceeded, compiler,
    manifest::{ByteBudget, CredentialBinding},
};
use super::{Candidate, Chain, target};
use crate::{
    config::{
        AccountStrategy, Config, presets::PresetResolution, router::RouterSelection, routing_table,
    },
    gateway::pipeline::routing::RoutingPrefs,
};

#[cfg(test)]
mod tests;

const MAX_CHAINS: usize = 4096;
const MAX_INDICES: usize = 4096;
const MAX_WORK_BYTES: usize = 8 * 1024 * 1024;

type Result<T> = std::result::Result<T, Unavailable>;

pub(super) struct NativeChain {
    pub(super) selector: String,
    pub(super) operation: ModelOperation,
    pub(super) candidates: Vec<usize>,
}

pub(super) enum NativeChains {
    Ready(Vec<NativeChain>),
    Unavailable(Unavailable),
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Unavailable {
    Unsupported,
    Capacity,
}

impl NativeChains {
    pub(super) fn chains(&self) -> std::result::Result<&[NativeChain], ErrorCode> {
        match self {
            Self::Ready(chains) => Ok(chains),
            Self::Unavailable(Unavailable::Unsupported) => Err(ErrorCode::CapabilityMismatch),
            Self::Unavailable(Unavailable::Capacity) => Err(ErrorCode::CapacityExceeded),
        }
    }
}

/// Cumulative per-revision work. Failed route attempts consume their work too;
/// exhaustion stops later attempts before parsing or allocating another source.
/// This serialized/source-expansion quota is separate from retained heap weight.
pub(super) struct Budget<'a> {
    operation: Option<&'a crate::storage::operation_budget::OperationBudget<'a>>,
    rejected: Option<crate::storage::operation_budget::Error>,
    chains: usize,
    indices: usize,
    work: ByteBudget,
    exhausted: bool,
}

impl<'a> Budget<'a> {
    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self::with_operation(None)
    }

    pub(super) fn with_operation(
        operation: Option<&'a crate::storage::operation_budget::OperationBudget<'a>>,
    ) -> Self {
        Self {
            operation,
            rejected: None,
            chains: MAX_CHAINS,
            indices: MAX_INDICES,
            work: ByteBudget::new(MAX_WORK_BYTES),
            exhausted: false,
        }
    }

    pub(super) fn block_config(&mut self, source: &Value) -> anyhow::Result<Config> {
        compiler::routing_config_observed(source, Some(&mut self.work), self.operation)
            .map(|(config, _)| config)
    }

    pub(super) fn block_pricing_bytes(&mut self, bytes: usize) -> anyhow::Result<()> {
        self.bytes(bytes)
            .map_err(|_| anyhow::Error::new(ExecutionCapacityExceeded))
    }

    fn shared(&mut self, bytes: usize) -> Result<()> {
        if let Some(operation) = self.operation
            && let Err(error) = operation.charge(0, bytes)
        {
            self.rejected = Some(error);
            return Err(Unavailable::Capacity);
        }
        Ok(())
    }

    /// A caller-level quota rejection cannot become a readable HTTP-only route.
    pub(super) fn check_operation(&mut self) -> anyhow::Result<()> {
        if let Some(error) = self.rejected.take() {
            return Err(error.into());
        }
        if let Some(operation) = self.operation {
            operation.check()?;
        }
        Ok(())
    }

    /// Target decoding has a separate 8 MiB content quota. Charge its exact
    /// serialized content before the typed clone/projection is allocated.
    pub(super) fn target(&self, value: &Value) -> anyhow::Result<()> {
        if let Some(operation) = self.operation {
            let mut measured = ByteBudget::new(super::super::MAX_ARTIFACT_BYTES);
            measured.charge(value)?;
            let bytes = super::super::MAX_ARTIFACT_BYTES - measured.remaining();
            operation.charge(0, bytes.checked_mul(2).ok_or(ExecutionCapacityExceeded)?)?;
        }
        Ok(())
    }

    fn bytes(&mut self, bytes: usize) -> Result<()> {
        let remaining = self
            .work
            .remaining()
            .checked_sub(bytes)
            .ok_or(Unavailable::Capacity)?;
        self.shared(bytes)?;
        self.work = ByteBudget::new(remaining);
        Ok(())
    }

    fn value(&mut self, value: &impl Serialize) -> Result<()> {
        let before = self.work.remaining();
        let result = self.work.charge(value).map_err(|_| Unavailable::Capacity);
        // Serialization itself is allocation-free. Even a failed local quota
        // check consumes its measured prefix in the caller's cumulative work.
        self.shared(before - self.work.remaining())?;
        result
    }

    fn prefs(&mut self, prefs: &RoutingPrefs) -> Result<()> {
        let RoutingPrefs {
            operation,
            sort,
            require_tags,
            only,
            ignore,
            require_capabilities,
            inbound_protocol,
            registry_requirements,
        } = prefs;
        // These are compiled Stage-0 preferences, not request-time requirements.
        // Compiler-v1 neither retains nor accounts for registry selection state.
        if registry_requirements.is_some() {
            return Err(Unavailable::Unsupported);
        }
        self.value(&(
            operation,
            sort,
            require_tags,
            only,
            ignore,
            require_capabilities,
            inbound_protocol,
        ))
    }

    fn chain(&mut self, selector: &str) -> Result<()> {
        self.chains = self.chains.checked_sub(1).ok_or(Unavailable::Capacity)?;
        self.bytes(
            selector
                .len()
                .checked_add(std::mem::size_of::<NativeChain>())
                .ok_or(Unavailable::Capacity)?,
        )
    }

    fn preflight(&mut self, count: usize, bytes: usize) -> Result<()> {
        if count > self.indices {
            return Err(Unavailable::Capacity);
        }
        self.bytes(bytes)?;
        self.bytes(
            count
                .checked_mul(std::mem::size_of::<usize>())
                .ok_or(Unavailable::Capacity)?,
        )
    }
}

pub(super) fn derive(
    base: &str,
    source: &Value,
    credentials: &[CredentialBinding],
    approved: &[Chain],
    candidates: &[Candidate],
    budget: &mut Budget<'_>,
) -> NativeChains {
    if budget.exhausted {
        return NativeChains::Unavailable(Unavailable::Capacity);
    }
    match derive_inner(base, source, credentials, approved, candidates, budget) {
        Ok(chains) => NativeChains::Ready(chains),
        Err(error) => {
            if matches!(error, Unavailable::Capacity) {
                budget.exhausted = true;
            }
            // The local Vec and every partial chain drop before this marker is
            // published. No usable base with missing policy alternatives escapes.
            NativeChains::Unavailable(error)
        }
    }
}

fn derive_inner(
    base: &str,
    source: &Value,
    credentials: &[CredentialBinding],
    approved: &[Chain],
    candidates: &[Candidate],
    budget: &mut Budget<'_>,
) -> Result<Vec<NativeChain>> {
    for (position, chain) in approved.iter().enumerate() {
        if !approved[..position].iter().any(|earlier| {
            earlier.selector == chain.selector
                && earlier.inbound.operation() == chain.inbound.operation()
        }) {
            budget.chain(&chain.selector)?;
        }
    }
    let (mut config, sizes) =
        match compiler::routing_config_observed(source, Some(&mut budget.work), budget.operation) {
            Ok(value) => value,
            Err(error) => match error.downcast::<crate::storage::operation_budget::Error>() {
                Ok(error) => {
                    budget.rejected = Some(error);
                    return Err(Unavailable::Capacity);
                }
                Err(error) => return Err(classify(error)),
            },
        };
    // Match immutable compilation: never consume the process balance counter.
    for provider in config.providers.values_mut() {
        provider.account_strategy = AccountStrategy::Failover;
    }
    let root = resolve_stage0(&config, base, budget)?;
    if !root.request_checks.is_empty() {
        return Err(Unavailable::Unsupported);
    }
    let mut result = Vec::new();
    for (position, chain) in approved.iter().enumerate() {
        let operation = chain.inbound.operation();
        if approved[..position].iter().any(|earlier| {
            earlier.selector == chain.selector && earlier.inbound.operation() == operation
        }) {
            continue;
        }
        let selected = if chain.selector == base {
            None
        } else {
            let selected = resolve_stage0(&config, &chain.selector, budget)?;
            if selected.policy.is_some() || !selected.request_checks.is_empty() {
                return Err(Unavailable::Unsupported);
            }
            Some(selected)
        };
        // Stage 0 exactly once. A clean provider model may itself end in a
        // configured variant name; resolve_route_chain would reinterpret it.
        let (clean, mut prefs) = match &selected {
            None => {
                budget.prefs(&root.prefs)?;
                (&root.clean_model, root.prefs.clone())
            }
            Some(selected) => {
                // Reserve both copies before cloning or merging. Root entries
                // already present in the selected prefs still count.
                budget.prefs(&selected.prefs)?;
                budget.prefs(&root.prefs)?;
                let mut prefs = selected.prefs.clone();
                routing_table::merge_prefs(&mut prefs, &root.prefs);
                (&selected.clean_model, prefs)
            }
        };
        prefs.operation = operation;
        prefs.inbound_protocol = None;
        let (count, bytes) = compiler::resolution_capacity(&config, &sizes, clean)
            .map_err(|_| Unavailable::Capacity)?;
        budget.preflight(count, bytes)?;
        let resolved = routing_table::resolve_clean_route_chain(&config, clean, &prefs)
            .map_err(|_| Unavailable::Unsupported)?;
        if resolved.is_empty() {
            return Err(Unavailable::Unsupported);
        }
        budget.indices = budget
            .indices
            .checked_sub(resolved.len())
            .ok_or(Unavailable::Capacity)?;
        let mut indices = Vec::with_capacity(resolved.len());
        for target in &resolved {
            let binding = credentials
                .iter()
                .find(|binding| {
                    binding.provider == target.provider_name
                        && binding.account == target.account_label.as_deref().unwrap_or("default")
                })
                .ok_or(Unavailable::Unsupported)?;
            let value = compiler::target_value(target).map_err(classify)?;
            budget.value(&value)?;
            let (_, projection) = target::decode(value).map_err(classify)?;
            budget.bytes(projection.len())?;
            let index = candidates
                .iter()
                .position(|candidate| {
                    candidate.projection == projection && candidate.credential == binding.credential
                })
                .ok_or(Unavailable::Unsupported)?;
            if !approved.iter().any(|approved| {
                approved.selector == chain.selector
                    && approved.inbound.operation() == operation
                    && approved.candidates.contains(&index)
            }) {
                return Err(Unavailable::Unsupported);
            }
            indices.push(index);
        }
        result.push(NativeChain {
            selector: chain.selector.clone(),
            operation,
            candidates: indices,
        });
    }
    Ok(result)
}

// Borrow the exact named definition Stage 0 can copy, without resolving it or
// materializing defaults. Keep this preflight aligned with config/presets.rs;
// it only reserves work, while the existing v1 resolver owns all semantics.
#[derive(Serialize)]
struct Stage0Source<'a> {
    model: Option<&'a str>,
    policy: Option<&'a str>,
    system_prompt: Option<&'a str>,
    params: &'a serde_json::Map<String, Value>,
    routing: &'a bitrouter_ai::routing::RoutingConfig,
}

fn named_source<'a>(config: &'a Config, name: &str) -> Result<Option<Stage0Source<'a>>> {
    if let Some(router) = config.routers.get(name) {
        let crate::config::router::RouterConfig {
            selection,
            defaults,
            checks,
        } = router;
        let crate::config::router::RouterDefaults {
            system_prompt,
            params,
        } = defaults;
        if !checks.request.is_empty() {
            return Err(Unavailable::Unsupported);
        }
        let (model, policy, routing) = match selection {
            RouterSelection::Model { model, routing } => (model, None, routing),
            RouterSelection::Policy {
                policy,
                base_model,
                routing,
            } => (base_model, Some(policy.as_str()), routing),
        };
        Ok(Some(Stage0Source {
            model: Some(model),
            policy,
            system_prompt: system_prompt.as_deref(),
            params,
            routing,
        }))
    } else {
        Ok(config.presets.get(name).map(
            |crate::config::PresetConfig {
                 model,
                 policy,
                 system_prompt,
                 params,
                 routing,
             }| Stage0Source {
                model: model.as_deref(),
                policy: policy.as_deref(),
                system_prompt: system_prompt.as_deref(),
                params,
                routing,
            },
        ))
    }
}

fn resolve_stage0(config: &Config, raw: &str, budget: &mut Budget<'_>) -> Result<PresetResolution> {
    let (head, variant) = match raw.rsplit_once(':') {
        Some((head, name)) if config.variants.contains_key(name) => (head, Some(name)),
        _ => (raw, None),
    };
    // Literal clean model, original selector and variant identity copies.
    budget.value(&(head, raw, variant))?;
    if let Some(name) = head
        .strip_prefix('@')
        .or_else(|| head.strip_prefix(crate::config::presets::RESERVED_NAMESPACE))
        && let Some(source) = named_source(config, name)?
    {
        // Returned model/policy/defaults and router prefs are each copied once.
        // Repeated tiers/variants must pay again, even for the same definition.
        budget.value(&source)?;
        budget.value(&name)?;
        // binding_digest allocates a JSON buffer plus a vector of borrowed
        // parameter keys. Its values exclude defaults, but include selection,
        // routing and every key. This maximal v1 fixed envelope plus separately
        // serialized values bounds its content before the resolver allocates it.
        const DIGEST_ENVELOPE: &str = r#"{"version":"router-v1","router_id":"","selection":{"kind":"policy","policy":"","base_model":null},"routing":null,"defaults":{"system_prompt_present":false,"param_keys":[]}}"#;
        const DIGEST_TEXT: usize = 64 + "router-v1:sha256:".len() + 64;
        budget.bytes(DIGEST_ENVELOPE.len() + DIGEST_TEXT)?;
        budget.value(&(name, source.model, source.policy, source.routing))?;
        for key in source.params.keys() {
            budget.bytes(std::mem::size_of::<&str>() + 1)?;
            budget.value(key)?;
        }
    }
    if let Some(variant) = variant.and_then(|name| config.variants.get(name)) {
        budget.value(&variant.routing)?;
    }
    config
        .resolve_router(raw)
        .map_err(|_| Unavailable::Unsupported)
}

fn classify(error: anyhow::Error) -> Unavailable {
    if error.is::<ExecutionCapacityExceeded>() {
        Unavailable::Capacity
    } else {
        Unavailable::Unsupported
    }
}
