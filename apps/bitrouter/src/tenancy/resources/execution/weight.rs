//! Conservative retained heap accounting, not allocator/RSS measurement.
//!
//! The cache charges the root allocation and its Arc header. This traversal
//! charges only its heap descendants, including spare String/Vec capacity and
//! nested Arc allocations. Shared Arcs are visited once per root; independently
//! cached roots may safely charge the same allocation more than once.
//!
//! Opaque-layout audit: Rust 1.99 alloc BTree nodes (B=6), std/hashbrown 0.17
//! Swiss tables, serde_json 1.0.150 with preserve_order/indexmap 2.14, and
//! http 1.4.2/bytes 1.12.1. Reaudit these allowances on dependency/toolchain
//! changes. No private layout is accessed or transmuted.
//!
//! JSON maps have no public capacity accessor. Their bound relies on the
//! execution decoder's provenance: fresh deserialization from authenticated
//! bytes, followed only by moves/clones, never reserve/removal/retain. Do not
//! pass a manually grown-and-shrunk JSON map through this accounting boundary.
//! Opaque tenancy IDs must first be normalized by the cache integration through
//! `String -> Box<str> -> String -> typed ID`. That guarantees capacity == length
//! for every root ID and every ID in document/route/dependency/credential/policy
//! references; it avoids exposing allocation details through the wire crate.

use std::{
    alloc::Layout,
    collections::{BTreeMap, HashSet},
    mem::{align_of, needs_drop, size_of},
    sync::{Arc, atomic::AtomicUsize},
};

use bitrouter_ai::{
    core::evidence::{ModelTokenLimits, NativeRouteConstraints},
    routing::PolicyModelTarget,
    tenancy::{
        configuration::{Configuration, GenerationDefaults, Learning, Limits, ResourceRef},
        operation::ErrorCode,
    },
    types::{ApiProtocol, ReasoningEffortConfig},
};
use serde_json::{Map, Value};

use super::native_v1::{NativeChain, NativeChains, Unavailable};
use super::{Candidate, Chain, RetainedExecutionRevision, RetainedPolicy, RouteData};
use crate::{
    config::AdequacyConfig,
    gateway::{
        pipeline::{routing::PromptOverrides, types::RoutingTarget},
        routing::{
            exploration::{
                OptimizationGate, PolicyOptimizationState, RouteExploration, RouteRejection,
            },
            workflow::predictive::PredictorContract,
        },
    },
    policy_lock::{
        CompilerIdentity, EconomicsSummary, LatencySummary, PolicyArtifact, PolicyCertificate,
        PolicyDefinition, PolicyLock, QualitySummary,
    },
    trajectory::guard::ProgressGuardPolicy,
};

#[cfg(test)]
mod tests;

// Bounds this accounting operation itself, independently of the compiler's
// plaintext/target/reference limits. No unbounded recursive JSON traversal.
const MAX_VISITS: usize = 1_000_000;
const MAX_JSON_DEPTH: usize = 64;

type Result<T> = std::result::Result<T, ErrorCode>;

pub(super) fn retained_heap_bytes(value: &RetainedExecutionRevision) -> Result<usize> {
    let RetainedExecutionRevision {
        installation,
        tenant,
        revision,
        authority_revision: _,
        document,
        routes,
    } = value;
    let mut weight = Weight::new();
    weight.add(installation.as_str().len())?;
    weight.add(tenant.as_str().len())?;
    weight.add(revision.as_str().len())?;
    weight.configuration(document)?;
    weight.hash_table::<(String, Arc<RouteData>)>(routes.capacity())?;
    weight.visits(routes.len())?;
    for (name, route) in routes {
        weight.string(name)?;
        if weight.arc(route, 0)? {
            weight.route(route)?;
        }
    }
    Ok(weight.bytes)
}

struct Weight {
    bytes: usize,
    remaining: usize,
    // Type tags separate RouteData, PolicyLock and Value allocation identities.
    // This temporary bookkeeping is not part of the cached object graph.
    seen: HashSet<(u8, *const ())>,
}

impl Weight {
    fn new() -> Self {
        Self {
            bytes: 0,
            remaining: MAX_VISITS,
            seen: HashSet::new(),
        }
    }

    fn add(&mut self, bytes: usize) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or(ErrorCode::CapacityExceeded)?;
        Ok(())
    }

    fn visits(&mut self, count: usize) -> Result<()> {
        self.remaining = self
            .remaining
            .checked_sub(count)
            .ok_or(ErrorCode::CapacityExceeded)?;
        Ok(())
    }

    fn array<T>(&mut self, capacity: usize) -> Result<()> {
        self.add(product(capacity, size_of::<T>())?)
    }

    fn vector<T>(&mut self, values: &Vec<T>) -> Result<()> {
        // Raw bytes and inline enums require O(1) accounting; their lengths
        // must not consume the recursive-work budget. Element walkers charge
        // each visited object separately.
        self.visits(1)?;
        self.array::<T>(values.capacity())
    }

    fn string(&mut self, value: &String) -> Result<()> {
        self.visits(1)?;
        self.add(value.capacity())
    }

    fn optional_string(&mut self, value: &Option<String>) -> Result<()> {
        if let Some(value) = value {
            self.string(value)?;
        }
        Ok(())
    }

    fn arc<T>(&mut self, value: &Arc<T>, tag: u8) -> Result<bool> {
        self.visits(1)?;
        if !self.seen.insert((tag, Arc::as_ptr(value).cast())) {
            return Ok(false);
        }
        // ArcInner is repr(C): two reference counts followed by T, including
        // alignment before T and tail padding. The outer/root Arc is excluded.
        let (layout, _) = Layout::new::<[AtomicUsize; 2]>()
            .extend(Layout::new::<T>())
            .map_err(|_| ErrorCode::CapacityExceeded)?;
        self.add(layout.pad_to_align().size())?;
        Ok(true)
    }

    fn hash_table<T>(&mut self, capacity: usize) -> Result<()> {
        if capacity == 0 {
            return Ok(());
        }
        // For the insert-only Swiss tables constructed by this decoder, the
        // bucket count is <= 2 * reported capacity (including small tables).
        // Charge every bucket's T, one control byte/bucket, the maximum audited
        // 16-byte SIMD control suffix, and worst-case control alignment padding.
        // This is not a bound for arbitrary tombstone-heavy external tables.
        let buckets = product(capacity, 2)?;
        self.array::<T>(buckets)?;
        self.add(buckets)?;
        self.add(16)?;
        self.add(align_of::<T>().max(16) - 1)
    }

    fn tree<K, V>(&mut self, values: &BTreeMap<K, V>) -> Result<()> {
        self.tree_storage::<K, V>(values.len())
    }

    fn tree_storage<K, V>(&mut self, len: usize) -> Result<()> {
        self.visits(len)?;
        // alloc BTree: a node has 11 K slots, 11 V slots, a parent pointer,
        // two u16s, and (for internal nodes) 12 child pointers. Every nonempty
        // node owns >=1 entry; len+1 also covers an allocated empty root. Charge
        // all nodes as internal, plus alignment for each field and the tail.
        let nodes = len.checked_add(1).ok_or(ErrorCode::CapacityExceeded)?;
        let slots = product(nodes, 11)?;
        self.array::<K>(slots)?;
        self.array::<V>(slots)?;
        self.array::<usize>(product(nodes, 13)?)?;
        self.array::<u16>(product(nodes, 2)?)?;
        let alignment = align_of::<K>()
            .max(align_of::<V>())
            .max(align_of::<usize>());
        self.add(product(product(nodes, 7)?, alignment)?)
    }

    fn json_map(&mut self, values: &Map<String, Value>, depth: usize) -> Result<()> {
        self.visits(values.len())?;
        if !values.is_empty() {
            // Fresh serde_json maps insert into IndexMap without removing keys.
            // IndexMap's entries Vec and Swiss index table grow geometrically;
            // 4*next_power_of_two(len) covers their small-capacity floors and
            // rounding independently. Each entry owns (hash, key, value).
            // Charge both storage arrays, even though capacity() is opaque here.
            let slots = product(
                values
                    .len()
                    .checked_next_power_of_two()
                    .ok_or(ErrorCode::CapacityExceeded)?,
                4,
            )?;
            self.array::<(usize, String, Value)>(slots)?;
            // Bucket is a private Rust-layout struct. The tuple covers its
            // three fields; also allow padding before each field and at the
            // tail rather than assuming its field order matches the tuple.
            let alignment = align_of::<(usize, String, Value)>();
            self.add(product(product(slots, 4)?, alignment - 1)?)?;
            self.hash_table::<usize>(slots)?;
        }
        for (key, value) in values {
            self.string(key)?;
            self.json(value, depth)?;
        }
        Ok(())
    }

    fn json(&mut self, value: &Value, depth: usize) -> Result<()> {
        self.visits(1)?;
        if depth > MAX_JSON_DEPTH {
            return Err(ErrorCode::CapacityExceeded);
        }
        match value {
            Value::Null | Value::Bool(_) => Ok(()),
            Value::Number(_) => {
                // The qualified serde_json Number is an inline numeric enum.
                // Fail closed if feature unification enables arbitrary_precision
                // (String-backed Number) instead of guessing its private capacity.
                if needs_drop::<serde_json::Number>() {
                    Err(ErrorCode::AuthorityUnavailable)
                } else {
                    Ok(())
                }
            }
            Value::String(value) => self.string(value),
            Value::Array(values) => {
                self.vector(values)?;
                for value in values {
                    self.json(value, depth + 1)?;
                }
                Ok(())
            }
            Value::Object(values) => self.json_map(values, depth + 1),
        }
    }

    fn reference(&mut self, value: &ResourceRef) -> Result<()> {
        let ResourceRef {
            owner_kind: _,
            owner_id,
            resource_kind: _,
            resource_id,
            revision_id,
        } = value;
        self.visits(1)?;
        self.add(owner_id.as_str().len())?;
        self.add(resource_id.as_str().len())?;
        self.add(revision_id.as_str().len())
    }

    fn configuration(&mut self, value: &Configuration) -> Result<()> {
        let Configuration {
            routes,
            policy,
            generation_defaults,
            learning,
            limits,
        } = value;
        let GenerationDefaults {
            max_output_tokens: _,
            temperature: _,
        } = generation_defaults;
        let Learning {
            mode: _,
            max_evaluations: _,
        } = learning;
        let Limits {
            active_executions: _,
            queued_work: _,
            learning_jobs: _,
        } = limits;
        self.vector(routes)?;
        for reference in routes {
            self.reference(reference)?;
        }
        if let Some(reference) = policy {
            self.reference(reference)?;
        }
        Ok(())
    }

    fn block(&mut self, value: &super::super::block::Profile) -> Result<()> {
        use crate::gateway::routing::blocks::{
            allocation::BanditConfig,
            control::{BlockDefinition, BlockRule},
        };
        let super::super::block::Profile {
            profile: _,
            definition,
        } = value;
        self.add(size_of::<super::super::block::Profile>())?;
        let BlockDefinition {
            block_id,
            source,
            rationale,
            rules,
            independence_rationale,
            dependencies,
            measurement_contract,
            batch_sessions: _,
            bandit,
        } = definition;
        for value in [
            block_id,
            source,
            rationale,
            independence_rationale,
            measurement_contract,
        ] {
            self.string(value)?;
        }
        self.vector(rules)?;
        for BlockRule {
            selector,
            fingerprint,
            baseline_route,
            challenger_route,
        } in rules
        {
            self.string(selector)?;
            self.optional_string(fingerprint)?;
            self.string(baseline_route)?;
            self.string(challenger_route)?;
        }
        self.tree(dependencies)?;
        for (key, value) in dependencies {
            self.string(key)?;
            self.string(value)?;
        }
        let BanditConfig {
            prior_version,
            baseline,
            challenger,
            minimum_quality_ppm: _,
            noninferiority_margin_ppm: _,
            maximum_latency_increase_ppm: _,
            promotion_probability_ppm: _,
            minimum_families_per_arm: _,
            initial_exposure_ppm: _,
            maximum_exposure_ppm: _,
            minimum_exposure_ppm: _,
            maximum_pending_challenger: _,
            maximum_challenger_sessions: _,
            posterior_draws: _,
            recent_guard_families: _,
            recent_guard_minimum_families: _,
        } = bandit;
        self.string(prior_version)?;
        // Both prior trees are inline scalar structures, already charged by `Box<Profile>`.
        for prior in [baseline, challenger] {
            let crate::gateway::routing::blocks::allocation::ArmPrior {
                quality,
                log_cost,
                log_latency,
            } = prior;
            for value in [quality, log_cost, log_latency] {
                let crate::gateway::routing::blocks::allocation::MeanPrior {
                    mean: _,
                    strength: _,
                    observation_variance: _,
                } = value;
            }
        }
        Ok(())
    }

    // The options are inline scalars. Exhaustive shape checks make new heap fields visible.
    fn price_bracket(value: &crate::gateway::pipeline::stream::UsagePricingBracket) {
        let crate::gateway::pipeline::stream::UsagePricingBracket {
            input_micro_usd_per_token: _,
            cache_read_micro_usd_per_token: _,
            cache_write_micro_usd_per_token: _,
            output_micro_usd_per_token: _,
            reasoning_output_micro_usd_per_token: _,
        } = value;
    }

    fn route(&mut self, value: &RouteData) -> Result<()> {
        let RouteData {
            reference,
            assessment,
            block,
            block_pricing,
            dependencies,
            selector,
            overrides,
            policy,
            chains,
            native,
            candidates,
            choice,
        } = value;
        if let Some(choice) = choice {
            self.string(&choice.digest)?;
            // DecisionPolicy has only inline scalar fields. Shared child data
            // and its descendants are charged once per retained root.
            if self.arc(&choice.route, 0)? {
                self.route(&choice.route)?;
            }
        }
        if let Some(block) = block {
            self.block(block)?;
        }
        self.vector(block_pricing)?;
        for pricing in block_pricing.iter().flatten() {
            let crate::gateway::pipeline::stream::UsagePricing {
                registry_price,
                base,
                context_tiers,
            } = pricing;
            // Registry JSON has a different heap representation and is not
            // admitted by compiler-v1. Never retain it without accounting.
            if registry_price.is_some() {
                return Err(ErrorCode::AuthorityUnavailable);
            }
            Self::price_bracket(base);
            self.vector(context_tiers)?;
            for crate::gateway::pipeline::stream::UsagePricingTier {
                above_input_tokens: _,
                bracket,
            } in context_tiers
            {
                Self::price_bracket(bracket);
            }
        }
        self.reference(reference)?;
        if let Some(assessment) = assessment {
            self.reference(assessment)?;
        }
        self.vector(dependencies)?;
        for reference in dependencies {
            self.reference(reference)?;
        }
        self.string(selector)?;
        let PromptOverrides {
            system_prompt,
            params,
        } = overrides;
        self.optional_string(system_prompt)?;
        self.json_map(params, 0)?;
        if let Some(policy) = policy {
            self.retained_policy(policy)?;
        }
        self.vector(chains)?;
        for Chain {
            selector,
            inbound,
            candidates,
        } in chains
        {
            self.string(selector)?;
            self.protocol(inbound)?;
            self.vector(candidates)?;
        }
        match native {
            NativeChains::Ready(chains) => {
                self.vector(chains)?;
                for NativeChain {
                    selector,
                    operation: _,
                    candidates,
                } in chains
                {
                    self.string(selector)?;
                    self.vector(candidates)?;
                }
            }
            NativeChains::Unavailable(Unavailable::Unsupported | Unavailable::Capacity) => {}
        }
        self.vector(candidates)?;
        for Candidate {
            target,
            projection,
            credential,
        } in candidates
        {
            self.target(target)?;
            self.vector(projection)?;
            self.reference(credential)?;
        }
        Ok(())
    }

    fn retained_policy(&mut self, value: &RetainedPolicy) -> Result<()> {
        let RetainedPolicy {
            reference,
            name,
            document,
            routing_source,
        } = value;
        self.reference(reference)?;
        self.string(name)?;
        if self.arc(document, 1)? {
            self.policy(document)?;
        }
        if self.arc(routing_source, 2)? {
            self.json(routing_source, 0)?;
        }
        Ok(())
    }

    fn protocol(&mut self, value: &ApiProtocol) -> Result<()> {
        match value {
            ApiProtocol::ChatCompletions
            | ApiProtocol::Messages
            | ApiProtocol::Responses
            | ApiProtocol::Decisions => Ok(()),
            ApiProtocol::Custom(name) => self.string(name),
        }
    }

    fn target(&mut self, value: &RoutingTarget) -> Result<()> {
        let RoutingTarget {
            registry,
            provider_name,
            service_id,
            api_base,
            api_key,
            api_protocol,
            chat_token_limit_field: _,
            chat_supports_store: _,
            chat_supports_stream_options: _,
            chat_google_extensions: _,
            reasoning_effort,
            model_constraints,
            account_label,
            api_key_override,
            api_base_override,
            auth_scheme: _,
            headers,
        } = value;
        // No SelectedInterface Arc is allowed to escape the owned codec.
        if registry.is_some() {
            return Err(ErrorCode::AuthorityUnavailable);
        }
        for value in [provider_name, service_id, api_base, api_key] {
            self.string(value)?;
        }
        self.protocol(api_protocol)?;
        for value in [account_label, api_key_override, api_base_override] {
            self.optional_string(value)?;
        }
        if let Some(ReasoningEffortConfig { levels, default: _ }) = reasoning_effort {
            self.vector(levels)?;
        }
        let NativeRouteConstraints {
            capabilities,
            token_limits,
            input_token_counting: _,
            source,
        } = model_constraints;
        let ModelTokenLimits {
            max_input_tokens: _,
            max_output_tokens: _,
            context_window: _,
        } = token_limits;
        self.vector(capabilities)?;
        self.optional_string(source)?;
        self.vector(headers)?;
        for header in headers {
            let (name, default, _passthrough) = header.retained_heap_parts();
            // Retained headers only come from OutboundHeaderRule::new: http
            // copies name/value bytes into exactly sized buffers. Count even
            // static names and shared clones in full. bytes::Bytes uses a
            // 3-word Shared; the uppercase-name BytesMut path can retain its
            // 5-word Shared. Five words per buffer conservatively cover both.
            self.add(name.as_str().len())?;
            self.array::<usize>(5)?;
            if let Some(value) = default {
                self.add(value.as_bytes().len())?;
                self.array::<usize>(5)?;
            }
        }
        Ok(())
    }

    fn policy(&mut self, value: &PolicyLock) -> Result<()> {
        let PolicyLock {
            lockfile_version: _,
            artifact,
            policies,
            certificates,
        } = value;
        if let Some(artifact) = artifact {
            self.artifact(artifact)?;
        }
        self.tree(policies)?;
        for (name, policy) in policies {
            self.string(name)?;
            self.definition(policy)?;
        }
        self.tree(certificates)?;
        for (name, certificates) in certificates {
            self.string(name)?;
            self.tree(certificates)?;
            for (name, certificate) in certificates {
                self.string(name)?;
                self.certificate(certificate)?;
            }
        }
        Ok(())
    }

    fn artifact(&mut self, value: &PolicyArtifact) -> Result<()> {
        let PolicyArtifact {
            parent_digest,
            evidence_root,
            eval_snapshot_root,
            source_snapshot_time_unix_ms: _,
            compiler,
        } = value;
        let CompilerIdentity {
            id,
            version: _,
            config_digest,
        } = compiler;
        self.optional_string(parent_digest)?;
        self.string(evidence_root)?;
        self.optional_string(eval_snapshot_root)?;
        self.string(id)?;
        self.string(config_digest)
    }

    fn definition(&mut self, value: &PolicyDefinition) -> Result<()> {
        let PolicyDefinition {
            key_strategy: _,
            tiers,
            routes,
            default_tier,
            tool_use_tier,
            tool_safe_tiers,
            progress_guard,
            predictor,
            optimization,
            adequacy,
        } = value;
        self.tree(tiers)?;
        for (name, target) in tiers {
            let PolicyModelTarget {
                model,
                effort: _,
                context: _,
            } = target;
            self.string(name)?;
            self.string(model)?;
        }
        self.tree(routes)?;
        for (key, tier) in routes {
            self.string(key)?;
            self.string(tier)?;
        }
        self.optional_string(default_tier)?;
        self.optional_string(tool_use_tier)?;
        self.vector(tool_safe_tiers)?;
        for tier in tool_safe_tiers {
            self.string(tier)?;
        }
        if let Some(guard) = progress_guard {
            self.guard(guard)?;
        }
        if let Some(predictor) = predictor {
            let PredictorContract {
                algorithm,
                version: _,
                config_digest,
                confidence_kind,
                calibration_digest,
            } = predictor;
            for value in [algorithm, config_digest, confidence_kind] {
                self.string(value)?;
            }
            self.optional_string(calibration_digest)?;
        }
        if let Some(optimization) = optimization {
            self.optimization(optimization)?;
        }
        self.adequacy(adequacy)
    }

    fn guard(&mut self, value: &ProgressGuardPolicy) -> Result<()> {
        let ProgressGuardPolicy {
            escalation_tier,
            protected_tiers,
            max_consecutive_unprotected: _,
            max_same_projection_unprotected: _,
            max_recovery_count: _,
            max_episode_requests: _,
            max_episode_elapsed_ms: _,
            max_episode_cost_micro_usd: _,
            hold_for_requests: _,
            incomplete_history: _,
        } = value;
        self.string(escalation_tier)?;
        self.tree_storage::<String, ()>(protected_tiers.len())?;
        for tier in protected_tiers {
            self.string(tier)?;
        }
        Ok(())
    }

    fn adequacy(&mut self, value: &AdequacyConfig) -> Result<()> {
        let AdequacyConfig {
            enabled: _,
            escalation_tier,
            escalation_threshold: _,
            pin_cooldown_secs: _,
            reliability_window_size: _,
            reliability_consecutive_failures: _,
            reliability_error_rate_percent: _,
            reliability_cooldown_secs: _,
            explore_enabled: _,
            explore_tier,
            explore_interval: _,
            explore_threshold: _,
            max_downgraded_requests_per_session: _,
            min_semantic_successes_for_lock: _,
            explore_opening: _,
            min_semantic_successes_for_opening: _,
        } = value;
        self.optional_string(escalation_tier)?;
        self.optional_string(explore_tier)
    }

    fn optimization(&mut self, value: &PolicyOptimizationState) -> Result<()> {
        let PolicyOptimizationState { active, rejections } = value;
        if let Some(active) = active {
            self.exploration(active)?;
        }
        self.vector(rejections)?;
        for RouteRejection {
            experiment_id,
            target_request_key,
            treatment_context_digest,
            treatment,
            experiment_parent_digest,
            source_policy_digest,
            evidence_root,
            reason,
        } in rejections
        {
            for value in [experiment_id, evidence_root, reason] {
                self.string(value)?;
            }
            for value in [
                target_request_key,
                treatment_context_digest,
                experiment_parent_digest,
                source_policy_digest,
            ] {
                self.optional_string(value)?;
            }
            if let Some(treatment) = treatment {
                self.exploration(treatment)?;
            }
        }
        Ok(())
    }

    fn exploration(&mut self, value: &RouteExploration) -> Result<()> {
        let RouteExploration {
            experiment_id,
            target_request_key,
            champion_tier,
            challenger_tier,
            challenger_exposure_ppm: _,
            gate,
        } = value;
        let OptimizationGate {
            objective: _,
            minimum_tasks_per_arm: _,
            maximum_challenger_tasks: _,
            minimum_pass_rate_ppm: _,
            evaluator_config_digest,
        } = gate;
        for value in [
            experiment_id,
            target_request_key,
            champion_tier,
            challenger_tier,
        ] {
            self.string(value)?;
        }
        self.optional_string(evaluator_config_digest)
    }

    fn certificate(&mut self, value: &PolicyCertificate) -> Result<()> {
        let PolicyCertificate {
            classifier_digest,
            owner: _,
            selected_tier,
            baseline_tier,
            source: _,
            eligible_episodes: _,
            independent_tasks: _,
            quality,
            economics,
            latency,
            critical_violations: _,
            verdict: _,
            evaluator_config_digest,
            compiler_config_digest,
            evidence_digest,
        } = value;
        let (None
        | Some(QualitySummary {
            baseline_pass_rate_ppm: _,
            candidate_pass_rate_ppm: _,
            delta_ppm: _,
            lower_bound_ppm: _,
        })) = quality;
        let (None
        | Some(EconomicsSummary {
            normalized_cost_delta_ppm: _,
        })) = economics;
        let (None
        | Some(LatencySummary {
            normalized_latency_delta_ppm: _,
        })) = latency;
        for value in [classifier_digest, baseline_tier, evaluator_config_digest] {
            self.optional_string(value)?;
        }
        for value in [selected_tier, compiler_config_digest, evidence_digest] {
            self.string(value)?;
        }
        Ok(())
    }
}

fn product(left: usize, right: usize) -> Result<usize> {
    left.checked_mul(right).ok_or(ErrorCode::CapacityExceeded)
}
