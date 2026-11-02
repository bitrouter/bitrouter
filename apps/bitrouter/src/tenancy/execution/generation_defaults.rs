//! Closed, per-submission defaults stripped before protocol parsing.

use bitrouter_ai::{
    tenancy::{
        generation_defaults::{
            BODY_MEMBER, CanonicalGenerationDefaultsV1, ConfigurationDefaults,
            ENVELOPE_BODY_MEMBER, GenerationDefaultsEnvelopeV1, GenerationDefaultsEnvelopeV2,
        },
        operation::ErrorCode,
    },
    types::{ApiProtocol, GenerationParams},
};
use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, Visitor},
};
use serde_json::{Map, Value};

use super::Submission;
use crate::{gateway::pipeline::types::PipelineInput, tenancy::error::Failure};

impl Submission {
    pub(in crate::tenancy) fn parse_http_input(
        &mut self,
        protocol: &ApiProtocol,
        body: &[u8],
    ) -> Result<PipelineInput, Failure> {
        // Only this successful, digest-verified parse may select another layer.
        // Old, native and failed parse paths retain the configuration fill.
        self.configuration_defaults = ConfigurationDefaults::Apply;
        let invalid = || Failure::from(ErrorCode::InvalidRequest);
        let (value, defaults, envelope) = if let Some(selection) = &self.binding.selection {
            let body: OrderedBody = serde_json::from_slice(body).map_err(|_| invalid())?;
            let digest = match (&body.defaults, &body.envelope) {
                (Some(defaults), None) => Some(defaults.digest()),
                (None, Some(envelope)) => Some(envelope.digest()),
                (None, None) => None,
                _ => return Err(invalid()),
            }
            .transpose()
            .map_err(|_| invalid())?;
            if digest != selection.generation_defaults_digest {
                return Err(invalid());
            }
            (Value::Object(body.request), body.defaults, body.envelope)
        } else {
            // Strict v1 retains its existing protocol-body decoding contract.
            (
                serde_json::from_slice(body).map_err(|_| invalid())?,
                None,
                None,
            )
        };
        let mut input =
            crate::gateway::http::parse_input(protocol, value).map_err(|_| invalid())?;
        if let Some(defaults) = defaults {
            let prompt = input.generation_prompt_mut().ok_or_else(invalid)?;
            apply(&defaults, &mut prompt.params);
        }
        if let Some(envelope) = envelope {
            let (canonical, supplemental, configuration_defaults) = match &envelope {
                Envelope::V1(value) => (
                    &value.canonical,
                    &value.supplemental,
                    ConfigurationDefaults::Apply,
                ),
                Envelope::V2(value) => (
                    &value.canonical,
                    &value.supplemental,
                    value.configuration_defaults,
                ),
            };
            let prompt = input.generation_prompt_mut().ok_or_else(invalid)?;
            if let Some(canonical) = canonical {
                apply(canonical, &mut prompt.params);
            }
            crate::gateway::pipeline::context::apply_parameter_defaults(
                &mut prompt.params,
                supplemental.iter(),
            );
            self.configuration_defaults = configuration_defaults;
        }
        Ok(input)
    }
}

// Keep the original JSON tokens through version selection. Serde's untagged
// buffering would erase exact decimal/exponent spellings required by the integer
// contract. Both concrete decoders reject unknown fields and duplicate keys;
// digest() validates the exact version before any layer choice can take effect.
enum Envelope {
    V1(GenerationDefaultsEnvelopeV1),
    V2(GenerationDefaultsEnvelopeV2),
}

impl Envelope {
    fn digest(
        &self,
    ) -> Result<
        bitrouter_ai::tenancy::identity::Digest,
        bitrouter_ai::tenancy::validation::ValidationError,
    > {
        match self {
            Self::V1(value) => value.digest(),
            Self::V2(value) => value.digest(),
        }
    }
}

impl<'de> Deserialize<'de> for Envelope {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Box::<serde_json::value::RawValue>::deserialize(deserializer)?;
        if let Ok(value) = serde_json::from_str::<GenerationDefaultsEnvelopeV1>(raw.get()) {
            return Ok(Self::V1(value));
        }
        serde_json::from_str::<GenerationDefaultsEnvelopeV2>(raw.get())
            .map(Self::V2)
            .map_err(serde::de::Error::custom)
    }
}

struct OrderedBody {
    request: Map<String, Value>,
    defaults: Option<CanonicalGenerationDefaultsV1>,
    envelope: Option<Envelope>,
}

impl<'de> Deserialize<'de> for OrderedBody {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BodyVisitor;
        impl<'de> Visitor<'de> for BodyVisitor {
            type Value = OrderedBody;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a generation request object")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut body = OrderedBody {
                    request: Map::new(),
                    defaults: None,
                    envelope: None,
                };
                while let Some(key) = map.next_key::<String>()? {
                    if key == BODY_MEMBER {
                        if body.defaults.is_some() || body.envelope.is_some() {
                            return Err(serde::de::Error::duplicate_field(BODY_MEMBER));
                        }
                        // Decode directly, before Value could erase duplicate keys.
                        body.defaults = Some(map.next_value()?);
                    } else if key == ENVELOPE_BODY_MEMBER {
                        if body.defaults.is_some() || body.envelope.is_some() {
                            return Err(serde::de::Error::custom(
                                "competing generation defaults members",
                            ));
                        }
                        body.envelope = Some(map.next_value()?);
                    } else {
                        body.request.insert(key, map.next_value()?);
                    }
                }
                Ok(body)
            }
        }
        deserializer.deserialize_map(BodyVisitor)
    }
}

fn apply(defaults: &CanonicalGenerationDefaultsV1, params: &mut GenerationParams) {
    macro_rules! fill {
        ($($field:ident),+ $(,)?) => {$(
            if params.$field.is_none() && !params.extra.contains_key(stringify!($field)) {
                params.$field = defaults.$field;
            }
        )+};
    }
    fill!(
        temperature,
        top_p,
        max_tokens,
        reasoning_effort,
        top_k,
        seed,
        presence_penalty,
        frequency_penalty
    );
    if params.stop.is_empty()
        && !params.extra.contains_key("stop")
        && let Some(stop) = &defaults.stop
    {
        params.stop.clone_from(stop);
    }
}

#[cfg(test)]
mod tests;
