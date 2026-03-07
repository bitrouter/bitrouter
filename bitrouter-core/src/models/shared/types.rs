//! Basic types shared across model kinds.

pub type JsonValue = serde_json::Value;
pub type JsonSchema = schemars::Schema;
pub type Record<K, V> = std::collections::HashMap<K, V>;
pub type TimestampMillis = i64;
