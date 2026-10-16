//! Public observation schema and attribute vocabulary.
//!
//! The schema declares names, required fields and the extension region. Its
//! committed `span-schema.json` artifact ships with this crate. Hosts own event
//! dispatch and optional telemetry rendering; neither is required by this module.

pub mod schema;
pub mod span_attributes;
