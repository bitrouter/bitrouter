//! Preserve exact profile-number tokens while keeping the legacy import codec.

use super::*;
use serde::de::Error;
use serde_json::value::RawValue;

#[derive(Deserialize)]
struct Tag {
    kind: String,
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
            let bitrouter_ai::tenancy::resources_v3::DefinitionV3::NativeProfiledRouteV1 {
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
            let bitrouter_ai::tenancy::resources_v2::DefinitionV2::AdmissionProfileV2 { profile } =
                serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
            return Ok(Self::AdmissionProfileV2 { profile });
        }
        if matches!(tag.kind.as_str(), "admission_profile" | "profiled_route") {
            let public: bitrouter_ai::tenancy::resources::Definition =
                serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
            match public {
                bitrouter_ai::tenancy::resources::Definition::AdmissionProfile { profile } => {
                    Ok(Self::AdmissionProfile { profile })
                }
                bitrouter_ai::tenancy::resources::Definition::ProfiledRoute {
                    routing,
                    selector,
                    policy,
                    credentials,
                    attachments,
                } => Ok(Self::ProfiledRoute {
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
                }),
                _ => Err(D::Error::custom("invalid profiled resource definition")),
            }
        } else {
            LegacyDefinition::deserialize(&mut serde_json::Deserializer::from_str(raw.get()))
                .map_err(D::Error::custom)
        }
    }
}
