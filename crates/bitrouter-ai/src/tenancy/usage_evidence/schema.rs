//! Dedicated schemas do not alter the frozen tenancy v1 definitions.
use super::{ExecutionUsageEvidenceV1, UsageEvidenceCapabilitiesV1};
use schemars::{JsonSchema, Schema, SchemaGenerator};
use std::borrow::Cow;

fn schema(document: &str) -> Schema {
    match serde_json::from_str::<serde_json::Value>(document)
        .ok()
        .and_then(|value| Schema::try_from(value).ok())
    {
        Some(schema) => schema,
        None => Schema::from(false),
    }
}
impl JsonSchema for ExecutionUsageEvidenceV1 {
    fn schema_name() -> Cow<'static, str> {
        "ExecutionUsageEvidenceV1".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        schema(include_str!("execution.schema.json"))
    }
}
impl JsonSchema for UsageEvidenceCapabilitiesV1 {
    fn schema_name() -> Cow<'static, str> {
        "UsageEvidenceCapabilitiesV1".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        schema(include_str!("capabilities.schema.json"))
    }
}
