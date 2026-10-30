//! Explicit operator-imported ACP Block profile inside the retained route.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::gateway::routing::blocks::control::BlockDefinition;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Profile {
    pub(crate) profile: Version,
    pub(crate) definition: BlockDefinition,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum Version {
    #[serde(rename = "block_v1")]
    V1,
}

impl Profile {
    pub(super) fn validate(&self, selector: &str) -> Result<()> {
        super::manifest::bounded_bytes(self, 64 * 1024)?;
        self.definition.validate()?;
        let definition = &self.definition;
        ensure!(
            definition.rules.len() == 1
                && definition.dependencies.is_empty()
                && definition.block_id.len() <= 128
                && definition.source.len() <= 128
                && definition.measurement_contract.len() <= 256
                && definition.batch_sessions <= 64
                && definition.bandit.maximum_challenger_sessions <= 64
                && definition.bandit.maximum_pending_challenger <= 64
                && definition.bandit.posterior_draws <= 4096,
            "retained Block profile exceeds its bounded static scope"
        );
        let rule = definition.rules.first().context("Block rule missing")?;
        ensure!(
            rule.selector == selector
                && rule.baseline_route == selector
                && rule.challenger_route != selector
                && rule.challenger_route.len() <= 512
                && rule
                    .fingerprint
                    .as_ref()
                    .is_none_or(|value| value.len() <= 256),
            "retained Block must inherit its exact base selector"
        );
        Ok(())
    }

    pub(crate) fn challenger(&self) -> Result<&str> {
        self.definition
            .rules
            .first()
            .map(|rule| rule.challenger_route.as_str())
            .context("Block rule missing")
    }
}

#[cfg(all(feature = "core", feature = "harness"))]
impl super::Resources {
    pub(in crate::tenancy) fn seal_block(
        &self,
        binding: &impl Serialize,
        value: &impl Serialize,
    ) -> Result<crate::runtime_crypto::SealedSecret> {
        let bytes = super::manifest::bounded_bytes(value, super::MAX_ARTIFACT_BYTES)?;
        self.key
            .as_ref()
            .context("Block wrapping key unavailable")?
            .seal_bound(
                "owned-block-record-v1",
                &(&self.installation, binding),
                &bytes,
            )
    }
    pub(in crate::tenancy) fn open_block<T: serde::de::DeserializeOwned>(
        &self,
        binding: &impl Serialize,
        sealed: &crate::runtime_crypto::SealedSecret,
    ) -> Result<T> {
        let bytes = self
            .key
            .as_ref()
            .context("Block wrapping key unavailable")?
            .open_bound(
                "owned-block-record-v1",
                &(&self.installation, binding),
                sealed,
            )?;
        ensure!(
            bytes.len() <= super::MAX_ARTIFACT_BYTES,
            "Block record bound"
        );
        Ok(serde_json::from_slice(&bytes)?)
    }
}
