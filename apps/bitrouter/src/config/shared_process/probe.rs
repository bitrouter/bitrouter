//! Probe-only collision handling; literal duplicate fields remain errors.

use serde::de::{DeserializeSeed, Error, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

pub(super) fn parse(input: &str, prefix: &str) -> Result<Value, serde_saphyr::Error> {
    // Validate merge sources before expansion can discard a duplicate field.
    // Repeated merge declarations remain legal; each source is still visited.
    let raw_options = serde_saphyr::options! {
        duplicate_keys: serde_saphyr::options::DuplicateKeyPolicy::LastWins,
        merge_keys: serde_saphyr::options::MergeKeyPolicy::AsOrdinary,
    };
    serde_saphyr::with_deserializer_from_str_with_options(input, raw_options, |de| {
        Probe {
            prefix,
            raw_merges: true,
        }
        .deserialize(de)
    })?;
    // LastWins passes duplicate map entries to our visitor, which permits only
    // keys containing this invocation's synthetic environment marker.
    // https://docs.rs/serde-saphyr/0.0.29/serde_saphyr/options/enum.DuplicateKeyPolicy.html
    let options = serde_saphyr::options! {
        duplicate_keys: serde_saphyr::options::DuplicateKeyPolicy::LastWins,
    };
    serde_saphyr::with_deserializer_from_str_with_options(input, options, |de| {
        Probe {
            prefix,
            raw_merges: false,
        }
        .deserialize(de)
    })
}

#[derive(Clone, Copy)]
struct Probe<'a> {
    prefix: &'a str,
    raw_merges: bool,
}

impl<'de> DeserializeSeed<'de> for Probe<'_> {
    type Value = Value;

    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<Value, D::Error> {
        de.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Probe<'_> {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a gateway configuration value")
    }

    fn visit_bool<E: Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: Error>(self, value: i64) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_u64<E: Error>(self, value: u64) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_f64<E: Error>(self, value: f64) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_str<E: Error>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.into()))
    }

    fn visit_string<E: Error>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E: Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_none<E: Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_some<D: serde::Deserializer<'de>>(self, de: D) -> Result<Value, D::Error> {
        self.deserialize(de)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(self)? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut values = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.contains_key(&key)
                && !key.contains(self.prefix)
                && !(self.raw_merges && key == "<<")
            {
                return Err(A::Error::custom("duplicate literal configuration field"));
            }
            values.insert(key, map.next_value_seed(self)?);
        }
        Ok(Value::Object(values))
    }
}
