//! Observer-local callback clocks, independent of older callback guard clocks.
//! This technical response cannot create a send, source, or financial authority.
use super::{
    execution::ExecutionReceipt,
    execution_v2::ExecutionReceiptV2,
    identity::AttemptId,
    provider_technical_facts::{MAX_BYTES, MAX_OPERATIONS},
    provider_technical_facts_v2::{HopEnd, HopProfile, ProviderTechnicalFactsV2},
    validation::{Validate, ValidationError, bytes, require, unique},
};
use serde::{Deserialize, Serialize};

/// Exact private observer callback source; not a generic LegacyHop permission.
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
/// Third technical wire. Old scope remains its own unchanged schema-two value;
/// distinct measurement categories may refer to the same actual arm group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub struct ProviderTechnicalFactsV3 {
    /// Exactly three; not a Core Git revision or native registration.
    #[serde(deserialize_with = "super::number::integer")]
    pub schema_version: u8,
    /// Actual original payload-two observations, not rewritten history.
    pub scope: ProviderTechnicalFactsV2,
    /// New observer data, empty when missing. Successful opened streams continue
    /// to use the old exact notify measurement, not a duplicate observer sample.
    pub observer_hops: Vec<ObserverHop>,
}
macro_rules! object_wire {($($ty:ident),+)=>{$(
 impl Serialize for $ty {fn serialize<S:serde::Serializer>(&self,s:S)->Result<S::Ok,S::Error>{$ty::serialize(self,s)}}
 impl<'de> Deserialize<'de> for $ty {fn deserialize<D:serde::Deserializer<'de>>(d:D)->Result<Self,D::Error>{
  struct Object;impl<'de> serde::de::Visitor<'de> for Object {type Value=$ty;
   fn expecting(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result{f.write_str("a closed technical observer JSON object")}
   fn visit_map<M:serde::de::MapAccess<'de>>(self,m:M)->Result<Self::Value,M::Error>{$ty::deserialize(serde::de::value::MapAccessDeserializer::new(m))}
  } d.deserialize_map(Object)
 }}
)+};}
object_wire!(ObserverHop, ProviderTechnicalFactsV3);
impl Validate for ProviderTechnicalFactsV3 {
    fn validate(&self) -> Result<(), ValidationError> {
        require(self.schema_version == 3, "technical observer schema")?;
        self.scope.validate()?;
        let bound = if self.scope.receipt_format == 1 {
            8
        } else {
            MAX_OPERATIONS
        };
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
                .scope
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
impl ProviderTechnicalFactsV3 {
    /// Bind both categories to the original receipt; category overlap is allowed.
    /// # Errors
    /// Rejects foreign identities, arms, order, format or unpaired observations.
    pub fn validate_against(&self, r: &ExecutionReceipt) -> Result<(), ValidationError> {
        self.validate()?;
        self.scope.validate_against(r)
    }
    /// Bind to the unchanged ordered receipt and original actual arm sequence.
    /// # Errors
    /// Rejects foreign identities, arms, order, format or unpaired observations.
    pub fn validate_against_v2(&self, r: &ExecutionReceiptV2) -> Result<(), ValidationError> {
        self.validate()?;
        self.scope.validate_against_v2(r)
    }
}
impl Validate for Option<ProviderTechnicalFactsV3> {
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
impl schemars::JsonSchema for ProviderTechnicalFactsV3 {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ProviderTechnicalFactsV3".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        match serde_json::from_str::<serde_json::Value>(include_str!(
            "provider_technical_facts_v3/schema.json"
        ))
        .ok()
        .and_then(|v| schemars::Schema::try_from(v).ok())
        {
            Some(v) => v,
            None => schemars::Schema::from(false),
        }
    }
}
