//! Explicit provider-native request controls, without historical ownership.
//!
//! Current resource grants still govern every attempt. An opaque provider
//! reference neither proves its creator nor selects a route or credential.

use serde::{Deserialize, Serialize};

use super::{
    identity::{AuthorityRevision, InstallationId, InstanceId, RevisionId},
    validation::{Validate, ValidationError, require},
};

/// Explicit ordered-request opt-in; consumed before provider dispatch.
pub const HEADER: &str = "bitrouter-native-controls";
/// Versioned provider-native mode. It never resolves managed public handles.
pub const PROVIDER_NATIVE_V1: &str = "provider_native_v1";

/// A protocol-owned opaque request field supported by this contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum NativeControlV1 {
    /// A Responses provider's opaque prior response reference.
    #[serde(rename = "responses.previous_response_id")]
    ResponsesPreviousResponseId,
    /// A Responses provider's native conversation value.
    #[serde(rename = "responses.conversation")]
    ResponsesConversation,
    /// Native Messages context management, without cross-protocol conversion.
    #[serde(rename = "messages.context_management")]
    MessagesContextManagement,
}

/// Exact supported field set, independent of whether the operator enables it.
pub const CONTROLS: [NativeControlV1; 3] = [
    NativeControlV1::ResponsesPreviousResponseId,
    NativeControlV1::ResponsesConversation,
    NativeControlV1::MessagesContextManagement,
];

/// Separate discovery preserving the ordinary ordered-v2 capabilities format.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct OrderedNativeControlsCapabilitiesV1 {
    /// Discovery version, exactly one.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 1)))]
    pub contract_version: u32,
    /// Persistent authority installation.
    pub installation_id: InstallationId,
    /// Actual process incarnation; a restart requires a fresh observation.
    pub instance_id: InstanceId,
    /// Current dispatch capability binding.
    pub capability_revision: RevisionId,
    /// Authority observed during discovery, not a dispatch lease.
    pub authority_revision: AuthorityRevision,
    /// Operator opt-in and current ordered dispatch readiness both hold.
    pub enabled: bool,
    /// Exactly the three protocol-owned fields supported by version one.
    #[cfg_attr(feature = "schema", schemars(extend("const" = serde_json::json!([
        "responses.previous_response_id", "responses.conversation", "messages.context_management"
    ]))))]
    pub controls: [NativeControlV1; 3],
}

impl Validate for OrderedNativeControlsCapabilitiesV1 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            self.contract_version == 1 && self.controls == CONTROLS,
            "ordered native controls capabilities",
        )
    }
}
