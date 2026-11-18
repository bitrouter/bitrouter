//! Compile the public source into a private, model-selection-only execution view.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use bitrouter_ai::policy::{Document, Target};
use sha2::{Digest, Sha256};

use crate::gateway::routing::workflow::predictive::{
    PredictiveRouteProjection, compiled_predictor_contract,
};

const MAX_BYTES: usize = 1024 * 1024;

/// Validated source and its execution indexes. This is never a serialized format.
#[derive(Debug, Clone)]
pub(crate) struct CompiledPolicy {
    #[cfg(any(feature = "multi-instance", test))]
    source: Document,
    source_digest: String,
    selections: BTreeMap<String, CompiledSelection>,
}

/// Model targets and classifier indexes contain no context or learning actions.
#[derive(Debug, Clone)]
pub(crate) struct CompiledSelection {
    pub(crate) tiers: BTreeMap<String, Target>,
    pub(crate) routes: BTreeMap<String, String>,
    pub(crate) default_tier: String,
    pub(crate) tool_use_tier: Option<String>,
    pub(crate) tool_safe_tiers: Vec<String>,
}

impl CompiledPolicy {
    #[cfg(any(feature = "multi-instance", test))]
    pub(crate) fn source(&self) -> &Document {
        &self.source
    }

    pub(crate) fn digest(&self) -> &str {
        &self.source_digest
    }

    pub(crate) fn selections(&self) -> &BTreeMap<String, CompiledSelection> {
        &self.selections
    }
}

/// Validate an already-decoded current source. Raw ingress must decode directly
/// into Document so duplicate keys cannot disappear inside a generic Value.
#[cfg(test)]
pub(crate) fn parse(value: &serde_json::Value) -> Result<Document> {
    let source: Document =
        serde_json::from_value(value.clone()).context("invalid model policy contract")?;
    compile(&source)?;
    Ok(source)
}

/// Strict raw source decoding shared by files and retained history.
pub(crate) fn decode(raw: &str) -> Result<Document> {
    ensure!(raw.len() <= MAX_BYTES, "policy byte capacity exceeded");
    let source: Document = serde_saphyr::from_str(raw).context("invalid model policy source")?;
    compile(&source)?;
    Ok(source)
}

/// Canonical source identity, independent of file presentation or resource revision.
pub(crate) fn source_digest(source: &Document) -> Result<String> {
    let mut value = serde_json::to_value(source).context("normalizing policy source")?;
    // preserve_order is enabled for service JSON. Sort every object explicitly,
    // including target fields, without changing array order or typed omissions.
    value.sort_all_objects();
    let canonical = serde_json::to_vec(&value).context("serializing canonical policy source")?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(canonical))))
}

/// Compile immutable source without filesystem, provider, session or learner access.
pub(crate) fn compile(source: &Document) -> Result<CompiledPolicy> {
    ensure!(
        serde_json::to_vec(source)?.len() <= MAX_BYTES,
        "policy byte capacity exceeded"
    );
    ensure!(
        !source.policies.is_empty() && source.policies.len() <= 256,
        "policy count must be 1..=256"
    );
    let predictor = compiled_predictor_contract();
    let mut selections = BTreeMap::new();
    for (name, policy) in &source.policies {
        validate_name(name)?;
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
        let mut targets = BTreeSet::new();
        for (tier, target) in &policy.tiers {
            ensure!(
                !tier.is_empty() && tier.len() <= 256,
                "invalid policy tier name"
            );
            ensure!(
                !target.model.trim().is_empty() && target.model.len() <= 512,
                "invalid policy model selector"
            );
            ensure!(
                !target.model.starts_with('@')
                    && !crate::config::resolution::is_reserved(&target.model)
                    && !matches!(target.model.as_str(), "bitrouter:auto" | "bitrouter:fusion"),
                "policy targets cannot refer to a router or preset"
            );
            ensure!(
                targets.insert(serde_json::to_string(target)?),
                "duplicate policy model/effort target"
            );
        }
        if let Some(tier) = &policy.tool_use_tier {
            ensure!(
                policy.tiers.contains_key(tier),
                "policy tool fallback tier must exist"
            );
            ensure!(
                policy.tool_safe_tiers.contains(tier),
                "policy tool fallback must be tool-safe"
            );
        }
        for tier in &policy.tool_safe_tiers {
            ensure!(
                policy.tiers.contains_key(tier),
                "policy tool-safe tier must exist"
            );
        }
        let mut routes = BTreeMap::new();
        for (class, tier) in &policy.routes {
            let key = format!("semantic_route/v1|{class}");
            ensure!(
                PredictiveRouteProjection::parse_key(&key).is_some(),
                "invalid policy route class"
            );
            ensure!(
                policy.tiers.contains_key(tier),
                "policy route tier must exist"
            );
            routes.insert(key, tier.clone());
        }
        selections.insert(
            name.clone(),
            CompiledSelection {
                tiers: policy.tiers.clone(),
                routes,
                default_tier: policy.default_tier.clone(),
                tool_use_tier: policy.tool_use_tier.clone(),
                tool_safe_tiers: policy.tool_safe_tiers.clone(),
            },
        );
    }
    Ok(CompiledPolicy {
        #[cfg(any(feature = "multi-instance", test))]
        source: source.clone(),
        source_digest: source_digest(source)?,
        selections,
    })
}

pub(crate) fn validate_name(name: &str) -> Result<()> {
    let segments: Vec<_> = name.split(':').collect();
    ensure!(
        (1..=2).contains(&segments.len())
            && segments.iter().all(|segment| {
                !segment.is_empty()
                    && !segment.starts_with('.')
                    && segment.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
                    })
            }),
        "invalid policy name '{name}'"
    );
    Ok(())
}

#[cfg(test)]
mod tests;
