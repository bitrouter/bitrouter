//! Decode the chosen dispatch directly from original JSON tokens.
use super::Dispatch;
use crate::tenancy::ordered::AcceptedSelection;
use serde::{Deserialize, Deserializer, de::Error};

enum Mode {
    Strict,
    Ordered,
}

impl<'de> Deserialize<'de> for Mode {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mode = String::deserialize(deserializer)?;
        match mode.as_str() {
            "strict" => Ok(Self::Strict),
            "ordered" => Ok(Self::Ordered),
            _ => Err(D::Error::unknown_variant(&mode, &["strict", "ordered"])),
        }
    }
}

impl<'de> Deserialize<'de> for Dispatch {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Serde's internally tagged buffering cannot preserve RawValue integer
        // tokens. Select the mode, then decode its closed shape from those bytes.
        let raw = Box::<serde_json::value::RawValue>::deserialize(deserializer)?;
        // Derived structs also accept positional arrays; the wire requires a map.
        if !raw.get().trim_start().starts_with('{') {
            return Err(D::Error::custom("dispatch must be a JSON object"));
        }
        #[derive(Deserialize)]
        struct Header {
            mode: Mode,
        }
        let header: Header = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
        match header.mode {
            Mode::Strict => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Strict {
                    mode: Mode,
                }
                let strict: Strict = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
                match strict.mode {
                    Mode::Strict => Ok(Self::Strict),
                    Mode::Ordered => Err(D::Error::custom("dispatch mode mismatch")),
                }
            }
            Mode::Ordered => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Ordered {
                    mode: Mode,
                    accepted_selection: AcceptedSelection,
                    #[serde(deserialize_with = "crate::tenancy::number::integer")]
                    attempt_limit: u8,
                }
                let ordered: Ordered = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
                match ordered.mode {
                    Mode::Ordered => Ok(Self::Ordered {
                        accepted_selection: ordered.accepted_selection,
                        attempt_limit: ordered.attempt_limit,
                    }),
                    Mode::Strict => Err(D::Error::custom("dispatch mode mismatch")),
                }
            }
        }
    }
}
