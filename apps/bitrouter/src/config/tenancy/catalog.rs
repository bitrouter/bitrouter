//! File-owned exposure of ordinary configured routes to tenant authority.

use std::collections::BTreeMap;

use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::{
    admission_v2::AdmissionProfileV2,
    identity::{OwnerId, ResourceId},
    validation::Validate,
};
use serde::Deserialize;

/// One installation-wide catalog compiled from the normal process configuration.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CatalogConfig {
    /// Stable operator-pool namespace used by explicit tenant resource grants.
    pub pool_id: String,
    /// Public route identity mapped to an existing models/routers selector.
    pub routes: BTreeMap<String, String>,
    /// Exact provider/account/model rules; accounts may explicitly share a health group.
    #[serde(deserialize_with = "profiles")]
    pub profiles: BTreeMap<String, AdmissionProfileV2>,
}

fn profiles<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, AdmissionProfileV2>, D::Error> {
    // Public profile DTOs deliberately inspect original JSON number tokens.
    // Configuration uses YAML, so cross that encoding boundary here, without
    // weakening the HTTP DTO decoder or introducing another profile model.
    let values = BTreeMap::<String, serde_json::Value>::deserialize(deserializer)?;
    values
        .into_iter()
        .map(|(name, value)| {
            let bytes = serde_json::to_vec(&value).map_err(serde::de::Error::custom)?;
            let profile = serde_json::from_slice(&bytes).map_err(serde::de::Error::custom)?;
            Ok((name, profile))
        })
        .collect()
}

impl CatalogConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        OwnerId::try_from(self.pool_id.as_str())?;
        ensure!(!self.routes.is_empty(), "configured catalog has no routes");
        for (id, selector) in &self.routes {
            ResourceId::try_from(id.as_str())?;
            ensure!(
                !selector.is_empty() && selector.len() <= 512,
                "invalid configured catalog selector"
            );
        }
        let mut health_groups = std::collections::HashMap::new();
        for (source, profile) in &self.profiles {
            let mut identity = source.splitn(3, '/');
            ensure!(
                identity.next().is_some_and(|value| !value.is_empty())
                    && identity.next().is_some_and(|value| !value.is_empty())
                    && identity.next().is_some_and(|value| !value.is_empty()),
                "catalog profile requires provider/account/canonical-model"
            );
            profile.validate()?;
            if let (Some(group), Some(health)) = (&profile.health_group_id, &profile.health)
                && let Some(previous) = health_groups.insert(group, health)
            {
                ensure!(
                    previous == health,
                    "configured health group has conflicting rules"
                );
            }
        }
        Ok(())
    }
}
