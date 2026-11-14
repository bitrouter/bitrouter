//! Optional provider-operation measurements, independent of receipt commitments.
//!
//! Schema one retains the provider operation's raw monotonic interval (including
//! its authentication recovery), not a physical HTTP/RPC span. JSON upstream
//! milliseconds truncate; stream upstream/first/last semantics use the original
//! positive-submillisecond helper. No clock is reconstructed during a read.
use serde::{Deserialize, Serialize};

use super::{
    execution::ExecutionReceipt,
    execution_v2::ExecutionReceiptV2,
    identity::{AttemptId, ExecutionId, InstallationId, PrincipalId, RevisionId, TenantId},
    validation::{Validate, ValidationError, bytes, nullable, require, unique},
};

/// Maximum encoded object size, charged before read/write transfer.
pub const MAX_BYTES: usize = 64 * 1024;
/// The original ordered receipt's maximum actual attempts also bounds operations.
pub const MAX_OPERATIONS: usize = 64;

/// First accepted provider-IR semantic delta, before output hooks/rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum FirstSemanticKind {
    /// Nonempty reasoning/thinking text.
    Reasoning,
    /// Nonempty visible text.
    Text,
    /// Nonempty tool name or arguments; a tool ID alone is not semantic.
    Tool,
}

/// A first semantic observation; duration and classification cannot be split.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct FirstSemantic {
    /// Provider operation start to the first accepted semantic delta.
    #[serde(deserialize_with = "super::number::full_integer")]
    pub ttft_ms: u64,
    /// Stable semantic classification; no generated content is retained.
    pub kind: FirstSemanticKind,
}

/// One real operation, identified only by its original actual-send attempts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct ProviderOperation {
    /// Actual arm IDs in send order. Auth recovery stays in this operation;
    /// another fallback/child operation is never merged by provider name.
    pub attempt_ids: Vec<AttemptId>,
    /// Actual selected attempt, if this operation was selected. This does not
    /// assert successful delivery, receipt completeness or financial authority.
    #[serde(deserialize_with = "nullable")]
    pub selected_attempt_id: Option<AttemptId>,
    /// The original pipeline's streamed clock path. A nonstreaming result that
    /// internally collects an SSE-only provider keeps its JSON clock path;
    /// this flag is not a claim about the provider HTTP media type.
    pub streamed: bool,
    /// Raw operation start to its original stop, or null if not observed.
    /// A successful submillisecond JSON measurement can be zero.
    #[serde(deserialize_with = "optional_millis")]
    pub upstream_duration_ms: Option<u64>,
    /// Absent before any semantic observation; JSON never manufactures TTFT.
    #[serde(deserialize_with = "nullable")]
    pub first_semantic: Option<FirstSemantic>,
    /// First to last semantic delta. One semantic delta is exactly zero;
    /// no semantic delta is null, independent of normalized usage counters.
    #[serde(deserialize_with = "optional_millis")]
    pub generation_duration_ms: Option<u64>,
    /// Only a prefix was observed (error/cancel/Drop/lost final measurement).
    /// This is not the receipt's usage-completeness or outcome classification.
    pub partial: bool,
}

/// Versioned optional observations bound to an unchanged receipt V1 or V2.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct ProviderTechnicalFactsV1 {
    /// Exactly one. A future schema requires its own compatible reader.
    #[serde(deserialize_with = "super::number::integer")]
    pub schema_version: u8,
    /// Original receipt format, exactly one or two.
    #[serde(deserialize_with = "super::number::integer")]
    pub receipt_format: u8,
    /// Original durable store identity.
    pub installation_id: InstallationId,
    /// Original owning tenant.
    pub tenant_id: TenantId,
    /// Original authenticated principal.
    pub principal_id: PrincipalId,
    /// Original execution; a correlation ID never substitutes for it.
    pub execution_id: ExecutionId,
    /// Original immutable routing/policy revision.
    pub execution_revision: RevisionId,
    /// Bounded actual operations; unarmed/reserved candidates have no record.
    pub operations: Vec<ProviderOperation>,
}

// Only the three new object DTOs use this direct map adapter. Serde's
// remote-Self methods retain typed fields/duplicate checks without a Value
// roundtrip, while ordered arrays cannot masquerade as closed wire objects.
macro_rules! object_wire {
    ($($ty:ident),+)=>{$(
        impl Serialize for $ty {
            fn serialize<S:serde::Serializer>(&self,serializer:S)->Result<S::Ok,S::Error> {
                $ty::serialize(self,serializer)
            }
        }
        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D:serde::Deserializer<'de>>(deserializer:D)->Result<Self,D::Error> {
                struct Object;
                impl<'de> serde::de::Visitor<'de> for Object {
                    type Value=$ty;
                    fn expecting(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {f.write_str("a provider technical JSON object")}
                    fn visit_map<M:serde::de::MapAccess<'de>>(self,map:M)->Result<Self::Value,M::Error> {
                        $ty::deserialize(serde::de::value::MapAccessDeserializer::new(map))
                    }
                }
                deserializer.deserialize_map(Object)
            }
        }
    )+};
}
object_wire!(FirstSemantic, ProviderOperation, ProviderTechnicalFactsV1);

fn optional_millis<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    struct Millis;
    impl<'de> serde::de::Visitor<'de> for Millis {
        type Value = Option<u64>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("null or an exact nonnegative u64 integer")
        }
        fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
            super::number::full_integer(d).map(Some)
        }
    }
    deserializer.deserialize_option(Millis)
}

impl Validate for ProviderTechnicalFactsV1 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(self.schema_version == 1, "provider technical schema")?;
        require(
            matches!(self.receipt_format, 1 | 2),
            "provider receipt format",
        )?;
        let bound = if self.receipt_format == 1 {
            8
        } else {
            MAX_OPERATIONS
        };
        require(
            !self.operations.is_empty() && self.operations.len() <= bound,
            "provider operations",
        )?;
        let mut all = Vec::new();
        let mut selected = 0;
        for operation in &self.operations {
            unique(&operation.attempt_ids, 1, bound)?;
            require(
                all.len() + operation.attempt_ids.len() <= bound,
                "provider total attempts",
            )?;
            all.extend(operation.attempt_ids.iter());
            if let Some(id) = &operation.selected_attempt_id {
                require(
                    operation.attempt_ids.last() == Some(id),
                    "selected provider attempt",
                )?;
                selected += 1;
            }
            require(
                operation.first_semantic.is_some() == operation.generation_duration_ms.is_some(),
                "provider semantic timing pair",
            )?;
            require(
                operation.streamed || operation.first_semantic.is_none(),
                "JSON provider semantic timing",
            )?;
            require(
                operation.partial || operation.upstream_duration_ms.is_some(),
                "completed provider operation timing",
            )?;
        }
        unique(&all, 1, bound)?;
        require(selected <= 1, "selected provider operation")?;
        // Separately rounded intervals are deliberately not added/compared.
        bytes(self, MAX_BYTES)
    }
}

impl Validate for Option<ProviderTechnicalFactsV1> {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Some(value) => value.validate(),
            None => Ok(()),
        }
    }
}

type ReceiptIdentity<'a> = (
    &'a InstallationId,
    &'a TenantId,
    &'a PrincipalId,
    &'a ExecutionId,
    &'a RevisionId,
);

impl ProviderTechnicalFactsV1 {
    /// Verify original identity and actual-attempt order against a V1 receipt.
    /// This never recomputes the Core-private receipt commitment or grants access.
    ///
    /// # Errors
    /// Rejects a foreign receipt, format, revision, missing/unarmed attempt or order.
    pub fn validate_against(&self, receipt: &ExecutionReceipt) -> Result<(), ValidationError> {
        receipt.validate()?;
        self.against(
            1,
            (
                &receipt.installation_id,
                &receipt.tenant_id,
                &receipt.principal_id,
                &receipt.execution_id,
                &receipt.execution_revision,
            ),
            receipt.attempts.iter().map(|attempt| &attempt.attempt_id),
        )
    }

    /// Verify identity and real ordinal-bearing attempts against a V2 receipt.
    /// Target/credential/ordinal facts remain in that original receipt.
    ///
    /// # Errors
    /// Rejects a foreign receipt or an unknown/duplicated/out-of-order attempt.
    pub fn validate_against_v2(&self, receipt: &ExecutionReceiptV2) -> Result<(), ValidationError> {
        receipt.validate()?;
        self.against(
            2,
            (
                &receipt.installation_id,
                &receipt.tenant_id,
                &receipt.principal_id,
                &receipt.execution_id,
                &receipt.execution_revision,
            ),
            receipt.attempts.iter().map(|attempt| &attempt.attempt_id),
        )
    }

    fn against<'a>(
        &self,
        format: u8,
        identity: ReceiptIdentity<'_>,
        attempts: impl Iterator<Item = &'a AttemptId>,
    ) -> Result<(), ValidationError> {
        self.validate()?;
        let (installation, tenant, principal, execution, revision) = identity;
        require(
            self.receipt_format == format
                && &self.installation_id == installation
                && &self.tenant_id == tenant
                && &self.principal_id == principal
                && &self.execution_id == execution
                && &self.execution_revision == revision,
            "provider technical receipt identity",
        )?;
        let attempts: Vec<_> = attempts.collect();
        let mut cursor = 0;
        for id in self
            .operations
            .iter()
            .flat_map(|operation| &operation.attempt_ids)
        {
            let offset = attempts[cursor..]
                .iter()
                .position(|actual| *actual == id)
                .ok_or(ValidationError("provider technical actual attempt"))?;
            cursor += offset + 1;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

#[cfg(feature = "schema")]
impl schemars::JsonSchema for ProviderTechnicalFactsV1 {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ProviderTechnicalFactsV1".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        match serde_json::from_str::<serde_json::Value>(include_str!(
            "provider_technical_facts/schema.json"
        ))
        .ok()
        .and_then(|value| schemars::Schema::try_from(value).ok())
        {
            Some(schema) => schema,
            None => schemars::Schema::from(false),
        }
    }
}
