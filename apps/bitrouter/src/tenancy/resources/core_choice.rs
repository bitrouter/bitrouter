//! Explicit depth-one context Choice composition inside immutable Route artifacts.
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{authority::ResourceKind, configuration::ResourceRef};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

use super::manifest::{Artifact, Definition, bounded_bytes};
use crate::gateway::{
    model::decision::policy::DecisionPolicy, providers::auth::credentials::Credential,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Profile {
    pub profile: Version,
    pub route: ResourceRef,
    pub policy: DecisionPolicy,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
pub(super) enum Version {
    #[serde(rename = "choice_v1")]
    ChoiceV1,
}

impl Profile {
    pub(super) fn validate(&self, parent: &ResourceRef) -> Result<()> {
        self.policy.validate().map_err(anyhow::Error::msg)?;
        ensure!(
            self.route.resource_kind == ResourceKind::Route && self.route != *parent,
            "CoreChoice requires a distinct exact child route"
        );
        ensure!(
            self.policy.max_candidates <= 32
                && self.policy.candidate_excerpt_bytes <= 2048
                && self.policy.max_request_bytes <= 128 * 1024
                && self.policy.target_context_bytes <= 16 * 1024 * 1024
                && self.policy.retain_recent_groups <= 256,
            "CoreChoice policy exceeds the supported profile"
        );
        Ok(())
    }

    pub(super) fn digest(&self) -> Result<String> {
        Ok(hex::encode(Sha256::digest(bounded_bytes(self, 8192)?)))
    }
}

/// Import includes credential definitions; authenticated compiler readback carries
/// only their exact references. Both paths validate the same hidden route graph.
pub(super) fn validate_graph(
    artifacts: &[Artifact],
    exposed: &[ResourceRef],
    credentials: bool,
) -> Result<()> {
    let by_ref: HashMap<_, _> = artifacts
        .iter()
        .map(|artifact| (&artifact.reference, artifact))
        .collect();
    for parent in artifacts {
        let Definition::Route {
            core_choice: Some(profile),
            ..
        } = &parent.definition
        else {
            continue;
        };
        profile.validate(&parent.reference)?;
        ensure!(
            !exposed.contains(&profile.route),
            "CoreChoice child is an exposed root"
        );
        let child = by_ref
            .get(&profile.route)
            .context("CoreChoice child is missing")?;
        let Definition::Route {
            policy: None,
            exploration,
            core_choice: None,
            block: None,
            assessment: None,
            credentials: slots,
            ..
        } = &child.definition
        else {
            anyhow::bail!("CoreChoice child requires an unbound static route");
        };
        ensure!(
            exploration.is_empty(),
            "CoreChoice child exploration is unsupported"
        );
        let envelope = child
            .envelope
            .as_ref()
            .context("CoreChoice child envelope is missing")?;
        ensure!(
            envelope.reference == profile.route
                && envelope.policy.is_none()
                && envelope.chains.len() == 1
                && envelope.chains[0].selector == envelope.selector
                && envelope.chains[0].inbound == bitrouter_ai::types::ApiProtocol::Decisions
                && envelope.chains[0].targets.len() == 1,
            "CoreChoice requires exactly one Decisions target"
        );
        let credential = &envelope.chains[0].targets[0].credential;
        ensure!(
            slots.iter().any(|slot| &slot.credential == credential),
            "CoreChoice credential is outside child bindings"
        );
        if credentials {
            let secret = by_ref
                .get(credential)
                .context("CoreChoice credential is missing")?;
            ensure!(
                matches!(
                    &secret.definition,
                    Definition::Credential {
                        credential: Credential::ApiKey { .. },
                        oauth: None,
                        oauth_generation: 0
                    }
                ),
                "CoreChoice requires a static credential"
            );
        }
    }
    Ok(())
}
