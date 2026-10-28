//! Public model wire contracts; encoding and cross-protocol adaptation are gateway code.
//! Optional response/event fields retain three states: `None` is missing,
//! `Some(None)` is JSON null, and `Some(Some(value))` is a populated field.

pub mod chat_completions;
pub mod decisions;
pub mod messages;
pub mod responses;

// Preserve a present JSON null separately from a missing optional field.
pub(crate) fn present_nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    <Option<T> as serde::Deserialize>::deserialize(deserializer).map(Some)
}

#[cfg(all(test, feature = "schema"))]
mod schema_tests;

// Deriving Serialize makes nested defaults newly visible to schemars. Preserve
// the published request schemas, which predate client-side serialization.
// https://graham.cool/schemars/deriving/#transform
#[cfg(feature = "schema")]
fn omit_serialized_default(schema: &mut schemars::Schema) {
    schema.remove("default");
}

pub mod mcp_skills;
