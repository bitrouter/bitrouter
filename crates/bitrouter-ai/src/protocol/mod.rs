//! Public request wire contracts; encoding and cross-protocol adaptation are gateway code.

pub mod chat_completions;
pub mod messages;
pub mod responses;

#[cfg(all(test, feature = "schema"))]
mod schema_tests;

// Deriving Serialize makes nested defaults newly visible to schemars. Preserve
// the published request schemas, which predate client-side serialization.
// https://graham.cool/schemars/deriving/#transform
#[cfg(feature = "schema")]
fn omit_serialized_default(schema: &mut schemars::Schema) {
    schema.remove("default");
}
