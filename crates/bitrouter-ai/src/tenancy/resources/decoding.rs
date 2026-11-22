//! Select a definition without buffering numeric tokens through Serde Content.
//! Internally tagged enum buffering cannot carry RawValue's exact-number input.
//! Legacy variants keep their original derived decoder and serialized fields.

use super::*;
use serde::de::Error;
use serde_json::value::RawValue;

#[derive(Deserialize)]
#[serde(
    remote = "Definition",
    tag = "kind",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum LegacyDefinition {
    Credential {
        secret: String,
    },
    Route {
        routing: Value,
        selector: String,
        #[serde(deserialize_with = "nullable")]
        policy: Option<ResourceRef>,
        credentials: Vec<CredentialBinding>,
    },
    Policy {
        document: Value,
        name: String,
        dependencies: Vec<ResourceRef>,
    },
}

impl<'de> Deserialize<'de> for Definition {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Box::<RawValue>::deserialize(deserializer)?;
        LegacyDefinition::deserialize(&mut serde_json::Deserializer::from_str(raw.get()))
            .map_err(D::Error::custom)
    }
}
