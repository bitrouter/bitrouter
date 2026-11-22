//! Private historical native artifact codec and runtime attachment identity.
use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    configuration::ResourceRef,
    identity::AuthorityRevision,
    ordered::{GenerationProtocol, MAX_CANDIDATES},
    resources::CredentialBinding,
};
use serde::{Deserialize, Serialize, de::Error};
use serde_json::{Value, value::RawValue};

/// Complete four-protocol route attachment bound to exact retained authorities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::tenancy::resources) struct NativeAdmissionAttachmentV1 {
    /// Actual ingress protocol, never inferred from a Chat permission.
    pub inbound_protocol: GenerationProtocol,
    /// Stable ordinal within that exact retained protocol root.
    #[serde(deserialize_with = "ordinal")]
    pub candidate_index: u16,
    /// Exact immutable static credential slot.
    pub credential: ResourceRef,
    /// Exact operator-pool admission profile revision.
    pub profile: ResourceRef,
}
impl NativeAdmissionAttachmentV1 {
    pub(in crate::tenancy::resources) fn validate(&self) -> Result<()> {
        require(
            usize::from(self.candidate_index) < MAX_CANDIDATES,
            "native admission ordinal",
        )?;
        require(
            self.credential.resource_kind == ResourceKind::Credential,
            "native admission credential",
        )?;
        require(
            self.profile.owner_kind == OwnerKind::OperatorPool
                && self.profile.resource_kind == ResourceKind::Policy,
            "admission profile reference",
        )
    }
}

/// Only a fully explicit current native route may use format three.
#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum NativeDefinition {
    /// One static route, with an exact profile mapping over each retained root.
    NativeProfiledRouteV1 {
        /// Explicit routing source; no ambient providers or inline credentials.
        routing: Value,
        /// The static selector resolved only within this source.
        selector: String,
        /// Required null; dynamic policies are outside this bounded format.
        policy: Option<ResourceRef>,
        /// Unique explicit provider/account credential slot associations.
        credentials: Vec<CredentialBinding>,
        /// Total current root map; at least one native GenerateContent entry.
        attachments: Vec<NativeAdmissionAttachmentV1>,
    },
}
impl std::fmt::Debug for NativeDefinition {
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
struct RawNativeDefinition {
    #[serde(rename = "kind")]
    _kind: NativeKind,
    #[serde(deserialize_with = "routing")]
    routing: Value,
    selector: String,
    #[serde(deserialize_with = "super::decoding::required_nullable")]
    policy: Option<ResourceRef>,
    credentials: Vec<CredentialBinding>,
    attachments: Vec<NativeAdmissionAttachmentV1>,
}
fn routing<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Value, D::Error> {
    json::object(deserializer, 3).map(|object| Value::Object(object.into_iter().collect()))
}
impl<'de> Deserialize<'de> for NativeDefinition {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let input: RawNativeDefinition =
            serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
        Ok(Self::NativeProfiledRouteV1 {
            routing: input.routing,
            selector: input.selector,
            policy: input.policy,
            credentials: input.credentials,
            attachments: input.attachments,
        })
    }
}
impl NativeDefinition {
    pub(in crate::tenancy::resources) fn validate(&self) -> Result<()> {
        let Self::NativeProfiledRouteV1 {
            routing,
            selector,
            policy,
            credentials,
            attachments,
        } = self;
        json_depth(routing, 3, "native routing JSON depth")?;
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

// Keep the same exact integer codec as authority revisions, then narrow to u16.
fn ordinal<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u16, D::Error> {
    let value = AuthorityRevision::deserialize(deserializer)?;
    u16::try_from(value.get()).map_err(D::Error::custom)
}

fn require(condition: bool, message: &'static str) -> Result<()> {
    ensure!(condition, "{message}");
    Ok(())
}

fn text(value: &str, maximum: usize) -> Result<()> {
    require(
        !value.is_empty() && value.chars().count() <= maximum,
        "text length",
    )
}

fn json_depth(value: &Value, parents: usize, message: &'static str) -> Result<()> {
    match value {
        Value::Array(values) => {
            require(parents + 1 < 128, message)?;
            for value in values {
                json_depth(value, parents + 1, message)?;
            }
        }
        Value::Object(values) => {
            require(parents + 1 < 128, message)?;
            for value in values.values() {
                json_depth(value, parents + 1, message)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn bytes(value: &impl Serialize, maximum: usize) -> Result<()> {
    struct Bounded(usize);
    impl std::io::Write for Bounded {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(buffer.len())
                .ok_or_else(|| std::io::Error::other("contract byte limit"))?;
            Ok(buffer.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Bounded(maximum), value)?;
    Ok(())
}

mod json;
#[cfg(test)]
mod tests;
