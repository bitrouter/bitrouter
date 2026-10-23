//! Exact registry JSON is isolated from standard JSON's global Cargo features.
//! Standard Serde buffered enums cannot consume arbitrary-precision number maps:
//! <https://github.com/serde-rs/serde/issues/1463>
//! Only the exact AST/serializer uses the separate package; strict standard
//! RawValue traversal remains the protocol ingress, never its lenient parser.
//! <https://github.com/google/serde_json_lenient/blob/main/Cargo.toml>

use crate::document::{RegistryError, integer};
use registry_json::Value;
use serde::de::DeserializeOwned;

/// A one-way typed UI/runtime projection. This may round legacy floating-point
/// fields; never use it to rewrite a registry document or calculate its tariff.
pub fn project<T: DeserializeOwned>(value: &Value) -> Result<T, RegistryError> {
    let bytes = registry_json::to_vec(value)
        .map_err(|_| RegistryError::invalid("", "Registry projection cannot be encoded"))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| RegistryError::invalid("", "Registry projection cannot be decoded"))
}

/// Validation-only view. Normalize mathematically integral u64 values exactly
/// before using the standard schema validator; semantic integer/range checks
/// still inspect the original exact AST. This view never becomes public data.
pub(crate) fn schema_view(value: &Value) -> Result<serde_json::Value, RegistryError> {
    use serde_json::Value as Standard;
    Ok(match value {
        Value::Null => Standard::Null,
        Value::Bool(v) => Standard::Bool(*v),
        Value::String(v) => Standard::String(v.clone()),
        Value::Array(v) => Standard::Array(v.iter().map(schema_view).collect::<Result<_, _>>()?),
        Value::Object(v) => Standard::Object(
            v.iter()
                .map(|(k, v)| Ok((k.clone(), schema_view(v)?)))
                .collect::<Result<_, RegistryError>>()?,
        ),
        Value::Number(number) => {
            let number = if let Some(n) = integer(value) {
                serde_json::Number::from(n)
            } else if let Some(n) = number.as_i64() {
                serde_json::Number::from(n)
            } else {
                let approximate = number
                    .as_f64()
                    .ok_or_else(|| RegistryError::invalid("", "Invalid schema-view number"))?;
                // Exact zero was handled by integer(). Retain the sign of a
                // nonzero decimal below binary64's subnormal range, so schema
                // positivity checks cannot mistake a valid weight for zero.
                let approximate = if approximate == 0.0 {
                    f64::from_bits(1).copysign(approximate)
                } else {
                    approximate
                };
                serde_json::Number::from_f64(approximate)
                    .ok_or_else(|| RegistryError::invalid("", "Non-finite schema-view number"))?
            };
            Standard::Number(number)
        }
    })
}

/// Private persisted tariff evidence crosses JSON implementations as text.
/// Generic serialization would expose the exact serializer's private Number
/// protocol as object maps in ordinary serde_json output.
pub mod optional_document {
    use registry_json::Value;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        value: &Option<Value>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value
            .as_ref()
            .map(registry_json::to_string)
            .transpose()
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Value>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|text| {
                crate::document::parse_json(text.as_bytes()).map_err(serde::de::Error::custom)
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(tag = "kind")]
    enum Buffered {
        Score { score: f64 },
    }

    #[test]
    fn ordinary_json_buffered_floats_remain_independent() -> Result<(), Box<dyn std::error::Error>>
    {
        let value: Buffered = serde_json::from_str(r#"{"kind":"Score","score":0.8}"#)?;
        assert_eq!(value, Buffered::Score { score: 0.8 });
        Ok(())
    }

    #[derive(Serialize, Deserialize)]
    struct Evidence {
        #[serde(with = "optional_document")]
        price: Option<Value>,
    }

    #[test]
    fn private_evidence_crosses_standard_json_as_exact_text()
    -> Result<(), Box<dyn std::error::Error>> {
        let raw = crate::document::parse_json(br#"{"amount":0.123456789012345678901,"future":{"$serde_json_lenient::private::Number":"opaque","sibling":true},"other":{"$serde_json::private::Number":"opaque"}}"#)?;
        let serialized = serde_json::to_vec(&Evidence {
            price: Some(raw.clone()),
        })?;
        let ordinary: serde_json::Value = serde_json::from_slice(&serialized)?;
        assert!(ordinary["price"].is_string());
        let evidence: Evidence = serde_json::from_slice(&serialized)?;
        assert_eq!(evidence.price, Some(raw));
        Ok(())
    }

    #[test]
    fn strict_ingress_rejects_lenient_extensions() {
        for raw in [
            b"{/*comment*/\"a\":1}".as_slice(),
            b"{\"a\":1,}",
            b"[1,]",
            b"NaN",
        ] {
            assert!(crate::document::parse_json(raw).is_err());
        }
    }

    #[test]
    fn schema_view_preserves_underflow_sign_and_exact_zero() -> Result<(), RegistryError> {
        for (text, sign) in [
            ("0.00000000000000000001e-308", 1),
            ("-0.00000000000000000001e-308", -1),
            ("-0e-308", 0),
        ] {
            let exact = crate::document::parse_json(text.as_bytes())?;
            let view = schema_view(&exact)?;
            let number = view
                .as_f64()
                .ok_or_else(|| RegistryError::invalid("", "not a number"))?;
            assert_eq!(number.partial_cmp(&0.0), Some(sign.cmp(&0)));
        }
        Ok(())
    }
}
