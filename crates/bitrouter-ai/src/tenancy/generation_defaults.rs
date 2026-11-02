//! Closed request-local defaults, bound before ordered submission. These are
//! canonical values, not source-protocol extras or permission to use a feature.

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest as _, Sha256};
use std::io::Write;

use super::{
    identity::Digest,
    validation::{Validate, ValidationError, bytes, require},
};
use crate::types::ReasoningEffort;

/// Reserved ordered request member, removed before protocol conversion.
pub const BODY_MEMBER: &str = "bitrouter_generation_defaults";
/// The existing tenant ingress ceiling applies to the entire request body.
/// This individual value cannot be larger; enclosing request bytes count too.
pub const MAX_DEFAULTS_BYTES: usize = 16 * 1024 * 1024;

/// Canonical preset defaults for one ordered request, without arbitrary fields.
///
/// Typed caller values (including zero) win. Typed null becomes None and permits
/// a default; an empty stop vector also permits a default. A source-owned opaque
/// extra with a competing canonical name blocks promotion without itself being
/// promoted across protocols. Unsupported outbound fields keep their existing
/// adapter behavior. Configuration defaults remain a separate retained object.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct CanonicalGenerationDefaultsV1 {
    /// Format, exactly one.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 1)))]
    pub schema_version: u8,
    /// Finite temperature; absence supplies no default.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[cfg_attr(feature = "schema", schemars(with = "f64"))]
    pub temperature: Option<f64>,
    /// Finite nucleus-sampling value.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[cfg_attr(feature = "schema", schemars(with = "f64"))]
    pub top_p: Option<f64>,
    /// Output-token default, including zero.
    #[serde(
        default,
        deserialize_with = "present_u32",
        skip_serializing_if = "Option::is_none"
    )]
    #[cfg_attr(feature = "schema", schemars(with = "u32"))]
    #[cfg_attr(feature = "schema", schemars(extend("maximum" = u32::MAX)))]
    pub max_tokens: Option<u32>,
    /// Existing canonical effort level; this is not a policy treatment.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[cfg_attr(feature = "schema", schemars(with = "ReasoningEffort"))]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Top-k default, rendered only by supporting protocol adapters.
    #[serde(
        default,
        deserialize_with = "present_u32",
        skip_serializing_if = "Option::is_none"
    )]
    #[cfg_attr(feature = "schema", schemars(with = "u32"))]
    #[cfg_attr(feature = "schema", schemars(extend("maximum" = u32::MAX)))]
    pub top_k: Option<u32>,
    /// Signed deterministic-sampling seed, including zero.
    #[serde(
        default,
        deserialize_with = "present_seed",
        skip_serializing_if = "Option::is_none"
    )]
    #[cfg_attr(feature = "schema", schemars(with = "i64"))]
    #[cfg_attr(feature = "schema", schemars(extend("minimum" = i64::MIN, "maximum" = i64::MAX)))]
    pub seed: Option<i64>,
    /// Stop sequences. A single input string normalizes to a one-element vector.
    #[serde(
        default,
        deserialize_with = "stop",
        skip_serializing_if = "Option::is_none"
    )]
    #[cfg_attr(feature = "schema", schemars(with = "StopInput"))]
    pub stop: Option<Vec<String>>,
    /// Finite presence-penalty default.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[cfg_attr(feature = "schema", schemars(with = "f64"))]
    pub presence_penalty: Option<f64>,
    /// Finite frequency-penalty default.
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    #[cfg_attr(feature = "schema", schemars(with = "f64"))]
    pub frequency_penalty: Option<f64>,
}

impl std::fmt::Debug for CanonicalGenerationDefaultsV1 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CanonicalGenerationDefaultsV1")
            .finish_non_exhaustive()
    }
}

fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    // Missing is handled by default; an explicit null preset is invalid.
    T::deserialize(d).map(Some)
}

fn present_u32<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    super::number::integer(d).map(Some)
}

fn present_seed<'de, D: Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    super::number::signed_integer(d).map(Some)
}

#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
enum StopInput {
    One(String),
    Many(Vec<String>),
}

fn stop<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<String>>, D::Error> {
    Ok(Some(match StopInput::deserialize(d)? {
        StopInput::One(value) => vec![value],
        StopInput::Many(values) => values,
    }))
}

impl Validate for CanonicalGenerationDefaultsV1 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(self.schema_version == 1, "canonical defaults version")?;
        require(
            [
                self.temperature,
                self.top_p,
                self.presence_penalty,
                self.frequency_penalty,
            ]
            .into_iter()
            .flatten()
            .all(f64::is_finite),
            "finite canonical defaults",
        )?;
        bytes(self, MAX_DEFAULTS_BYTES)
    }
}

impl CanonicalGenerationDefaultsV1 {
    /// SHA-256 of the domain `bitrouter.canonical-generation-defaults.v1\0`
    /// followed by this type's compact Serde JSON in declared field order.
    /// Absent fields are omitted and a single stop string is normalized to an
    /// array. This binds requested defaults, not source-wire bytes, the entire
    /// input, effective provider settings, or measured usage.
    ///
    /// # Errors
    /// Rejects invalid or oversized defaults before computing an identity.
    pub fn digest(&self) -> Result<Digest, ValidationError> {
        self.validate()?;
        struct Sink(Sha256);
        impl Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.update(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut sink = Sink(Sha256::new());
        sink.0
            .update(b"bitrouter.canonical-generation-defaults.v1\0");
        require(
            serde_json::to_writer(&mut sink, self).is_ok(),
            "canonical defaults serialization",
        )?;
        sink.0
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
            .try_into()
    }
}
