//! Optional Core-owned pipeline and hop observations beside original operations.
//!
//! Payload two does not change receipt V1/V2 or qualify a financial contract.
//! A context ID is observed independently of the receipt correlation, and Core
//! never claims ownership of a logical loop assembled by another process.
use serde::{Deserialize, Serialize};

use super::{
    execution::ExecutionReceipt,
    execution_v2::ExecutionReceiptV2,
    identity::{AttemptId, ExecutionId, InstallationId, PrincipalId, RevisionId, TenantId},
    provider_technical_facts::{
        FirstSemantic, MAX_BYTES, MAX_OPERATIONS, ProviderOperation, ProviderTechnicalFactsV1,
    },
    validation::{Validate, ValidationError, bytes, nullable, require, unique},
};

/// The direct owning-context finalization boundary, never a remote RPC clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineProfile {
    /// PipelineContext creation through its original pre-recorder freeze.
    CorePipelineFinalizedV1,
}

/// Ordinary SSE is the singleton case of this same owning processor boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamProfile {
    /// First opened upstream through owning merged-stream finalization.
    CorePipelineMergedStreamV1,
}

/// A precise observed boundary, not an assertion of legacy health equivalence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HopProfile {
    /// Original JSON outer hop callbacks, including intervening auth work.
    CoreJsonHopCallbacksV1,
    /// An unsuccessful stream open, before a stream wrapper could own it.
    CoreStreamOpenCallbacksV1,
    /// The original opened-stream notify scalar, before observer callbacks.
    CoreStreamHopNotifyV1,
}

/// What ended the observed hop; independent of execution/financial outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HopEnd {
    /// Snapshot preceded this hop's actual end. No duration is fabricated.
    Unobserved,
    /// The original callback completed; partial still distinguishes an unvalidated EOF.
    Completed,
    /// The original operation or stream wrapper observed an error.
    Failed,
    /// An opened stream was dropped before its end was observed.
    Dropped,
}

/// Only the actual Core-owned logical processor supplies this measurement.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct PipelineStream {
    /// Exact versioned first-open/merged observation profile.
    pub profile: StreamProfile,
    /// Actual closing arm that supplied the FIRST opened stream's start.
    #[serde(deserialize_with = "nullable")]
    pub source_attempt_id: Option<AttemptId>,
    /// Original final selected label's arm. This need not be the source turn
    /// or the last failed arm in the enclosing receipt.
    #[serde(deserialize_with = "nullable")]
    pub terminal_attempt_id: Option<AttemptId>,
    /// Existing first-open start through original stream finalization.
    #[serde(deserialize_with = "optional_millis")]
    pub upstream_duration_ms: Option<u64>,
    /// First semantic accepted by the owning MERGED processor before hooks.
    #[serde(deserialize_with = "nullable")]
    pub first_semantic: Option<FirstSemantic>,
    /// First to last merged semantic; one is zero, none is null.
    #[serde(deserialize_with = "optional_millis")]
    pub generation_duration_ms: Option<u64>,
    /// Incomplete measurement or missing original start/end-arm binding.
    pub partial: bool,
}

/// A finalized actual context. Absence is represented by a null whole object,
/// never by inventing request_duration=0 or streamed=false.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct PipelineScope {
    /// Existing context finalization, independent of provider operation timing.
    pub profile: PipelineProfile,
    /// Actual context string; not inferred from correlation/execution metadata.
    pub context_request_id: String,
    /// Existing finalized monotonic duration before settlement recorders.
    #[serde(deserialize_with = "super::number::full_integer")]
    pub request_duration_ms: u64,
    /// Actual owning settlement path, not a parent's or media-type mode.
    pub streamed: bool,
    /// Null for JSON or an unobserved stream. A normal single SSE and a Core
    /// server-tool merge use the same processor boundary; neither is Cloud's loop.
    #[serde(deserialize_with = "nullable")]
    pub stream: Option<PipelineStream>,
}

/// One real hop and its actual auth arm group, never grouped by provider name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct HopObservation {
    /// Source boundary. Consumers must establish eligibility for their reader.
    pub profile: HopProfile,
    /// Actual send-order arms within this one hop; unarmed hops have no row.
    pub attempt_ids: Vec<AttemptId>,
    /// Original final arm in the group, not a new selected-route assertion.
    pub closing_attempt_id: AttemptId,
    /// Original measured scalar, or null; never replaced by operation elapsed.
    #[serde(deserialize_with = "optional_millis")]
    pub duration_ms: Option<u64>,
    /// Exact observed completion/error/drop source, not receipt outcome.
    pub end: HopEnd,
    /// Prefix or missing final measurement; a failed fully timed hop need not
    /// be partial, while a dropped stream always is.
    pub partial: bool,
}

/// Explicit independent technical payload. Original operations keep their V1
/// meanings; all optional data shares ONE encoded 64KiB transfer/storage bound.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct ProviderTechnicalFactsV2 {
    /// Exactly two, unrelated to Core Git/native financial registration.
    #[serde(deserialize_with = "super::number::integer")]
    pub schema_version: u8,
    /// Original unchanged receipt format, one or two.
    #[serde(deserialize_with = "super::number::integer")]
    pub receipt_format: u8,
    /// Original owning identity; a new timing object cannot create it.
    pub installation_id: InstallationId,
    /// Original owning tenant.
    pub tenant_id: TenantId,
    /// Original authenticated principal.
    pub principal_id: PrincipalId,
    /// Original execution, independent of context_request_id/correlation.
    pub execution_id: ExecutionId,
    /// Original immutable profile revision.
    pub execution_revision: RevisionId,
    /// Original provider-operation observations, with unchanged V1 semantics.
    pub operations: Vec<ProviderOperation>,
    /// Actual finalized owning pipeline, or null if not retained/finalized.
    #[serde(deserialize_with = "nullable")]
    pub pipeline_scope: Option<PipelineScope>,
    /// Bounded original hops; empty is unobserved, not zero-duration samples.
    pub hop_observations: Vec<HopObservation>,
}

macro_rules! object_wire {
    ($($ty:ident),+)=>{$(
        impl Serialize for $ty {
            fn serialize<S:serde::Serializer>(&self,s:S)->Result<S::Ok,S::Error> {$ty::serialize(self,s)}
        }
        impl<'de> Deserialize<'de> for $ty {
            fn deserialize<D:serde::Deserializer<'de>>(d:D)->Result<Self,D::Error> {
                struct Object;
                impl<'de> serde::de::Visitor<'de> for Object {
                    type Value=$ty;
                    fn expecting(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {f.write_str("a Core technical scope JSON object")}
                    fn visit_map<M:serde::de::MapAccess<'de>>(self,m:M)->Result<Self::Value,M::Error> {$ty::deserialize(serde::de::value::MapAccessDeserializer::new(m))}
                }
                d.deserialize_map(Object)
            }
        }
    )+};
}
object_wire!(
    PipelineStream,
    PipelineScope,
    HopObservation,
    ProviderTechnicalFactsV2
);

fn optional_millis<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    struct Millis;
    impl<'de> serde::de::Visitor<'de> for Millis {
        type Value = Option<u64>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("null or an exact nonnegative integer")
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
    d.deserialize_option(Millis)
}

impl ProviderTechnicalFactsV2 {
    /// Compatibility view of the actual unchanged operation observations.
    /// This never updates retained bytes or relabels historical V1 data as V2.
    pub fn operation_view(&self) -> Option<ProviderTechnicalFactsV1> {
        if self.operations.is_empty() {
            return None;
        }
        Some(ProviderTechnicalFactsV1 {
            schema_version: 1,
            receipt_format: self.receipt_format,
            installation_id: self.installation_id.clone(),
            tenant_id: self.tenant_id.clone(),
            principal_id: self.principal_id.clone(),
            execution_id: self.execution_id.clone(),
            execution_revision: self.execution_revision.clone(),
            operations: self.operations.clone(),
        })
    }

    /// Check original V1 identity and actual arm order; never recompute a receipt.
    /// # Errors
    /// Rejects foreign identity/revision/format/arms or invalid technical values.
    pub fn validate_against(&self, r: &ExecutionReceipt) -> Result<(), ValidationError> {
        r.validate()?;
        self.against(
            1,
            (
                &r.installation_id,
                &r.tenant_id,
                &r.principal_id,
                &r.execution_id,
                &r.execution_revision,
            ),
            r.attempts.iter().map(|a| &a.attempt_id),
        )?;
        if let Some(v) = self.operation_view() {
            v.validate_against(r)?;
        }
        Ok(())
    }
    /// Check the unchanged V2 receipt and original ordered actual arms.
    /// # Errors
    /// Rejects foreign identity/revision/format/arms or invalid technical values.
    pub fn validate_against_v2(&self, r: &ExecutionReceiptV2) -> Result<(), ValidationError> {
        r.validate()?;
        self.against(
            2,
            (
                &r.installation_id,
                &r.tenant_id,
                &r.principal_id,
                &r.execution_id,
                &r.execution_revision,
            ),
            r.attempts.iter().map(|a| &a.attempt_id),
        )?;
        if let Some(v) = self.operation_view() {
            v.validate_against_v2(r)?;
        }
        Ok(())
    }
    fn against<'a>(
        &self,
        format: u8,
        identity: (
            &InstallationId,
            &TenantId,
            &PrincipalId,
            &ExecutionId,
            &RevisionId,
        ),
        arms: impl Iterator<Item = &'a AttemptId>,
    ) -> Result<(), ValidationError> {
        self.validate()?;
        require(
            self.receipt_format == format
                && identity
                    == (
                        &self.installation_id,
                        &self.tenant_id,
                        &self.principal_id,
                        &self.execution_id,
                        &self.execution_revision,
                    ),
            "technical scope original identity",
        )?;
        let arms: Vec<_> = arms.collect();
        let index = |id: &AttemptId| {
            arms.iter()
                .position(|a| *a == id)
                .ok_or(ValidationError("technical scope actual arm"))
        };
        if let Some(stream) = self.pipeline_scope.as_ref().and_then(|s| s.stream.as_ref()) {
            if let Some(id) = &stream.source_attempt_id {
                index(id)?;
            }
            if let Some(id) = &stream.terminal_attempt_id {
                index(id)?;
            }
            if let (Some(source), Some(terminal)) =
                (&stream.source_attempt_id, &stream.terminal_attempt_id)
            {
                require(
                    index(source)? <= index(terminal)?,
                    "logical stream original arm order",
                )?;
            }
        }
        let mut previous = None;
        for hop in &self.hop_observations {
            for id in &hop.attempt_ids {
                let position = index(id)?;
                require(
                    previous.is_none_or(|p| p < position),
                    "technical hop original order",
                )?;
                previous = Some(position);
            }
        }
        Ok(())
    }
}

impl Validate for ProviderTechnicalFactsV2 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            self.schema_version == 2 && matches!(self.receipt_format, 1 | 2),
            "technical scope schema/receipt",
        )?;
        let bound = if self.receipt_format == 1 {
            8
        } else {
            MAX_OPERATIONS
        };
        require(
            self.operations.len() <= bound && self.hop_observations.len() <= bound,
            "technical scope count",
        )?;
        require(
            !self.operations.is_empty()
                || self.pipeline_scope.is_some()
                || !self.hop_observations.is_empty(),
            "empty technical scope",
        )?;
        if let Some(v) = self.operation_view() {
            v.validate()?;
        }
        if let Some(scope) = &self.pipeline_scope {
            require(
                !scope.context_request_id.is_empty()
                    && scope.context_request_id.len() <= MAX_BYTES
                    && !scope.context_request_id.contains('\0'),
                "technical context identity",
            )?;
            require(
                scope.streamed || scope.stream.is_none(),
                "JSON logical stream",
            )?;
            if let Some(stream) = &scope.stream {
                require(
                    stream.first_semantic.is_some() == stream.generation_duration_ms.is_some(),
                    "logical semantic pair",
                )?;
                require(
                    stream.partial
                        || (stream.upstream_duration_ms.is_some()
                            && stream.source_attempt_id.is_some()
                            && stream.terminal_attempt_id.is_some()),
                    "logical stream complete binding",
                )?;
            }
        }
        let mut ids = Vec::new();
        for hop in &self.hop_observations {
            unique(&hop.attempt_ids, 1, bound)?;
            require(
                hop.attempt_ids.last() == Some(&hop.closing_attempt_id),
                "technical hop closing arm",
            )?;
            require(
                ids.len() + hop.attempt_ids.len() <= bound,
                "technical hop total arms",
            )?;
            ids.extend(hop.attempt_ids.iter());
            require(
                hop.partial || hop.duration_ms.is_some(),
                "technical hop missing duration",
            )?;
            require(
                !matches!(hop.end, HopEnd::Dropped | HopEnd::Unobserved) || hop.partial,
                "technical unfinished hop prefix",
            )?;
            require(
                hop.end != HopEnd::Unobserved || hop.duration_ms.is_none(),
                "unobserved hop has no duration",
            )?;
        }
        if !ids.is_empty() {
            unique(&ids, 1, bound)?;
        }
        bytes(self, MAX_BYTES)
    }
}

impl Validate for Option<ProviderTechnicalFactsV2> {
    fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Some(v) => v.validate(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(feature = "schema")]
impl schemars::JsonSchema for ProviderTechnicalFactsV2 {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ProviderTechnicalFactsV2".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        match serde_json::from_str::<serde_json::Value>(include_str!(
            "provider_technical_facts_v2/schema.json"
        ))
        .ok()
        .and_then(|v| schemars::Schema::try_from(v).ok())
        {
            Some(schema) => schema,
            None => schemars::Schema::from(false),
        }
    }
}
