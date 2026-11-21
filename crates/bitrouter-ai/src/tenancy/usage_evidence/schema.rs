//! Packaged schemas for the current usage observation contract.
use super::{ExecutionUsageEvidence, UsageEvidenceCapabilities};
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
impl JsonSchema for ExecutionUsageEvidence {
    fn schema_name() -> Cow<'static, str> {
        "ExecutionUsageEvidence".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        schema(include_str!("execution.schema.json"))
    }
}
impl JsonSchema for UsageEvidenceCapabilities {
    fn schema_name() -> Cow<'static, str> {
        "UsageEvidenceCapabilities".into()
    }
    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        schema(include_str!("capabilities.schema.json"))
    }
}
