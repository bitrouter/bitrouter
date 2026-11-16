//! Compile the public model-only policy source into the current private routing IR.
//!
//! The IR also serves harness experiments. It is never the public Policy format.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::{
    policy::{Document, Policy, SpecVersion},
    routing::{ContextStrategy, PolicyModelTarget},
};

use crate::{
    gateway::routing::workflow::predictive::{
        PredictiveRouteProjection, compiled_predictor_contract,
    },
    policy_lock::{
        CertificateSource, PolicyCertificate, PolicyDefinition, PolicyLock, PromotionVerdict,
        RouteOwner,
    },
};

const MAX_BYTES: usize = 1024 * 1024;
const COMPILER_ID: &str = "bitrouter-model-policy";

pub(crate) fn is_model_only(document: &PolicyLock) -> bool {
    document
        .artifact
        .as_ref()
        .is_some_and(|artifact| artifact.compiler.id == COMPILER_ID)
}

#[cfg(test)]
mod tests;

/// Admit only the exact public contract; old lockfiles are not a fallback.
pub(crate) fn parse(value: &serde_json::Value) -> Result<PolicyLock> {
    let source: Document =
        serde_json::from_value(value.clone()).context("invalid model policy contract")?;
    compile(&source)
}

/// Compile immutable source without filesystem, provider, session or learner access.
pub(crate) fn compile(source: &Document) -> Result<PolicyLock> {
    ensure!(
        serde_json::to_vec(source)?.len() <= MAX_BYTES,
        "policy byte capacity exceeded"
    );
    ensure!(
        !source.policies.is_empty() && source.policies.len() <= 256,
        "policy count must be 1..=256"
    );
    let predictor = compiled_predictor_contract();
    let mut document = PolicyLock::default();
    let artifact = document
        .artifact
        .as_mut()
        .context("policy compiler metadata missing")?;
    artifact.compiler.id = COMPILER_ID.into();
    artifact.compiler.version = 1;
    artifact.compiler.config_digest = format!(
        "sha256:{}",
        hex::encode(Sha256::digest(serde_json::to_vec(source)?))
    );
    for (name, policy) in &source.policies {
        crate::policy_lock::validate_name(name)?;
        ensure!(name.len() <= 256, "policy name capacity exceeded");
        ensure!(
            !policy.tiers.is_empty() && policy.tiers.len() <= 256,
            "policy tier count must be 1..=256"
        );
        ensure!(
            policy.routes.len() <= 4096,
            "policy route capacity exceeded"
        );
        ensure!(
            policy.tool_safe_tiers.len() <= 256,
            "policy tool tier capacity exceeded"
        );
        ensure!(
            policy.tiers.contains_key(&policy.default_tier),
            "policy default tier must exist"
        );
        if policy.routes.is_empty() {
            ensure!(
                policy.classifier_digest.is_none(),
                "classifier digest requires explicit routes"
            );
        } else {
            ensure!(
                policy.classifier_digest.as_deref() == Some(predictor.config_digest.as_str()),
                "unsupported policy classifier digest"
            );
        }
        let mut definition = PolicyDefinition {
            default_tier: Some(policy.default_tier.clone()),
            tool_use_tier: policy.tool_use_tier.clone(),
            tool_safe_tiers: policy.tool_safe_tiers.clone(),
            ..Default::default()
        };
        // Model selection cannot initiate paid adequacy inference or context work.
        definition.adequacy.enabled = false;
        for (tier, target) in &policy.tiers {
            ensure!(
                !tier.is_empty() && tier.len() <= 256,
                "invalid policy tier name"
            );
            ensure!(
                !target.model.trim().is_empty() && target.model.len() <= 512,
                "invalid policy model selector"
            );
            definition.tiers.insert(
                tier.clone(),
                PolicyModelTarget {
                    model: target.model.clone(),
                    effort: target.effort,
                    context: ContextStrategy::Preserve,
                },
            );
        }
        let mut certificates = BTreeMap::new();
        for (class, tier) in &policy.routes {
            let key = format!("semantic_route/v1|{class}");
            ensure!(
                PredictiveRouteProjection::parse_key(&key).is_some(),
                "invalid policy route class"
            );
            definition.routes.insert(key.clone(), tier.clone());
            // These attest an explicit submitted rule, not learned quality evidence.
            certificates.insert(
                key,
                PolicyCertificate {
                    classifier_digest: None,
                    owner: RouteOwner::Operator,
                    selected_tier: tier.clone(),
                    baseline_tier: None,
                    source: CertificateSource::Operator,
                    eligible_episodes: 0,
                    independent_tasks: 0,
                    quality: None,
                    economics: None,
                    latency: None,
                    critical_violations: 0,
                    verdict: PromotionVerdict::Retain,
                    evaluator_config_digest: None,
                    compiler_config_digest: predictor.config_digest.clone(),
                    evidence_digest: document
                        .artifact
                        .as_ref()
                        .context("policy compiler metadata missing")?
                        .evidence_root
                        .clone(),
                },
            );
        }
        if !certificates.is_empty() {
            definition.predictor = Some(predictor.clone());
            document.certificates.insert(name.clone(), certificates);
        }
        document.policies.insert(name.clone(), definition);
    }
    crate::policy_lock::validate_document(&document)?;
    Ok(document)
}

/// Project compatible retained model policies; experimental state is never erased.
pub(crate) fn source(document: &PolicyLock) -> Result<Document> {
    ensure!(
        is_model_only(document),
        "policy was not compiled from the model-only contract"
    );
    let mut policies = BTreeMap::new();
    for (name, policy) in &document.policies {
        ensure!(
            policy.progress_guard.is_none()
                && policy.optimization.is_none()
                && !policy.adequacy.enabled,
            "experimental policy has no model-only source projection"
        );
        let mut tiers = BTreeMap::new();
        for (name, target) in &policy.tiers {
            ensure!(
                target.context == ContextStrategy::Preserve,
                "context policy has no model-only source projection"
            );
            tiers.insert(
                name.clone(),
                bitrouter_ai::policy::Target {
                    model: target.model.clone(),
                    effort: target.effort,
                },
            );
        }
        let routes = policy
            .routes
            .iter()
            .map(|(key, value)| {
                Ok((
                    key.strip_prefix("semantic_route/v1|")
                        .context("unsupported internal route class")?
                        .to_owned(),
                    value.clone(),
                ))
            })
            .collect::<Result<_>>()?;
        policies.insert(
            name.clone(),
            Policy {
                tiers,
                default_tier: policy
                    .default_tier
                    .clone()
                    .context("policy default tier missing")?,
                routes,
                classifier_digest: policy
                    .predictor
                    .as_ref()
                    .map(|value| value.config_digest.clone()),
                tool_use_tier: policy.tool_use_tier.clone(),
                tool_safe_tiers: policy.tool_safe_tiers.clone(),
            },
        );
    }
    let source = Document {
        spec_version: SpecVersion::Beta1,
        policies,
    };
    ensure!(
        compile(&source)? == *document,
        "runtime policy is not fully representable by the public contract"
    );
    Ok(source)
}
