//! Packaged ordered-contract definitions stay separate from frozen v1 schemas.

use std::{borrow::Cow, sync::OnceLock};

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde_json::Value;

use super::{
    AcceptedSelectionV2, CandidateDescriptorV1, CandidateManifestV1, GenerationProtocol,
    OrderedCandidateSelectionV1, OrderedCapabilitiesV2, OrderedDispatchBindingV2,
};
use crate::tenancy::{
    execution_v2::{AttemptV2, ExecutionReceiptV2},
    usage_evidence_v2::ExecutionUsageEvidenceV2,
};

fn contract(name: &str) -> Schema {
    static DOCUMENT: OnceLock<Option<Value>> = OnceLock::new();
    let document =
        DOCUMENT.get_or_init(|| serde_json::from_str(include_str!("wire-schema.json")).ok());
    let schema = document.as_ref().and_then(|document| {
        let mut definition = document.get("$defs")?.get(name)?.clone();
        inline(&mut definition, document, 0)?;
        Schema::try_from(definition).ok()
    });
    // A malformed packaged definition fails closed without panics or network I/O.
    schema.unwrap_or_else(|| Schema::from(false))
}

fn inline(value: &mut Value, document: &Value, depth: usize) -> Option<()> {
    if depth > 64 {
        return None;
    }
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                if object.len() != 1 {
                    return None;
                }
                *value = document.pointer(reference.strip_prefix('#')?)?.clone();
                inline(value, document, depth + 1)?;
            } else {
                for child in object.values_mut() {
                    inline(child, document, depth + 1)?;
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                inline(value, document, depth + 1)?;
            }
        }
        _ => {}
    }
    Some(())
}

macro_rules! contracts {
    ($($ty:ty),+ $(,)?) => {$(
        impl JsonSchema for $ty {
            fn schema_name() -> Cow<'static, str> { stringify!($ty).into() }
            fn schema_id() -> Cow<'static, str> {
                concat!("bitrouter_ai::tenancy::ordered::", stringify!($ty)).into()
            }
            fn json_schema(_: &mut SchemaGenerator) -> Schema { contract(stringify!($ty)) }
        }
    )+};
}

contracts! {
    GenerationProtocol, OrderedCandidateSelectionV1, AcceptedSelectionV2,
    CandidateDescriptorV1, CandidateManifestV1, OrderedCapabilitiesV2,
    OrderedDispatchBindingV2, AttemptV2, ExecutionReceiptV2, ExecutionUsageEvidenceV2,
}
