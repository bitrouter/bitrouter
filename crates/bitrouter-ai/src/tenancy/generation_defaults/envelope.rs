//! Decoding and hashing preserve opaque JSON semantics without protocol promotion.
use std::{collections::BTreeMap, io::Write};

use serde::{
    Deserializer, Serialize, Serializer,
    ser::{SerializeMap, SerializeSeq, SerializeStruct},
};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

use super::{BODY_MEMBER, ENVELOPE_BODY_MEMBER, GenerationDefaultsEnvelopeV1, MAX_DEFAULTS_BYTES};
use crate::tenancy::{
    identity::Digest,
    validation::{Validate, ValidationError, bytes, require},
};

pub(super) fn supplemental<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, Value>, D::Error> {
    crate::tenancy::json::object(d, 3)
}

impl Validate for GenerationDefaultsEnvelopeV1 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            self.schema_version == 1,
            "generation defaults envelope version",
        )?;
        validate_parts(self.canonical.as_ref(), &self.supplemental)?;
        bytes(self, MAX_DEFAULTS_BYTES)
    }
}

pub(super) fn validate_parts(
    canonical: Option<&super::CanonicalGenerationDefaultsV1>,
    supplemental: &BTreeMap<String, Value>,
) -> Result<(), ValidationError> {
    if let Some(canonical) = canonical {
        canonical.validate()?;
    }
    for key in supplemental.keys() {
        require(
            !matches!(
                key.as_str(),
                "model"
                    | "messages"
                    | "stream"
                    | "temperature"
                    | "top_p"
                    | "max_tokens"
                    | "reasoning_effort"
                    | "top_k"
                    | "seed"
                    | "stop"
                    | "presence_penalty"
                    | "frequency_penalty"
                    | BODY_MEMBER
                    | ENVELOPE_BODY_MEMBER
            ),
            "reserved supplemental parameter",
        )?;
    }
    // Reserve the enclosing request, envelope and supplemental objects.
    // serde_json's default recursion budget rejects the 128th container.
    // Check programmatic Values before recursive hashing/serialization.
    for value in supplemental.values() {
        crate::tenancy::validation::json_depth(value, 3, "generation defaults nesting")?;
    }
    Ok(())
}

pub(super) struct Sorted<'a>(pub(super) &'a Value);
impl Serialize for Sorted<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Value::Object(values) => {
                let sorted: BTreeMap<_, _> = values.iter().collect();
                let mut map = serializer.serialize_map(Some(values.len()))?;
                for (key, value) in sorted {
                    map.serialize_entry(key, &Sorted(value))?;
                }
                map.end()
            }
            Value::Array(values) => {
                let mut seq = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    seq.serialize_element(&Sorted(value))?;
                }
                seq.end()
            }
            value => value.serialize(serializer),
        }
    }
}

struct Normalized<'a>(&'a GenerationDefaultsEnvelopeV1);
impl Serialize for Normalized<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut object = serializer.serialize_struct(
            "GenerationDefaultsEnvelopeV1",
            if self.0.canonical.is_some() { 3 } else { 2 },
        )?;
        object.serialize_field("schema_version", &self.0.schema_version)?;
        if let Some(canonical) = &self.0.canonical {
            object.serialize_field("canonical", canonical)?;
        }
        let values: BTreeMap<_, _> = self
            .0
            .supplemental
            .iter()
            .map(|(key, value)| (key, Sorted(value)))
            .collect();
        object.serialize_field("supplemental", &values)?;
        object.end()
    }
}

impl GenerationDefaultsEnvelopeV1 {
    /// SHA-256 of `bitrouter.generation-defaults-envelope.v1\0` followed by
    /// compact JSON in schema_version, canonical (if present), supplemental
    /// order. Supplemental object keys are sorted recursively; arrays retain
    /// order and numbers use Serde JSON representation, not RFC 8785. The nested
    /// canonical encoding is unchanged. Suppressed defaults still bind identity.
    ///
    /// # Errors
    /// Rejects invalid, oversized or excessively nested defaults before hashing.
    pub fn digest(&self) -> Result<Digest, ValidationError> {
        self.validate()?;
        struct Sink(Sha256);
        impl Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.update(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut sink = Sink(Sha256::new());
        sink.0
            .update(b"bitrouter.generation-defaults-envelope.v1\0");
        require(
            serde_json::to_writer(&mut sink, &Normalized(self)).is_ok(),
            "generation defaults envelope serialization",
        )?;
        sink.0
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
            .try_into()
    }
}
