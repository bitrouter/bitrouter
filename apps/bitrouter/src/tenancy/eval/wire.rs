//! Bounded JSON parsing with duplicate keys rejected before typed conversion.
use super::super::error::Failure;
use bitrouter_ai::tenancy::operation::ErrorCode;
use serde::{
    Deserialize, Deserializer,
    de::{self, DeserializeOwned, MapAccess, SeqAccess, Visitor},
};
use serde_json::Value;
use std::fmt;

struct Unique(Value);
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Json;
        impl<'de> Visitor<'de> for Json {
            type Value = Unique;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("JSON with unique object keys")
            }
            fn visit_unit<E: de::Error>(self) -> Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_bool<E: de::Error>(self, value: bool) -> Result<Unique, E> {
                Ok(Unique(Value::Bool(value)))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Unique, E> {
                Ok(Unique(value.into()))
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Unique, E> {
                Ok(Unique(value.into()))
            }
            fn visit_f64<E: de::Error>(self, value: f64) -> Result<Unique, E> {
                serde_json::Number::from_f64(value)
                    .map(|number| Unique(Value::Number(number)))
                    .ok_or_else(|| E::custom("invalid JSON number"))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Unique, E> {
                Ok(Unique(Value::String(value.into())))
            }
            fn visit_string<E: de::Error>(self, value: String) -> Result<Unique, E> {
                Ok(Unique(Value::String(value)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Unique, A::Error> {
                let mut values = Vec::new();
                while let Some(Unique(value)) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(Unique(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut object: A) -> Result<Unique, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some(key) = object.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(de::Error::custom("duplicate JSON object key"));
                    }
                    let Unique(value) = object.next_value()?;
                    values.insert(key, value);
                }
                Ok(Unique(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(Json)
    }
}
fn value(bytes: &[u8]) -> Result<Value, Failure> {
    let mut parser = serde_json::Deserializer::from_slice(bytes);
    let Unique(value) =
        Unique::deserialize(&mut parser).map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    parser
        .end()
        .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    Ok(value)
}
pub(super) fn parse<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, Failure> {
    serde_json::from_value(value(bytes)?).map_err(|_| ErrorCode::InvalidRequest.into())
}
pub(super) fn freeze(bytes: &[u8]) -> Result<Option<String>, Failure> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let value = value(bytes)?;
    if !value
        .as_object()
        .is_some_and(|fields| fields.keys().all(|key| key == "frozen_at"))
    {
        return Err(ErrorCode::InvalidRequest.into());
    }
    let request: bitrouter_ai::eval::FreezeRequest =
        serde_json::from_value(value).map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    if let Some(time) = &request.frozen_at {
        chrono::DateTime::parse_from_rfc3339(time)
            .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
    }
    Ok(request.frozen_at)
}
pub(super) fn size(value: &impl serde::Serialize, limit: usize) -> Result<(), Failure> {
    struct Counter {
        bytes: usize,
        limit: usize,
    }
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes) {
                return Err(std::io::Error::other("evaluation JSON exceeds bound"));
            }
            self.bytes += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(&mut Counter { bytes: 0, limit }, value)
        .map_err(|_| ErrorCode::CapacityExceeded.into())
}

/// Count every value, including repeated names, before parsing authority.
pub(super) fn headers(headers: &::http::HeaderMap) -> Result<(), Failure> {
    if headers.len() > 128 {
        return Err(ErrorCode::PayloadTooLarge.into());
    }
    let mut bytes = 0_usize;
    for (name, value) in headers {
        bytes = bytes
            .checked_add(name.as_str().len())
            .and_then(|sum| sum.checked_add(value.as_bytes().len()))
            .ok_or(ErrorCode::PayloadTooLarge)?;
        if bytes > 64 * 1024 {
            return Err(ErrorCode::PayloadTooLarge.into());
        }
    }
    Ok(())
}
