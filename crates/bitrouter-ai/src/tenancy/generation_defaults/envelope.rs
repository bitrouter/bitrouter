//! Decoding and hashing preserve opaque JSON semantics without protocol promotion.
use std::{collections::BTreeMap, io::Write};

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{MapAccess, SeqAccess, Visitor},
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
    struct Object;
    impl<'de> Visitor<'de> for Object {
        type Value = BTreeMap<String, Value>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a supplemental defaults object")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut values = BTreeMap::new();
            while let Some(key) = map.next_key::<String>()? {
                if values.contains_key(&key) {
                    return Err(serde::de::Error::custom("duplicate supplemental key"));
                }
                values.insert(key, map.next_value::<UniqueValue>()?.0);
            }
            Ok(values)
        }
    }
    d.deserialize_map(Object)
}

struct UniqueValue(Value);
impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Json;
        impl<'de> Visitor<'de> for Json {
            type Value = UniqueValue;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("JSON without duplicate keys")
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Null))
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(|n| UniqueValue(Value::Number(n)))
                    .ok_or_else(|| E::custom("invalid JSON number"))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Self::Value, S::Error> {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element::<UniqueValue>()? {
                    values.push(value.0);
                }
                Ok(UniqueValue(Value::Array(values)))
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut values = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(serde::de::Error::custom("duplicate supplemental key"));
                    }
                    values.insert(key, map.next_value::<UniqueValue>()?.0);
                }
                Ok(UniqueValue(Value::Object(values)))
            }
        }
        d.deserialize_any(Json)
    }
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
    fn depth(value: &Value, parents: usize) -> Result<(), ValidationError> {
        match value {
            Value::Array(values) => {
                require(parents + 1 < 128, "generation defaults nesting")?;
                for value in values {
                    depth(value, parents + 1)?;
                }
            }
            Value::Object(values) => {
                require(parents + 1 < 128, "generation defaults nesting")?;
                for value in values.values() {
                    depth(value, parents + 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    for value in supplemental.values() {
        depth(value, 3)?;
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
