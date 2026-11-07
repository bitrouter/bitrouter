//! Private ordered Generate Content requests under retained protocol authority.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    identity::{AuthorityRevision, InstallationId, InstanceId, RevisionId},
    validation::{Validate, ValidationError, bytes, json_depth, require},
};

/// Dedicated ordered data path. Ordinary public Google endpoints are independent.
pub const PATH: &str = "v1/tenancy/ordered/generate-content";
/// Versioned request envelope, used for both JSON and SSE submissions.
pub const MIME: &str = "application/vnd.bitrouter.ordered-generate-content.v1+json";
/// Canonical native protocol identity used by the private codec.
pub const PROTOCOL: &str = "generate_content";
/// Outbound protocol identity for an operator-paired native endpoint.
pub const BRIDGE_PROTOCOL: &str = "ordered_generate_content";
/// Bound before native protocol decoding or provider rendering.
pub const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;

/// Native body and explicit mode for one retained ordered model root.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderedGenerateContentRequestV1 {
    /// Exactly one.
    #[serde(deserialize_with = "super::number::integer")]
    pub schema_version: u8,
    /// The selected retained route's resource ID.
    pub model: String,
    /// JSON when false, SSE when true. Request query parameters cannot override it.
    pub stream: bool,
    /// Original native fields, excluding model, stream and dispatch metadata.
    #[serde(deserialize_with = "request")]
    pub request: BTreeMap<String, Value>,
}

fn request<'de, D: serde::Deserializer<'de>>(d: D) -> Result<BTreeMap<String, Value>, D::Error> {
    super::json::object(d, 2)
}

impl Validate for OrderedGenerateContentRequestV1 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(self.schema_version == 1, "ordered GenerateContent version")?;
        require(
            !self.model.is_empty() && self.model.len() <= 512,
            "ordered GenerateContent model",
        )?;
        require(
            !self.request.keys().any(|key| {
                matches!(
                    key.as_str(),
                    "model"
                        | "stream"
                        | "bitrouter_generation_defaults"
                        | "bitrouter_generation_defaults_envelope"
                )
            }),
            "competing ordered GenerateContent envelope member",
        )?;
        // Reserve the outer envelope and native request object, preserving the
        // same recursion budget as a transport decode of this exact shape.
        for value in self.request.values() {
            json_depth(value, 2, "ordered GenerateContent nesting")?;
        }
        bytes(self, MAX_REQUEST_BYTES)
    }
}

/// Separate discovery for private native ingress. Every value binds one process.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderedGenerateContentCapabilitiesV1 {
    /// Exactly one.
    #[serde(deserialize_with = "super::number::integer")]
    pub contract_version: u8,
    /// Persistent authority installation.
    pub installation_id: InstallationId,
    /// Current service incarnation.
    pub instance_id: InstanceId,
    /// Current dispatch eligibility revision.
    pub capability_revision: RevisionId,
    /// Current authority observation.
    pub authority_revision: AuthorityRevision,
    /// Ordered authority and an operator-paired adapter are currently available.
    pub enabled: bool,
    /// Native cache references are enabled by current operator policy. Sending
    /// one also requires the independent provider-native request header opt-in.
    pub cached_content_enabled: bool,
    /// Exact relative data path.
    pub path: String,
    /// Exact request media type.
    pub request_media_type: String,
}
impl Validate for OrderedGenerateContentCapabilitiesV1 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            self.contract_version == 1
                && self.path == PATH
                && self.request_media_type == MIME
                && (!self.cached_content_enabled || self.enabled),
            "ordered GenerateContent capabilities",
        )
    }
}

#[cfg(test)]
mod tests;
