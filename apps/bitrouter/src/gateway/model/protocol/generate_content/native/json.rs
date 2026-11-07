//! Original response tokens, including literal private-looking object keys.
use serde::{
    Deserialize, Deserializer,
    de::{DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use serde_json::Value;
use std::collections::BTreeMap;

pub(super) fn decode(text: &str) -> Result<Value, serde_json::Error> {
    struct Object(Value);
    impl<'de> Deserialize<'de> for Object {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            object(d, 1).map(|entries| Self(Value::Object(entries.into_iter().collect())))
        }
    }
    serde_json::from_str::<Object>(text).map(|object| object.0)
}

pub(super) fn object<'de, D: Deserializer<'de>>(
    d: D,
    parents: usize,
) -> Result<BTreeMap<String, Value>, D::Error> {
    if parents >= 128 {
        return Err(serde::de::Error::custom("JSON nesting"));
    }
    struct Object {
        parents: usize,
    }
    impl<'de> Visitor<'de> for Object {
        type Value = BTreeMap<String, Value>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an opaque JSON object")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut values = BTreeMap::new();
            while let Some(key) = map.next_key::<String>()? {
                if values.contains_key(&key) {
                    return Err(serde::de::Error::custom("duplicate JSON key"));
                }
                // Preserve original numeric tokens through feature unification.
                // A second Value decode could interpret opaque private keys as
                // serde instructions instead of preserving supplemental data.
                let raw = map.next_value::<Box<serde_json::value::RawValue>>()?;
                let value = UniqueValueSeed {
                    parents: self.parents,
                }
                .deserialize(&mut serde_json::Deserializer::from_str(raw.get()))
                .map_err(serde::de::Error::custom)?
                .0;
                values.insert(key, value);
            }
            Ok(values)
        }
    }
    d.deserialize_map(Object { parents })
}

struct UniqueValue(Value);
#[derive(Clone, Copy)]
struct UniqueValueSeed {
    parents: usize,
}
impl<'de> DeserializeSeed<'de> for UniqueValueSeed {
    type Value = UniqueValue;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
        // Only the top-level object value owns a raw buffer. Nested
        // tokens borrow it, including when the outer DTO came from a Value.
        // https://docs.rs/serde_json/1.0.150/serde_json/value/struct.RawValue.html
        let raw = <&serde_json::value::RawValue>::deserialize(d)?;
        if matches!(raw.get().as_bytes().first(), Some(b'-' | b'0'..=b'9')) {
            return serde_json::from_str::<serde_json::Number>(raw.get())
                .map(|number| UniqueValue(Value::Number(number)))
                .map_err(serde::de::Error::custom);
        }
        // RawValue scanning has no recursion budget. Carry the complete-body
        // budget across fresh token decoders before recursive construction.
        let parents = if matches!(raw.get().as_bytes().first(), Some(b'[' | b'{')) {
            let parents = self.parents + 1;
            if parents >= 128 {
                return Err(serde::de::Error::custom("JSON nesting"));
            }
            parents
        } else {
            self.parents
        };
        struct Json {
            parents: usize,
        }
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
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Self::Value, S::Error> {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element_seed(UniqueValueSeed {
                    parents: self.parents,
                })? {
                    values.push(value.0);
                }
                Ok(UniqueValue(Value::Array(values)))
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut values = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(serde::de::Error::custom("duplicate JSON key"));
                    }
                    values.insert(
                        key,
                        map.next_value_seed(UniqueValueSeed {
                            parents: self.parents,
                        })?
                        .0,
                    );
                }
                Ok(UniqueValue(Value::Object(values)))
            }
        }
        serde_json::Deserializer::from_str(raw.get())
            .deserialize_any(Json { parents })
            .map_err(serde::de::Error::custom)
    }
}
