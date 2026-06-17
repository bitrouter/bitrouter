//! Glob-prefix pattern matching for the registry-style provider schema.
//!
//! A provider's `api_protocol` / `rate_limits` are lists of `(pattern, value)`
//! where `pattern` is a **glob-prefix** — `*`, `prefix*`, or an exact literal
//!. Glob-prefix (not full regex) is chosen deliberately: "longest
//! literal prefix wins" gives a clean, total specificity ordering, which full
//! regex does not.
//!
//! Precedence for a model name: exact literal > longest `prefix*` > `*`.

use serde::Deserialize;

/// A single glob-prefix pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    /// `*` — matches anything; lowest specificity.
    Wildcard,
    /// `prefix*` — matches names starting with `prefix`; specificity = prefix length.
    Prefix(String),
    /// An exact literal — highest specificity.
    Exact(String),
}

impl Pattern {
    /// Parse a pattern string.
    pub fn parse(s: &str) -> Self {
        if s == "*" {
            Pattern::Wildcard
        } else if let Some(prefix) = s.strip_suffix('*') {
            Pattern::Prefix(prefix.to_string())
        } else {
            Pattern::Exact(s.to_string())
        }
    }

    /// Whether this pattern matches `name`.
    pub fn matches(&self, name: &str) -> bool {
        match self {
            Pattern::Wildcard => true,
            Pattern::Prefix(p) => name.starts_with(p.as_str()),
            Pattern::Exact(e) => e == name,
        }
    }

    /// Specificity score — higher wins. Exact beats any prefix; a longer prefix
    /// beats a shorter one; `*` is lowest.
    pub fn specificity(&self) -> usize {
        match self {
            Pattern::Wildcard => 0,
            // +1 so a zero-length prefix (`""*`, i.e. `*` written oddly) still
            // outranks Wildcard but stays below any real prefix.
            Pattern::Prefix(p) => p.len() + 1,
            Pattern::Exact(_) => usize::MAX,
        }
    }
}

/// An ordered list of `(pattern, value)` entries. Resolution picks the
/// most-specific matching pattern.
#[derive(Debug, Clone, Default)]
pub struct PatternMap<T> {
    entries: Vec<(Pattern, T)>,
}

impl<T> PatternMap<T> {
    /// An empty map.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Append a `(pattern, value)` entry.
    pub fn push(&mut self, pattern: Pattern, value: T) {
        self.entries.push((pattern, value));
    }

    /// Resolve `name` to the value of its most-specific matching pattern.
    pub fn resolve(&self, name: &str) -> Option<&T> {
        self.entries
            .iter()
            .filter(|(p, _)| p.matches(name))
            .max_by_key(|(p, _)| p.specificity())
            .map(|(_, v)| v)
    }

    /// Whether the map has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// YAML shape for a pattern list: `[ { "pattern": value }, ... ]`. Each map in
/// the list has exactly one key (the pattern) — order is preserved.
impl<'de, T> Deserialize<'de> for PatternMap<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw: Vec<std::collections::BTreeMap<String, T>> = Vec::deserialize(deserializer)?;
        let mut map = PatternMap::new();
        for entry in raw {
            for (pattern, value) in entry {
                map.push(Pattern::parse(&pattern), value);
            }
        }
        Ok(map)
    }
}

/// JSON Schema for the `[ { "pattern": value }, … ]` wire shape produced by the
/// [`Deserialize`] impl above: an array of single-key objects whose value is the
/// schema of `T`. Hand-written because the in-memory `PatternMap` (a `Vec` of
/// parsed `(Pattern, T)`) does not match the serialized shape.
impl<T: schemars::JsonSchema> schemars::JsonSchema for PatternMap<T> {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Owned(format!("PatternMap_for_{}", T::schema_name()))
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Owned(format!("PatternMap<{}>", T::schema_id()))
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let value = generator.subschema_for::<T>();
        schemars::json_schema!({
            "type": "array",
            "description": "Glob-prefix pattern list; each entry is a single-key \
                object mapping a pattern (`*`, `prefix*`, or an exact literal) to \
                its value.",
            "items": {
                "type": "object",
                "additionalProperties": value,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_classifies_patterns() {
        assert_eq!(Pattern::parse("*"), Pattern::Wildcard);
        assert_eq!(Pattern::parse("gpt-5*"), Pattern::Prefix("gpt-5".into()));
        assert_eq!(
            Pattern::parse("claude-sonnet-4-6"),
            Pattern::Exact("claude-sonnet-4-6".into())
        );
    }

    #[test]
    fn json_schema_is_array_of_single_key_objects() {
        // The schema must mirror the `Deserialize` wire shape, not the
        // in-memory `Vec<(Pattern, T)>`: an array whose items are objects with
        // `additionalProperties` of `T`'s schema.
        let schema = schemars::schema_for!(PatternMap<u32>);
        let value = serde_json::to_value(&schema).expect("schema serializes");
        assert_eq!(value["type"], "array");
        assert_eq!(value["items"]["type"], "object");
        assert!(
            value["items"]["additionalProperties"].is_object(),
            "items.additionalProperties should carry T's schema, got {value}"
        );
    }

    #[test]
    fn resolve_picks_most_specific() {
        let mut map: PatternMap<&str> = PatternMap::new();
        map.push(Pattern::Wildcard, "default");
        map.push(Pattern::parse("gpt-*"), "gpt");
        map.push(Pattern::parse("gpt-5*"), "gpt5");
        map.push(Pattern::parse("gpt-5-turbo"), "exact");

        assert_eq!(map.resolve("gpt-5-turbo"), Some(&"exact"));
        assert_eq!(map.resolve("gpt-5-mini"), Some(&"gpt5"));
        assert_eq!(map.resolve("gpt-4o"), Some(&"gpt"));
        assert_eq!(map.resolve("claude-x"), Some(&"default"));
    }
}
