//! Model-only routing policy contracts. Compilation and selection are service-owned.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The exact supported policy contract, independent of software and resource revisions.
pub const SPEC_VERSION: &str = "policy/1.0.0-beta.1";

/// Closed discriminator: unsupported contracts cannot silently become defaults.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum SpecVersion {
    /// Initial independently versioned model-only policy contract.
    #[serde(rename = "policy/1.0.0-beta.1")]
    Beta1,
}

/// Portable policy source. Tenant identity, activation and revision live in the API envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Document {
    /// Required, exact contract discriminator; never inferred from other fields.
    pub spec_version: SpecVersion,
    /// Named policies selected by explicit router bindings.
    #[serde(deserialize_with = "unique_map")]
    pub policies: BTreeMap<String, Policy>,
}

/// Model selection without context transformations, agent execution or learning state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Named generation targets. A target is a model and optional reasoning effort.
    #[serde(deserialize_with = "unique_map")]
    pub tiers: BTreeMap<String, Target>,
    /// Required fallback when no admitted classification matches.
    pub default_tier: String,
    /// Exact classifier configuration digest; required when routes are nonempty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classifier_digest: Option<String>,
    /// Canonical `task-family|next-step|risk` classification to tier.
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "unique_map"
    )]
    pub routes: BTreeMap<String, String>,
    /// Tool-safe fallback when a tool-bearing request selects an unsafe tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_use_tier: Option<String>,
    /// Tiers permitted for tool-bearing requests when a tool fallback is configured.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "tool_tiers"
    )]
    pub tool_safe_tiers: Vec<String>,
}

/// A generation target. No field can request context rewriting or harness behavior.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Target {
    /// Explicit model selector resolved inside the authorized routing source.
    pub model: String,
    /// Absence preserves the caller's reasoning-effort constraint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<crate::types::ReasoningEffort>,
}

fn unique_map<'de, D, V>(deserializer: D) -> Result<BTreeMap<String, V>, D::Error>
where
    D: serde::Deserializer<'de>,
    V: Deserialize<'de>,
{
    struct Unique<V>(std::marker::PhantomData<V>);
    impl<'de, V: Deserialize<'de>> serde::de::Visitor<'de> for Unique<V> {
        type Value = BTreeMap<String, V>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a mapping with unique keys")
        }
        fn visit_some<D: serde::Deserializer<'de>>(
            self,
            deserializer: D,
        ) -> Result<Self::Value, D::Error> {
            deserializer.deserialize_map(self)
        }
        fn visit_map<M: serde::de::MapAccess<'de>>(
            self,
            mut map: M,
        ) -> Result<Self::Value, M::Error> {
            let mut result = BTreeMap::new();
            while let Some((key, value)) = map.next_entry()? {
                if result.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate policy mapping key"));
                }
            }
            Ok(result)
        }
    }
    // YAML deserializers can coerce a null scalar into an empty map. Only
    // omission may select the field's default.
    deserializer.deserialize_option(Unique(std::marker::PhantomData))
}

fn tool_tiers<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Vec<String>, D::Error> {
    // Likewise, an explicit null is not an empty array.
    Option::<Vec<String>>::deserialize(deserializer)?
        .ok_or_else(|| serde::de::Error::custom("expected a tool-safe tier array, found null"))
}
