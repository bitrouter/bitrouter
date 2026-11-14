//! Narrow format-two provisioning for shared-health admission profiles.
//! Existing format-one resources, revocation and status contracts remain exact.

use serde::{Deserialize, Serialize, de::Error};
use serde_json::value::RawValue;

use super::{
    admission_v2::AdmissionProfileV2,
    authority::{OwnerKind, ResourceKey, ResourceKind},
    identity::{InstallationId, OperationId, RevisionId},
    validation::{Validate, ValidationError, bytes, nullable, require},
};

/// Strict request-format dispatch preserving each version's original typed codec.
pub enum ResourcePutRequest {
    /// Original format-one command, including its closed definition variants.
    V1(Box<super::resources::ResourcePut>),
    /// Explicit shared-health profile command.
    V2(Box<ResourcePutV2>),
    /// Explicit current native route; no format-one authority widening.
    V3(Box<super::resources_v3::ResourcePutV3>),
}

impl<'de> Deserialize<'de> for ResourcePutRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Format {
            #[serde(deserialize_with = "super::number::integer")]
            format: u32,
        }
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let format: Format = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
        match format.format {
            1 => serde_json::from_str(raw.get())
                .map(Self::V1)
                .map_err(D::Error::custom),
            2 => serde_json::from_str(raw.get())
                .map(Self::V2)
                .map_err(D::Error::custom),
            3 => serde_json::from_str(raw.get())
                .map(Self::V3)
                .map_err(D::Error::custom),
            _ => Err(D::Error::custom("unsupported resource format")),
        }
    }
}

impl Validate for ResourcePutRequest {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::V1(input) => input.validate(),
            Self::V2(input) => input.validate(),
            Self::V3(input) => input.validate(),
        }
    }
}

/// The closed set of definitions supported by format-two provisioning.
#[derive(Clone, Debug, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DefinitionV2 {
    /// An operator-pool Policy resource with explicit shared health identity.
    AdmissionProfileV2 {
        /// Independently addressable member rules and immutable group membership.
        profile: AdmissionProfileV2,
    },
}

#[derive(Deserialize)]
enum ProfileTag {
    #[serde(rename = "admission_profile_v2")]
    Profile,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileDefinition {
    #[serde(rename = "kind")]
    _kind: ProfileTag,
    profile: AdmissionProfileV2,
}

impl<'de> Deserialize<'de> for DefinitionV2 {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Preserve exact numeric tokens rather than internally tagged enum buffering.
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let decoded: ProfileDefinition =
            serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
        Ok(Self::AdmissionProfileV2 {
            profile: decoded.profile,
        })
    }
}

impl Validate for DefinitionV2 {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::AdmissionProfileV2 { profile } => profile.validate(),
        }
    }
}

/// Create or rotate a shared-health profile through the existing authority writer.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ResourcePutV2 {
    /// Resource provisioning format, exactly two.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(min = 2, max = 2)))]
    pub format: u32,
    /// Stable identity for replay and uncertain-outcome recovery.
    pub operation_id: OperationId,
    /// Explicit persistent installation identity.
    pub installation_id: InstallationId,
    /// Operator-pool Policy member key, independent of group identities.
    pub key: ResourceKey,
    /// Null creates a new managed key; otherwise compare its exact current head.
    #[serde(deserialize_with = "nullable")]
    #[cfg_attr(
        feature = "schema",
        schemars(with = "super::resources::schema::RequiredNullable<RevisionId>")
    )]
    pub expected_revision: Option<RevisionId>,
    /// Closed, bounded shared-health profile definition.
    pub definition: DefinitionV2,
}

impl Validate for ResourcePutV2 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(self.format == 2, "resource format")?;
        require(
            self.key.owner_kind == OwnerKind::OperatorPool
                && self.key.resource_kind == ResourceKind::Policy,
            "admission profile owner and kind",
        )?;
        self.definition.validate()?;
        bytes(self, 1024 * 1024)
    }
}

/// Explicit format-two discovery; does not alter format-one discovery or grant authority.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ResourceCapabilitiesV2 {
    /// Resource capability contract, exactly two.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(min = 2, max = 2)))]
    pub format: u32,
    /// Persistent installation identity.
    pub installation_id: InstallationId,
    /// Encrypted provisioning has its explicit wrapping key.
    pub provisioning_enabled: bool,
    /// Maximum complete serialized command bytes.
    #[serde(deserialize_with = "super::number::integer")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1, max = 1048576)))]
    pub max_command_bytes: u32,
    /// Existing tenant route origin restrictions remain mandatory.
    pub tenant_routes_require_approved_origins: bool,
    /// Supported profile versions, exactly [1, 2]; version two requires shared health binding.
    #[serde(deserialize_with = "profile_versions")]
    #[cfg_attr(feature = "schema", schemars(extend("const" = serde_json::json!([1, 2]))))]
    pub admission_profile_versions: [u8; 2],
}

fn profile_versions<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<[u8; 2], D::Error> {
    #[derive(Deserialize)]
    struct Version(#[serde(deserialize_with = "super::number::integer")] u8);
    let [first, second] = <[Version; 2]>::deserialize(deserializer)?;
    Ok([first.0, second.0])
}

impl Validate for ResourceCapabilitiesV2 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            self.format == 2
                && self.max_command_bytes > 0
                && self.max_command_bytes <= 1024 * 1024
                && self.tenant_routes_require_approved_origins
                && self.admission_profile_versions == [1, 2],
            "resource capabilities",
        )
    }
}
