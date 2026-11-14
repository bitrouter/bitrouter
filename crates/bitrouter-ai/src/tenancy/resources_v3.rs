//! Explicit native-route provisioning, separate from frozen three-protocol v1.
//! Resource format two remains restricted to shared-health profiles.
use super::{
    authority::{ResourceKey, ResourceKind},
    configuration::ResourceRef,
    identity::{InstallationId, OperationId, RevisionId},
    ordered::{GenerationProtocol, MAX_CANDIDATES},
    resources::CredentialBinding,
    validation::{Validate, ValidationError, bytes, nullable, require, text},
};
use serde::{Deserialize, Serialize, de::Error};
use serde_json::{Value, value::RawValue};

/// Complete four-protocol route attachment bound to exact retained authorities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct NativeAdmissionAttachmentV1 {
    /// Actual ingress protocol, never inferred from a Chat permission.
    pub inbound_protocol: GenerationProtocol,
    /// Stable ordinal within that exact retained protocol root.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(max = 255)))]
    pub candidate_index: u16,
    /// Exact immutable static credential slot.
    pub credential: ResourceRef,
    /// Exact operator-pool admission profile revision.
    pub profile: ResourceRef,
}
impl Validate for NativeAdmissionAttachmentV1 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            usize::from(self.candidate_index) < MAX_CANDIDATES,
            "native admission ordinal",
        )?;
        require(
            self.credential.resource_kind == ResourceKind::Credential,
            "native admission credential",
        )?;
        super::admission::validate_profile_reference(&self.profile)
    }
}

/// Only a fully explicit current native route may use format three.
#[derive(Clone, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DefinitionV3 {
    /// One static route, with an exact profile mapping over each retained root.
    NativeProfiledRouteV1 {
        /// Explicit routing source; no ambient providers or inline credentials.
        #[cfg_attr(
            feature = "schema",
            schemars(with = "std::collections::BTreeMap<String, Value>")
        )]
        routing: Value,
        /// The static selector resolved only within this source.
        #[cfg_attr(feature = "schema", schemars(length(min = 1, max = 512)))]
        selector: String,
        /// Required null; dynamic policies are outside this bounded format.
        #[cfg_attr(feature = "schema", schemars(required, with = "()"))]
        policy: Option<ResourceRef>,
        /// Unique explicit provider/account credential slot associations.
        #[cfg_attr(feature = "schema", schemars(length(min = 1, max = 256)))]
        credentials: Vec<CredentialBinding>,
        /// Total current root map; at least one native GenerateContent entry.
        #[cfg_attr(feature = "schema", schemars(length(min = 1, max = 1024), extend("contains" = serde_json::json!({"type":"object","properties":{"inbound_protocol":{"const":"generate_content"}},"required":["inbound_protocol"]}))))]
        attachments: Vec<NativeAdmissionAttachmentV1>,
    },
}
impl std::fmt::Debug for DefinitionV3 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeProfiledRouteV1")
            .finish_non_exhaustive()
    }
}
#[derive(Deserialize)]
enum NativeKind {
    #[serde(rename = "native_profiled_route_v1")]
    Native,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeDefinition {
    #[serde(rename = "kind")]
    _kind: NativeKind,
    #[serde(deserialize_with = "routing")]
    routing: Value,
    selector: String,
    #[serde(deserialize_with = "nullable")]
    policy: Option<ResourceRef>,
    credentials: Vec<CredentialBinding>,
    attachments: Vec<NativeAdmissionAttachmentV1>,
}
fn routing<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Value, D::Error> {
    super::json::object(deserializer, 3).map(|object| Value::Object(object.into_iter().collect()))
}
impl<'de> Deserialize<'de> for DefinitionV3 {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let input: NativeDefinition = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
        Ok(Self::NativeProfiledRouteV1 {
            routing: input.routing,
            selector: input.selector,
            policy: input.policy,
            credentials: input.credentials,
            attachments: input.attachments,
        })
    }
}
impl Validate for DefinitionV3 {
    fn validate(&self) -> Result<(), ValidationError> {
        let Self::NativeProfiledRouteV1 {
            routing,
            selector,
            policy,
            credentials,
            attachments,
        } = self;
        super::validation::json_depth(routing, 3, "native routing JSON depth")?;
        require(routing.is_object(), "native routing document")?;
        text(selector, 512)?;
        require(selector.len() <= 512, "native selector byte bound")?;
        require(policy.is_none(), "native static route policy")?;
        require(
            !credentials.is_empty() && credentials.len() <= 256,
            "native credential bindings",
        )?;
        let mut slots = std::collections::HashSet::new();
        for binding in credentials {
            text(&binding.provider, 128)?;
            text(&binding.account, 128)?;
            require(
                binding.credential.resource_kind == ResourceKind::Credential,
                "native credential reference",
            )?;
            require(
                slots.insert((&binding.provider, &binding.account)),
                "duplicate native credential binding",
            )?;
        }
        require(
            !attachments.is_empty() && attachments.len() <= 1024,
            "native attachment count",
        )?;
        require(
            attachments
                .iter()
                .any(|item| item.inbound_protocol == GenerationProtocol::GenerateContent),
            "native attachment required",
        )?;
        let mut ordinals = std::collections::HashSet::new();
        for attachment in attachments {
            attachment.validate()?;
            require(
                ordinals.insert((attachment.inbound_protocol, attachment.candidate_index)),
                "duplicate native attachment",
            )?;
        }
        bytes(self, 1024 * 1024)
    }
}

/// Operator-only creation/rotation through the existing resource transaction.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ResourcePutV3 {
    /// Exactly three; older formats cannot create native route authority.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(min = 3, max = 3)))]
    pub format: u32,
    /// Existing operation identity, including ACK-loss recovery.
    pub operation_id: OperationId,
    /// Persistent authority store addressed by the command.
    pub installation_id: InstallationId,
    /// Exact route key; current writer enforces owner/grant scope.
    pub key: ResourceKey,
    /// Explicit nullable managed-head CAS; never an implicit head lookup.
    #[serde(deserialize_with = "nullable")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "super::resources::schema::RequiredNullable<RevisionId>")
    )]
    pub expected_revision: Option<RevisionId>,
    /// Closed native profile/credential/root association definition.
    pub definition: DefinitionV3,
}
impl Validate for ResourcePutV3 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(self.format == 3, "native resource format")?;
        require(
            self.key.resource_kind == ResourceKind::Route,
            "native resource kind",
        )?;
        self.definition.validate()?;
        bytes(self, 1024 * 1024)
    }
}

/// Explicit discovery does not widen v1/v2 capabilities or grant native access.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ResourceCapabilitiesV3 {
    /// Exactly three.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(min = 3, max = 3)))]
    pub format: u32,
    /// Persistent store identity; instance-specific ingress discovery is separate.
    pub installation_id: InstallationId,
    /// False unless encrypted writer and exact paired transport are available.
    pub provisioning_enabled: bool,
    /// Maximum whole command encoding, at most one MiB.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 1048576)))]
    pub max_command_bytes: u32,
    /// Must be true; this surface grants no endpoint-origin bypass.
    #[cfg_attr(feature = "schema", schemars(extend("const" = true)))]
    pub tenant_routes_require_approved_origins: bool,
    /// Exactly Chat, Messages, Responses, GenerateContent in this order.
    #[cfg_attr(feature = "schema", schemars(extend("const" = serde_json::json!(["chat_completions","messages","responses","generate_content"]))))]
    pub generation_protocols: [GenerationProtocol; 4],
}
impl Validate for ResourceCapabilitiesV3 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            self.format == 3
                && self.max_command_bytes > 0
                && self.max_command_bytes <= 1048576
                && self.tenant_routes_require_approved_origins
                && self.generation_protocols
                    == [
                        GenerationProtocol::ChatCompletions,
                        GenerationProtocol::Messages,
                        GenerationProtocol::Responses,
                        GenerationProtocol::GenerateContent,
                    ],
            "native resource capabilities",
        )
    }
}
