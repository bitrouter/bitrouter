//! Private immutable row3 framing, not public HTTP JSON. Keeping observer JSON
//! opaque here isolates its semantic corruption from original V1/V2 reads.
use super::*;
use serde::{Deserialize, Serialize};
#[derive(Serialize, Deserialize)]
#[serde(remote = "Self", deny_unknown_fields)]
pub(super) struct Retained {
    schema_version: u8,
    pub scope: ProviderTechnicalFactsV2,
    observer_hops_json: String,
}
impl Serialize for Retained {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        Retained::serialize(self, s)
    }
}
impl<'de> Deserialize<'de> for Retained {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Object;
        impl<'de> serde::de::Visitor<'de> for Object {
            type Value = Retained;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a closed retained technical payload3 object")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                m: M,
            ) -> Result<Self::Value, M::Error> {
                Retained::deserialize(serde::de::value::MapAccessDeserializer::new(m))
            }
        }
        d.deserialize_map(Object)
    }
}
impl Retained {
    pub(super) fn from_facts(facts: &ProviderTechnicalFactsV3) -> Result<Self> {
        Ok(Self {
            schema_version: 3,
            scope: facts.scope.clone(),
            observer_hops_json: serde_json::to_string(&facts.observer_hops)?,
        })
    }
    pub(super) fn validate_scope(&self, terminal: &Terminal) -> Result<()> {
        ensure!(
            self.schema_version == 3 && self.observer_hops_json.len() <= MAX_BYTES,
            "retained observer framing"
        );
        match terminal {
            Terminal::V1(value) => self.scope.validate_against(&value.receipt)?,
            Terminal::V2(value) => self.scope.validate_against_v2(&value.receipt)?,
        };
        Ok(())
    }
    pub(super) fn into_facts(self, terminal: &Terminal) -> Result<ProviderTechnicalFactsV3> {
        let facts = ProviderTechnicalFactsV3 {
            schema_version: 3,
            scope: self.scope,
            observer_hops: serde_json::from_str(&self.observer_hops_json)?,
        };
        match terminal {
            Terminal::V1(value) => facts.validate_against(&value.receipt)?,
            Terminal::V2(value) => facts.validate_against_v2(&value.receipt)?,
        };
        Ok(facts)
    }
}

#[cfg(test)]
mod tests;
