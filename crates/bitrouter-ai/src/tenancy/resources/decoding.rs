//! Select a definition without buffering numeric tokens through Serde Content.
//! Internally tagged enum buffering cannot carry RawValue's exact-number input.
//! Every current variant uses a closed decoder with exact numeric tokens.

use super::*;
use serde::de::Error;
use serde_json::value::RawValue;

#[derive(Deserialize)]
struct Tag {
    kind: String,
}

#[derive(Deserialize)]
enum PolicyTag {
    #[serde(rename = "policy")]
    Policy,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelPolicy {
    #[serde(rename = "kind")]
    _kind: PolicyTag,
    document: crate::policy::Document,
    name: String,
    dependencies: Vec<ResourceRef>,
}

#[derive(Deserialize)]
enum ProfileTag {
    #[serde(rename = "admission_profile")]
    Profile,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {
    #[serde(rename = "kind")]
    _kind: ProfileTag,
    profile: AdmissionProfile,
}

#[derive(Deserialize)]
enum RouteTag {
    #[serde(rename = "profiled_route")]
    Route,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Route {
    #[serde(rename = "kind")]
    _kind: RouteTag,
    #[serde(deserialize_with = "routing")]
    routing: Value,
    selector: String,
    #[serde(deserialize_with = "nullable")]
    policy: Option<ResourceRef>,
    credentials: Vec<CredentialBinding>,
    attachments: Vec<AdmissionAttachment>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Credential {
    kind: String,
    secret: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UnprofiledRoute {
    kind: String,
    #[serde(deserialize_with = "routing")]
    routing: Value,
    selector: String,
    #[serde(deserialize_with = "nullable")]
    policy: Option<ResourceRef>,
    credentials: Vec<CredentialBinding>,
}

fn routing<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Value, D::Error> {
    super::super::json::object(deserializer, 3)
        .map(|object| Value::Object(object.into_iter().collect()))
}

impl<'de> Deserialize<'de> for Definition {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        let tag: Tag = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
        match tag.kind.as_str() {
            "policy" => {
                let value: ModelPolicy =
                    serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
                Ok(Self::Policy {
                    document: value.document,
                    name: value.name,
                    dependencies: value.dependencies,
                })
            }
            "admission_profile" => {
                let value: Profile = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
                Ok(Self::AdmissionProfile {
                    profile: value.profile,
                })
            }
            "profiled_route" => {
                let value: Route = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
                Ok(Self::ProfiledRoute {
                    routing: value.routing,
                    selector: value.selector,
                    policy: value.policy,
                    credentials: value.credentials,
                    attachments: value.attachments,
                })
            }
            "credential" => {
                let value: Credential =
                    serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
                if value.kind != "credential" {
                    return Err(D::Error::custom("invalid resource kind"));
                }
                Ok(Self::Credential {
                    secret: value.secret,
                })
            }
            "route" => {
                let value: UnprofiledRoute =
                    serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
                if value.kind != "route" {
                    return Err(D::Error::custom("invalid resource kind"));
                }
                Ok(Self::Route {
                    routing: value.routing,
                    selector: value.selector,
                    policy: value.policy,
                    credentials: value.credentials,
                })
            }
            _ => Err(D::Error::custom("unsupported resource kind")),
        }
    }
}
