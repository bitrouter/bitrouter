//! In-memory representation of a span before it is handed to the export
//! pipeline.
//!
//! Kept dependency-free so tests can construct and inspect spans without
//! pulling in the OpenTelemetry SDK. The follow-up commit that wires
//! `opentelemetry-otlp` will translate [`Span`] → `opentelemetry::trace::Span`
//! at the boundary inside `pipeline::Pipeline::export`.

use std::collections::BTreeMap;

/// Attribute value kinds supported by OTel attributes that BitRouter actually
/// emits. Numeric/boolean/string suffice for every GenAI semconv field.
#[derive(Debug, Clone, PartialEq)]
pub enum AttributeValue {
    String(String),
    I64(i64),
    F64(f64),
    Bool(bool),
    StringArray(Vec<String>),
}

impl From<&str> for AttributeValue {
    fn from(s: &str) -> Self {
        Self::String(s.to_owned())
    }
}

impl From<String> for AttributeValue {
    fn from(s: String) -> Self {
        Self::String(s)
    }
}

impl From<i64> for AttributeValue {
    fn from(v: i64) -> Self {
        Self::I64(v)
    }
}

impl From<u64> for AttributeValue {
    fn from(v: u64) -> Self {
        // OTel attributes are i64 — saturate on overflow rather than panic.
        Self::I64(i64::try_from(v).unwrap_or(i64::MAX))
    }
}

impl From<u32> for AttributeValue {
    fn from(v: u32) -> Self {
        Self::I64(i64::from(v))
    }
}

impl From<f64> for AttributeValue {
    fn from(v: f64) -> Self {
        Self::F64(v)
    }
}

impl From<bool> for AttributeValue {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

/// A single GenAI span in flight, ready for sampling/redact/export.
///
/// Uses a [`BTreeMap`] for attributes so iteration order is deterministic —
/// helpful both for snapshot tests and for stable log output during the
/// `tracing`-only export phase.
#[derive(Debug, Clone)]
pub struct Span {
    pub name: String,
    pub trace_id: Option<[u8; 16]>,
    pub parent_span_id: Option<[u8; 8]>,
    pub attributes: BTreeMap<String, AttributeValue>,
}

impl Span {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            trace_id: None,
            parent_span_id: None,
            attributes: BTreeMap::new(),
        }
    }

    /// Sets an attribute. Replaces any existing value at the same key.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<AttributeValue>) {
        self.attributes.insert(key.into(), value.into());
    }

    /// Sets an attribute when the value is `Some`. No-op otherwise.
    pub fn set_opt<V>(&mut self, key: impl Into<String>, value: Option<V>)
    where
        V: Into<AttributeValue>,
    {
        if let Some(v) = value {
            self.attributes.insert(key.into(), v.into());
        }
    }

    /// Removes attributes whose name appears in `redact`. Used by the
    /// per-destination redact pipeline.
    pub fn redact_attributes(&mut self, redact: &[String]) {
        for key in redact {
            self.attributes.remove(key);
        }
    }

    /// Returns the string value for an attribute, if present and a string.
    /// Used by the sampler to read `gen_ai.conversation.id` (and similar
    /// configurable sampling keys) without exposing `AttributeValue` to it.
    pub fn string_attr(&self, key: &str) -> Option<&str> {
        match self.attributes.get(key)? {
            AttributeValue::String(s) => Some(s.as_str()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_redact_roundtrip() {
        let mut s = Span::new("chat gpt-4o");
        s.set("gen_ai.request.model", "gpt-4o");
        s.set("gen_ai.usage.input_tokens", 1000_u32);
        s.set("gen_ai.input.messages", "raw user content");

        s.redact_attributes(&["gen_ai.input.messages".to_owned()]);

        assert_eq!(s.string_attr("gen_ai.request.model"), Some("gpt-4o"));
        assert!(!s.attributes.contains_key("gen_ai.input.messages"));
        assert!(matches!(
            s.attributes.get("gen_ai.usage.input_tokens"),
            Some(AttributeValue::I64(1000))
        ));
    }

    #[test]
    fn set_opt_skips_none() {
        let mut s = Span::new("chat gpt-4o");
        s.set_opt::<String>("gen_ai.response.id", None);
        s.set_opt("gen_ai.response.id", Some("resp_xyz"));
        assert_eq!(s.string_attr("gen_ai.response.id"), Some("resp_xyz"));
    }

    #[test]
    fn u64_overflow_saturates_to_i64_max() {
        let v: AttributeValue = u64::MAX.into();
        assert!(matches!(v, AttributeValue::I64(i) if i == i64::MAX));
    }

    #[test]
    fn string_attr_returns_none_for_non_string_value() {
        let mut s = Span::new("chat gpt-4o");
        s.set("gen_ai.usage.input_tokens", 100_u32);
        assert!(s.string_attr("gen_ai.usage.input_tokens").is_none());
    }
}
