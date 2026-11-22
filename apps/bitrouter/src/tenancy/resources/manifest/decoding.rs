//! Preserve exact profile-number tokens while keeping the legacy import codec.

use super::*;
use serde::de::Error;
use serde_json::value::RawValue;

#[derive(Deserialize)]
struct Tag {
    kind: String,
}

pub(super) fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainedProfileV1 {
    #[serde(rename = "kind")]
    _kind: String,
    profile: AdmissionProfileV1,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainedProfileV2 {
    #[serde(rename = "kind")]
    _kind: String,
    profile: AdmissionProfileV2,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainedProfiledRoute {
    #[serde(rename = "kind")]
    _kind: String,
    routing: Value,
    selector: String,
    #[serde(deserialize_with = "required_nullable")]
    policy: Option<ResourceRef>,
    credentials: Vec<bitrouter_ai::tenancy::resources::CredentialBinding>,
    attachments: Vec<AdmissionAttachmentV1>,
}

#[derive(Deserialize)]
#[serde(
    remote = "Definition",
    tag = "kind",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum LegacyDefinition {
    Route {
        routing: Value,
        selector: String,
        policy: Option<ResourceRef>,
        credentials: Vec<CredentialBinding>,
        #[serde(default)]
        exploration: Vec<String>,
        #[serde(default)]
        core_choice: Option<Box<super::super::core_choice::Profile>>,
        #[serde(default)]
        assessment: Option<AssessmentProfile>,
        #[serde(default)]
        block: Option<Box<super::super::block::Profile>>,
    },
    Policy {
        document: PolicyLock,
        name: String,
        #[serde(default)]
        dependencies: Vec<ResourceRef>,
    },
    Checkpoint {
        evidence: super::super::checkpoint::Evidence,
    },
    Credential {
        credential: Credential,
        #[serde(default)]
        oauth_generation: u64,
        #[serde(default)]
        oauth: Option<crate::tenancy::oauth::profile::Profile>,
    },
}

impl<'de> Deserialize<'de> for Definition {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let tag: Tag = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
        if tag.kind == "native_profiled_route_v1" {
            let crate::tenancy::resources::manifest::native::NativeDefinition::NativeProfiledRouteV1 {
                routing,
                selector,
                policy,
                credentials,
                attachments,
            } = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
            return Ok(Self::NativeProfiledRouteV1 {
                routing,
                selector,
                policy,
                attachments,
                credentials: credentials
                    .into_iter()
                    .map(|binding| CredentialBinding {
                        provider: binding.provider,
                        account: binding.account,
                        credential: binding.credential,
                    })
                    .collect(),
            });
        }
        if tag.kind == "admission_profile_v2" {
            let RetainedProfileV2 { profile, .. } =
                serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
            return Ok(Self::AdmissionProfileV2 { profile });
        }
        if tag.kind == "admission_profile" {
            let RetainedProfileV1 { profile, .. } =
                serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
            return Ok(Self::AdmissionProfile { profile });
        }
        if tag.kind == "profiled_route" {
            let RetainedProfiledRoute {
                routing,
                selector,
                policy,
                credentials,
                attachments,
                ..
            } = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
            return Ok(Self::ProfiledRoute {
                routing,
                selector,
                policy,
                attachments,
                credentials: credentials
                    .into_iter()
                    .map(|binding| CredentialBinding {
                        provider: binding.provider,
                        account: binding.account,
                        credential: binding.credential,
                    })
                    .collect(),
            });
        }
        LegacyDefinition::deserialize(&mut serde_json::Deserializer::from_str(raw.get()))
            .map_err(D::Error::custom)
    }
}
