//! Optional schemas preserve required-null and conditional wire constraints that
//! a bare Rust derive cannot express. The distribution helper compares these
//! packaged definitions with the independent frozen contract and its fixtures.

use std::{borrow::Cow, sync::OnceLock};

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde_json::Value;

use super::{authority::*, configuration::*, execution::*, identity::*, operation::*};

fn contract(pointer: &str) -> Schema {
    static DOCUMENT: OnceLock<Option<Value>> = OnceLock::new();
    let document =
        DOCUMENT.get_or_init(|| serde_json::from_str(include_str!("wire-schema.json")).ok());
    let schema = document.as_ref().and_then(|document| {
        let mut value = document.pointer(pointer)?.clone();
        inline(&mut value, document, 0)?;
        Schema::try_from(value).ok()
    });
    // Invalid packaged schemas fail closed and are caught by fixture/schema CI;
    // schema generation never panics or fetches external references.
    schema.unwrap_or_else(|| Schema::from(false))
}

fn inline(value: &mut Value, document: &Value, depth: usize) -> Option<()> {
    if depth > 64 {
        return None;
    }
    match value {
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref").and_then(Value::as_str) {
                // The packaged schema is acyclic and uses only local $defs refs.
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
        Value::Array(array) => {
            for child in array {
                inline(child, document, depth + 1)?;
            }
        }
        _ => {}
    }
    Some(())
}

macro_rules! contracts {
    ($($ty:ty => $pointer:literal),+ $(,)?) => {$(
        impl JsonSchema for $ty {
            fn schema_name() -> Cow<'static, str> { stringify!($ty).into() }
            fn schema_id() -> Cow<'static, str> { concat!("bitrouter_ai::tenancy::", stringify!($ty)).into() }
            fn json_schema(_: &mut SchemaGenerator) -> Schema { contract($pointer) }
        }
    )+};
}

contracts! {
    InstallationId => "/$defs/Id", InstanceId => "/$defs/Id",
    TenantId => "/$defs/Id", PrincipalId => "/$defs/Id",
    CredentialId => "/$defs/Id", BindingId => "/$defs/Id",
    OwnerId => "/$defs/Id", ResourceId => "/$defs/Id",
    RevisionId => "/$defs/Id", OperationId => "/$defs/Id",
    ExecutionId => "/$defs/Id", AttemptId => "/$defs/Id",
    CorrelationId => "/$defs/Id", ProviderId => "/$defs/Id", AccountId => "/$defs/Id",
    AuthorityRevision => "/$defs/AuthorityRevision", Digest => "/$defs/Digest", Timestamp => "/$defs/Time",
    OwnerKind => "/$defs/ResourceKey/properties/owner_kind",
    ResourceKind => "/$defs/ResourceKey/properties/resource_kind",
    ResourceKey => "/$defs/ResourceKey", ResourceRef => "/$defs/ResourceRef",
    GrantScope => "/$defs/GrantScope", TenantRole => "/$defs/TenantRole", Permission => "/$defs/Permission",
    TenantState => "/$defs/TenantPut/properties/state", TenantPut => "/$defs/TenantPut",
    PrincipalPut => "/$defs/PrincipalPut", BindingPut => "/$defs/BindingPut",
    CredentialCreate => "/$defs/CredentialCreate", CredentialIssued => "/$defs/CredentialIssued",
    AuditReaderCreate => "/$defs/AuditReaderCreate", CredentialStatus => "/$defs/CredentialStatus",
    CredentialState => "/$defs/CredentialStatus/properties/state",
    Configuration => "/$defs/Configuration", ConfigurationPut => "/$defs/ConfigurationPut",
    ConfigurationRevision => "/$defs/ConfigurationRevision", Activation => "/$defs/Activation",
    ActiveRevision => "/$defs/ActiveRevision",
    GenerationDefaults => "/$defs/Configuration/properties/generation_defaults",
    Learning => "/$defs/Configuration/properties/learning", LearningMode => "/$defs/Configuration/properties/learning/properties/mode",
    Limits => "/$defs/Configuration/properties/limits",
    Mutation => "/$defs/Mutation", OperationResult => "/$defs/OperationResult",
    OperationStatus => "/$defs/OperationResult/properties/status", Error => "/$defs/Error",
    ErrorCode => "/$defs/Error/properties/code", DispatchState => "/$defs/Error/properties/dispatch_state",
    TenantCapabilities => "/$defs/TenantCapabilities", DispatchBinding => "/$defs/DispatchBinding",
    Usage => "/$defs/Usage", Provenance => "/$defs/Usage/properties/provenance",
    Target => "/$defs/Target", NumericUsage => "/$defs/NumericUsage",
    UsageDimension => "/$defs/NumericUsage/properties/name", Attempt => "/$defs/Attempt",
    Outcome => "/$defs/Attempt/properties/outcome", ReceiptState => "/$defs/ExecutionReceipt/properties/state",
    ExecutionReceipt => "/$defs/ExecutionReceipt", ExecutionPage => "/$defs/ExecutionPage",
}
