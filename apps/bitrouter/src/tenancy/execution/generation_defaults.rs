//! Closed, per-submission defaults stripped before protocol parsing.

use bitrouter_ai::{
    tenancy::{
        generation_defaults::{
            CanonicalGenerationDefaultsV1, ConfigurationDefaults, ENVELOPE_BODY_MEMBER,
            GenerationDefaultsEnvelope,
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
        // The three ordinary wires retain their configuration fill. The
        // separate native envelope selects its absence/default layer below.
        self.configuration_defaults = ConfigurationDefaults::Apply;
        self.generate_content = None;
        self.binding.catalog_selection = None;
        if matches!(protocol, ApiProtocol::Custom(name) if name ==
            bitrouter_ai::tenancy::generate_content::PROTOCOL)
        {
            return self.parse_generate_content(body);
        }
        let invalid = || Failure::from(ErrorCode::InvalidRequest);
        let (value, envelope) = if let Some(selection) = &self.binding.selection {
            let body: OrderedBody = serde_json::from_slice(body).map_err(|_| invalid())?;
            if digest(body.envelope.as_ref()).map_err(|_| invalid())?
                != selection.generation_defaults_digest
            {
                return Err(invalid());
            }
            self.binding.catalog_selection = body.catalog;
            self.check_catalog_protocol(protocol)?;
            (Value::Object(body.request), body.envelope)
        } else {
            (serde_json::from_slice(body).map_err(|_| invalid())?, None)
        };
        let mut input =
            crate::gateway::http::parse_input(protocol, value).map_err(|_| invalid())?;
        if let Some(envelope) = envelope {
            let prompt = input.generation_prompt_mut().ok_or_else(invalid)?;
            apply_envelope(&envelope, &mut prompt.params);
            self.configuration_defaults = envelope.configuration_defaults;
        }
        Ok(input)
    }

    fn check_catalog_protocol(&self, protocol: &ApiProtocol) -> Result<(), Failure> {
        if self
            .binding
            .catalog_selection
            .as_ref()
            .is_some_and(|input| {
                super::super::resources::execution::generation_protocol(protocol).ok()
                    != Some(input.inbound_protocol)
            })
        {
            return Err(ErrorCode::CapabilityMismatch.into());
        }
        Ok(())
    }

    fn parse_generate_content(&mut self, bytes: &[u8]) -> Result<PipelineInput, Failure> {
        use crate::gateway::pipeline::types::generate_content::Input;
        let invalid = || Failure::from(ErrorCode::InvalidRequest);
        let selection = self.binding.selection.as_ref().ok_or_else(invalid)?;
        let body: NativeBody = serde_json::from_slice(bytes).map_err(|_| invalid())?;
        if digest(body.envelope.as_ref()).map_err(|_| invalid())?
            != selection.generation_defaults_digest
        {
            return Err(invalid());
        }
        self.binding.catalog_selection = body.catalog;
        self.check_catalog_protocol(&ApiProtocol::Custom(
            bitrouter_ai::tenancy::generate_content::PROTOCOL.into(),
        ))?;
        // Preserve the original tokens for the native request decoder.
        let raw = serde_json::to_vec(&body.request).map_err(|_| invalid())?;
        let request = serde_json::from_slice(&raw).map_err(|_| invalid())?;
        let default_layer = body
            .envelope
            .as_ref()
            .map(|value| {
                serde_json::to_value(value).map(|value| (ENVELOPE_BODY_MEMBER.into(), value))
            })
            .transpose()
            .map_err(|_| invalid())?;
        let mut input = Input::new(request, default_layer).map_err(|_| invalid())?;
        self.configuration_defaults = ConfigurationDefaults::PreserveAbsence;
        if let Some(envelope) = &body.envelope {
            if let Some(prompt) = input.projection_mut() {
                apply_envelope(envelope, &mut prompt.params);
            }
            self.configuration_defaults = envelope.configuration_defaults;
        }
        input.seal_defaults();
        self.generate_content = Some(input.clone());
        Ok(PipelineInput::GenerateContent(Box::new(input)))
    }
}

fn digest(
    envelope: Option<&GenerationDefaultsEnvelope>,
) -> Result<
    Option<bitrouter_ai::tenancy::identity::Digest>,
    bitrouter_ai::tenancy::validation::ValidationError,
> {
    envelope.map(GenerationDefaultsEnvelope::digest).transpose()
}

// This member is invalid on all routes and must never reach a provider.
const REMOVED_DEFAULTS_MEMBER: &str = "bitrouter_generation_defaults";

struct OrderedBody {
    request: Map<String, Value>,
    envelope: Option<GenerationDefaultsEnvelope>,
    catalog: Option<bitrouter_ai::tenancy::catalog::CatalogSelectionRequestV1>,
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
                    envelope: None,
                    catalog: None,
                };
                while let Some(key) = map.next_key::<String>()? {
                    if key == REMOVED_DEFAULTS_MEMBER {
                        return Err(serde::de::Error::unknown_field(
                            &key,
                            &[ENVELOPE_BODY_MEMBER],
                        ));
                    }
                    if key == ENVELOPE_BODY_MEMBER {
                        if body.envelope.is_some() {
                            return Err(serde::de::Error::duplicate_field(ENVELOPE_BODY_MEMBER));
                        }
                        body.envelope = Some(map.next_value()?);
                    } else if key == bitrouter_ai::tenancy::catalog::BODY_MEMBER {
                        if body.catalog.is_some() {
                            return Err(serde::de::Error::custom("duplicate configured selection"));
                        }
                        body.catalog = Some(catalog_member(&mut map)?);
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

struct NativeBody {
    request: std::collections::BTreeMap<String, Box<serde_json::value::RawValue>>,
    envelope: Option<GenerationDefaultsEnvelope>,
    catalog: Option<bitrouter_ai::tenancy::catalog::CatalogSelectionRequestV1>,
}

impl<'de> Deserialize<'de> for NativeBody {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Body;
        impl<'de> Visitor<'de> for Body {
            type Value = NativeBody;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a native ordered envelope")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut body = NativeBody {
                    request: Default::default(),
                    envelope: None,
                    catalog: None,
                };
                while let Some(key) = map.next_key::<String>()? {
                    if key == REMOVED_DEFAULTS_MEMBER {
                        return Err(serde::de::Error::unknown_field(
                            &key,
                            &[ENVELOPE_BODY_MEMBER],
                        ));
                    }
                    if key == ENVELOPE_BODY_MEMBER {
                        if body.envelope.is_some() {
                            return Err(serde::de::Error::duplicate_field(ENVELOPE_BODY_MEMBER));
                        }
                        body.envelope = Some(map.next_value()?);
                    } else if key == bitrouter_ai::tenancy::catalog::BODY_MEMBER {
                        if body.catalog.is_some() {
                            return Err(serde::de::Error::custom("duplicate configured selection"));
                        }
                        body.catalog = Some(catalog_member(&mut map)?);
                    } else {
                        if body.request.contains_key(&key) {
                            return Err(serde::de::Error::custom(
                                "duplicate native envelope member",
                            ));
                        }
                        body.request.insert(key, map.next_value()?);
                    }
                }
                Ok(body)
            }
        }
        d.deserialize_map(Body)
    }
}

fn apply_envelope(envelope: &GenerationDefaultsEnvelope, params: &mut GenerationParams) {
    if let Some(defaults) = &envelope.canonical {
        apply(defaults, params);
    }
    crate::gateway::pipeline::context::apply_parameter_defaults(
        params,
        envelope.supplemental.iter(),
    );
}

fn catalog_member<'de, M: MapAccess<'de>>(
    map: &mut M,
) -> Result<bitrouter_ai::tenancy::catalog::CatalogSelectionRequestV1, M::Error> {
    use bitrouter_ai::tenancy::{catalog::MAX_SELECTION_BYTES, validation::Validate};
    let raw = map.next_value::<Box<serde_json::value::RawValue>>()?;
    if raw.get().len() > MAX_SELECTION_BYTES {
        return Err(serde::de::Error::custom(
            "configured selection exceeds body limit",
        ));
    }
    let value: bitrouter_ai::tenancy::catalog::CatalogSelectionRequestV1 =
        serde_json::from_str(raw.get()).map_err(serde::de::Error::custom)?;
    value.validate().map_err(serde::de::Error::custom)?;
    Ok(value)
}

pub(in crate::tenancy) fn apply(
    defaults: &CanonicalGenerationDefaultsV1,
    params: &mut GenerationParams,
) {
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
