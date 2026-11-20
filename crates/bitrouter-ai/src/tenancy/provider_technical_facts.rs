//! Bounded provider operation, pipeline, hop and observer measurements.
//! Observations bind to an execution receipt and grant no execution authority.
use super::{
    execution::ExecutionReceipt,
    execution_v2::ExecutionReceiptV2,
    identity::{AttemptId, ExecutionId, InstallationId, PrincipalId, RevisionId, TenantId},
    validation::{Validate, ValidationError, bytes, nullable, require, unique},
};
use serde::{Deserialize, Serialize};
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

/// The exact callback boundary that supplies a hop measurement.
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

/// Provider operations, owning pipeline, hop and observer measurements.
/// All categories share one encoded 64 KiB transfer and storage bound.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct ProviderTechnicalFacts {
    /// Current technical observation format, exactly three.
    #[serde(deserialize_with = "super::number::integer")]
    pub schema_version: u8,
    /// Owning receipt format, one or two.
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
    /// Provider-operation observations at their original measurement boundary.
    pub operations: Vec<ProviderOperation>,
    /// Actual finalized owning pipeline, or null if not retained/finalized.
    #[serde(deserialize_with = "nullable")]
    pub pipeline_scope: Option<PipelineScope>,
    /// Bounded original hops; empty is unobserved, not zero-duration samples.
    pub hop_observations: Vec<HopObservation>,
    /// Paired observer callbacks in actual hop order.
    pub observer_hops: Vec<ObserverHop>,
}

/// Exact observer callback source and quantization.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserverHopProfile {
    /// Inside the same observer's JSON start/end callbacks, integer floor.
    CoreJsonHopObserverFloorV1,
    /// Inside the same observer's failed stream-open callbacks, integer floor.
    CoreStreamOpenHopObserverFloorV1,
}
/// Only an actually paired, ended observer callback supplies a row. No callback
/// end means no row, never zero or an elapsed reconstructed from a receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct ObserverHop {
    /// Original callback boundary and quantization, independent of guard timing.
    pub profile: ObserverHopProfile,
    /// Exact actual send-order group, including original auth recovery arms.
    pub attempt_ids: Vec<AttemptId>,
    /// Exact last original arm in this group.
    pub closing_attempt_id: AttemptId,
    /// One original observer scalar: floor milliseconds, so positive sub-ms is 0.
    #[serde(deserialize_with = "super::number::full_integer")]
    pub duration_ms: u64,
    /// Completed means Generated callback, Failed means Failed callback. Neither
    /// grants execution/financial success. Other ends are invalid for this source.
    pub end: HopEnd,
}
// Direct map adapters retain typed field and duplicate checks without a Value
// roundtrip. Positional arrays cannot masquerade as closed wire objects.
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

object_wire!(
    FirstSemantic,
    ProviderOperation,
    PipelineStream,
    PipelineScope,
    HopObservation,
    ObserverHop,
    ProviderTechnicalFacts
);
impl ProviderTechnicalFacts {
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
        for id in self
            .operations
            .iter()
            .flat_map(|operation| &operation.attempt_ids)
        {
            let position = index(id)?;
            require(
                previous.is_none_or(|p| p < position),
                "provider operation original order",
            )?;
            previous = Some(position);
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

impl Validate for ProviderTechnicalFacts {
    fn validate(&self) -> Result<(), ValidationError> {
        require(
            self.schema_version == 3 && matches!(self.receipt_format, 1 | 2),
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
        validate_operations(&self.operations, bound)?;
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
        require(self.observer_hops.len() <= bound, "observer hop count")?;
        let mut previous = None;
        let mut ids = Vec::new();
        for observed in &self.observer_hops {
            unique(&observed.attempt_ids, 1, bound)?;
            require(
                observed.attempt_ids.last() == Some(&observed.closing_attempt_id),
                "observer closing arm",
            )?;
            require(
                matches!(observed.end, HopEnd::Completed | HopEnd::Failed),
                "observer actual callback end",
            )?;
            let expected = match observed.profile {
                ObserverHopProfile::CoreJsonHopObserverFloorV1 => {
                    HopProfile::CoreJsonHopCallbacksV1
                }
                ObserverHopProfile::CoreStreamOpenHopObserverFloorV1 => {
                    require(
                        observed.end == HopEnd::Failed,
                        "successful open uses notify only",
                    )?;
                    HopProfile::CoreStreamOpenCallbacksV1
                }
            };
            let position = self
                .hop_observations
                .iter()
                .position(|hop| {
                    hop.profile == expected
                        && hop.attempt_ids == observed.attempt_ids
                        && hop.closing_attempt_id == observed.closing_attempt_id
                })
                .ok_or(ValidationError("observer original hop group"))?;
            require(
                previous.is_none_or(|p| p < position),
                "observer original hop order",
            )?;
            previous = Some(position);
            require(
                ids.len() + observed.attempt_ids.len() <= bound,
                "observer original arm bound",
            )?;
            ids.extend(observed.attempt_ids.iter());
        }
        if !ids.is_empty() {
            unique(&ids, 1, bound)?;
        }
        bytes(self, MAX_BYTES)
    }
}

fn validate_operations(
    operations: &[ProviderOperation],
    bound: usize,
) -> Result<(), ValidationError> {
    let mut all = Vec::new();
    let mut selected = 0;
    for operation in operations {
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
    if !all.is_empty() {
        unique(&all, 1, bound)?;
    }
    require(selected <= 1, "selected provider operation")?;
    Ok(())
}
impl Validate for Option<ProviderTechnicalFacts> {
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
impl schemars::JsonSchema for ProviderTechnicalFacts {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ProviderTechnicalFacts".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        match serde_json::from_str::<serde_json::Value>(include_str!(
            "provider_technical_facts/schema.json"
        ))
        .ok()
        .and_then(|v| schemars::Schema::try_from(v).ok())
        {
            Some(v) => v,
            None => schemars::Schema::from(false),
        }
    }
}
