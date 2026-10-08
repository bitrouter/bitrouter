//! Protocol dispatch for the canonical classifier representation.

use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

use crate::classifier::{ClassifierRequest, ClassifierResult};
use crate::conversion::{
    ConversionDisposition, ConversionEffect, ConversionIssue, ConversionLocation, ConversionReason,
    ConversionReport, ConversionStage,
};
use crate::error::{ModelError, Result};
use crate::protocol::Transport;
use crate::protocol::decisions::{DecisionsCodec, DecisionsTransport};
use crate::protocol::systemone::{SystemOneCodec, SystemOneTransport};
use crate::types::ApiProtocol;

struct JsonSeed<'a> {
    duplicates: &'a mut bool,
    ambiguous_usage: &'a mut bool,
    root: bool,
    in_usage: bool,
}

impl<'de> DeserializeSeed<'de> for JsonSeed<'_> {
    type Value = Value;
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for JsonSeed<'_> {
    type Value = Value;
    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a classifier JSON value")
    }
    fn visit_bool<E: serde::de::Error>(self, value: bool) -> std::result::Result<Value, E> {
        Ok(Value::Bool(value))
    }
    fn visit_i64<E: serde::de::Error>(self, value: i64) -> std::result::Result<Value, E> {
        Ok(Value::Number(value.into()))
    }
    fn visit_u64<E: serde::de::Error>(self, value: u64) -> std::result::Result<Value, E> {
        Ok(Value::Number(value.into()))
    }
    fn visit_f64<E: serde::de::Error>(self, value: f64) -> std::result::Result<Value, E> {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("invalid classifier number"))
    }
    fn visit_str<E: serde::de::Error>(self, value: &str) -> std::result::Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }
    fn visit_string<E: serde::de::Error>(self, value: String) -> std::result::Result<Value, E> {
        Ok(Value::String(value))
    }
    fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> std::result::Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(JsonSeed {
            duplicates: self.duplicates,
            ambiguous_usage: self.ambiguous_usage,
            root: false,
            in_usage: self.in_usage,
        })? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Value, A::Error> {
        let mut values = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let in_usage = self.in_usage || (self.root && key == "usage");
            let value = map.next_value_seed(JsonSeed {
                duplicates: self.duplicates,
                ambiguous_usage: self.ambiguous_usage,
                root: false,
                in_usage,
            })?;
            if values.insert(key, value).is_some() {
                *self.duplicates = true;
                if in_usage {
                    *self.ambiguous_usage = true;
                }
            }
        }
        Ok(Value::Object(values))
    }
}

fn decode_json(bytes: &[u8]) -> std::result::Result<(Value, bool, bool), serde_json::Error> {
    let mut duplicates = false;
    let mut ambiguous_usage = false;
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let value = JsonSeed {
        duplicates: &mut duplicates,
        ambiguous_usage: &mut ambiguous_usage,
        root: true,
        in_usage: false,
    }
    .deserialize(&mut decoder)?;
    decoder.end()?;
    Ok((value, duplicates, ambiguous_usage))
}

/// Parse ingress before duplicate object members can erase question identity.
pub fn parse_request_json(protocol: &ApiProtocol, bytes: &[u8]) -> Result<ClassifierRequest> {
    let (body, duplicates, _) = decode_json(bytes)
        .map_err(|_| ModelError::invalid_request("invalid classifier request JSON"))?;
    if duplicates {
        return Err(ModelError::invalid_request(
            "duplicate classifier request field",
        ));
    }
    codec_for(protocol)
        .ok_or_else(|| ModelError::invalid_request("unsupported classifier protocol"))?
        .parse_request(body)
}

/// Decode completed output while retaining usage whose own object is unambiguous.
pub fn parse_response_json(
    protocol: &ApiProtocol,
    bytes: &[u8],
    request: &ClassifierRequest,
) -> Result<ClassifierResult> {
    let fail = |usage| ModelError::ClassifierResponse {
        failure: crate::classifier::ClassifierResponseFailure {
            message: "invalid or ambiguous classifier response JSON".into(),
            usage,
        },
    };
    let (body, duplicates, ambiguous_usage) = decode_json(bytes).map_err(|_| fail(None))?;
    if duplicates {
        let usage = if ambiguous_usage {
            None
        } else {
            body.get("usage")
                .and_then(|raw| match protocol {
                    ApiProtocol::Decisions => crate::protocol::decisions::decode_usage(raw).ok(),
                    ApiProtocol::SystemOne => crate::protocol::systemone::decode_usage(raw),
                    _ => None,
                })
                .map(|mut usage| {
                    if let Some(raw) = usage.raw.as_deref()
                        && serde_json::to_vec(raw).map_or(true, |bytes| bytes.len() > 64 * 1024)
                    {
                        usage.raw = Some(Box::new(crate::protocol::decisions::known_usage_fields(
                            raw, protocol,
                        )));
                    }
                    Box::new(usage)
                })
        };
        return Err(fail(usage));
    }
    codec_for(protocol)
        .ok_or_else(|| ModelError::invalid_request("unsupported classifier protocol"))?
        .parse_response(body, request)
}

/// Admit both the model-visible projection and the caller's required usage shape.
pub fn admission(protocol: &ApiProtocol, request: &ClassifierRequest) -> ConversionReport {
    let mut report = ConversionReport::default();
    let reason = match codec_for(protocol) {
        None => Some(ConversionReason::OperationUnsupported),
        Some(codec) if codec.render_request(request).is_err() => {
            Some(ConversionReason::InputContentUnrepresentable)
        }
        Some(_)
            if *protocol == ApiProtocol::SystemOne
                && request.source_protocol == Some(ApiProtocol::Decisions) =>
        {
            Some(ConversionReason::ClassifierUsageUnrepresentable)
        }
        Some(_) => None,
    };
    if let Some(reason) = reason {
        report.issues.push(ConversionIssue {
            stage: ConversionStage::RequestProjection,
            protocol: protocol.into(),
            location: ConversionLocation::Operation,
            reason,
            effect: ConversionEffect::TaskSemantics,
            disposition: ConversionDisposition::ExcludeTarget,
        });
    }
    if *protocol == ApiProtocol::Decisions
        && request.source_protocol == Some(ApiProtocol::SystemOne)
    {
        report.admitted.push(ConversionIssue {
            stage: ConversionStage::ResponseEncoding,
            protocol: (&ApiProtocol::SystemOne).into(),
            location: ConversionLocation::Operation,
            reason: ConversionReason::ClassifierConfidenceDerived,
            effect: ConversionEffect::EquivalentRepresentation,
            disposition: ConversionDisposition::Allow,
        });
    }
    report
}

/// Pure bidirectional classifier conversion; no generative or stream methods.
pub trait ClassifierCodec: Send + Sync {
    /// Parse native client evidence and questions.
    fn parse_request(&self, body: Value) -> Result<ClassifierRequest>;
    /// Render a target-compatible canonical request.
    fn render_request(&self, request: &ClassifierRequest) -> Result<Value>;
    /// Decode a completed response against the original correlated request.
    fn parse_response(&self, body: Value, request: &ClassifierRequest) -> Result<ClassifierResult>;
    /// Render the result in the caller's selected classifier protocol.
    fn render_response(
        &self,
        result: &ClassifierResult,
        request: &ClassifierRequest,
    ) -> Result<Value>;
}

impl ClassifierCodec for DecisionsCodec {
    fn parse_request(&self, body: Value) -> Result<ClassifierRequest> {
        Self::parse_request(body)
    }
    fn render_request(&self, request: &ClassifierRequest) -> Result<Value> {
        Self::render_request(request)
    }
    fn parse_response(&self, body: Value, request: &ClassifierRequest) -> Result<ClassifierResult> {
        Self::parse_response(body, request)
    }
    fn render_response(
        &self,
        result: &ClassifierResult,
        request: &ClassifierRequest,
    ) -> Result<Value> {
        Self::render_response(result, request)
    }
}

/// Built-in classifier codecs are stateless and share one dispatch contract.
pub fn codec_for(protocol: &ApiProtocol) -> Option<&'static dyn ClassifierCodec> {
    match protocol {
        ApiProtocol::Decisions => Some(&DecisionsCodec),
        ApiProtocol::SystemOne => Some(&SystemOneCodec),
        ApiProtocol::ChatCompletions
        | ApiProtocol::Messages
        | ApiProtocol::Responses
        | ApiProtocol::Custom(_) => None,
    }
}

/// Selected classifier HTTP transport, independent of the caller's wire.
pub fn transport_for(protocol: &ApiProtocol) -> Option<&'static dyn Transport> {
    match protocol {
        ApiProtocol::Decisions => Some(&DecisionsTransport),
        ApiProtocol::SystemOne => Some(&SystemOneTransport),
        ApiProtocol::ChatCompletions
        | ApiProtocol::Messages
        | ApiProtocol::Responses
        | ApiProtocol::Custom(_) => None,
    }
}

/// Validate canonical results before success hooks, including custom executors.
pub fn validate_result(result: &ClassifierResult, request: &ClassifierRequest) -> Result<()> {
    let codec = codec_for(&result.protocol)
        .ok_or_else(|| ModelError::invalid_request("non-classifier result protocol"))?;
    let mut native_request = request.clone();
    native_request.source_protocol = Some(result.protocol.clone());
    // Validate under the source response contract, independently of client egress.
    codec.render_response(result, &native_request).map(|_| ())
}
