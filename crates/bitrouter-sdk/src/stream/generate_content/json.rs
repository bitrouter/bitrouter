//! Native wire values retain original tokens and reject duplicate members.

use serde::{
    Deserialize, Deserializer,
    de::{DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use serde_json::Value;

pub(crate) struct UniqueValue(pub(crate) Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
        ValueSeed { parents: 0 }.deserialize(decoder)
    }
}

#[derive(Clone, Copy)]
struct ValueSeed {
    parents: usize,
}

impl<'de> DeserializeSeed<'de> for ValueSeed {
    type Value = UniqueValue;

    fn deserialize<D: Deserializer<'de>>(self, decoder: D) -> Result<Self::Value, D::Error> {
        // The caller owns the complete JSON/SSE input. Nested tokens borrow it;
        // only actual number tokens reach Number's deserializer, so literal
        // objects cannot be mistaken for serde's private numeric representation.
        let raw = <&serde_json::value::RawValue>::deserialize(decoder)?;
        if matches!(raw.get().as_bytes().first(), Some(b'-' | b'0'..=b'9')) {
            return serde_json::from_str::<serde_json::Number>(raw.get())
                .map(|number| UniqueValue(Value::Number(number)))
                .map_err(serde::de::Error::custom);
        }
        // Fresh token scanners must not reset the complete JSON depth budget.
        let parents = if matches!(raw.get().as_bytes().first(), Some(b'[' | b'{')) {
            let parents = self.parents + 1;
            if parents >= 128 {
                return Err(serde::de::Error::custom("native JSON nesting"));
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

            fn expecting(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                out.write_str("JSON with unique object members")
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueValue(Value::Null))
            }
            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UniqueValue(value.into()))
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(UniqueValue(value.into()))
            }
            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(UniqueValue(value.into()))
            }
            fn visit_seq<S: SeqAccess<'de>>(
                self,
                mut sequence: S,
            ) -> Result<Self::Value, S::Error> {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element_seed(ValueSeed {
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
                        return Err(serde::de::Error::custom("duplicate native JSON member"));
                    }
                    values.insert(
                        key,
                        map.next_value_seed(ValueSeed {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_values_survive_both_native_decode_boundaries()
    -> Result<(), Box<dyn std::error::Error>> {
        let raw = r#"{"amount":1.25,"large":18446744073709551616,"negative":-9223372036854775809,"nested":[null,true,-7,2.5e2]}"#;
        let expected: Value = serde_json::from_str(raw)?;
        let first: UniqueValue = serde_json::from_slice(raw.as_bytes())?;
        assert_eq!(first.0, expected);
        let second: UniqueValue = serde_json::from_str(raw)?;
        assert_eq!(second.0, expected);
        Ok(())
    }

    #[test]
    fn borrowed_tokens_keep_the_complete_container_depth_budget()
    -> Result<(), Box<dyn std::error::Error>> {
        for (open, close) in [("[", "]"), ("{\"nested\":", "}")] {
            for (depth, accepted) in [(127, true), (128, false), (256, false)] {
                let raw = format!("{}null{}", open.repeat(depth), close.repeat(depth));
                assert_eq!(serde_json::from_str::<UniqueValue>(&raw).is_ok(), accepted);
                assert_eq!(serde_json::from_str::<Value>(&raw).is_ok(), accepted);
            }
        }
        Ok(())
    }
}
