//! Publisher-owned discovery policy and identity audit payloads.

use std::collections::{BTreeMap, BTreeSet};

use registry_json::Value;
use serde::Serialize;

use crate::document::{RegistryError, canonical_id, timestamp};

/// Source policy for recommendation selection, separate from provider inventory.
#[derive(Debug, Default, Serialize)]
pub struct DiscoveryPolicy {
    /// Publisher-defined family selectors and retention rules.
    pub families: Vec<Value>,
    /// Evidence-backed exclusions from curated recommendations.
    #[serde(rename = "retired_models")]
    pub tombstones: Vec<Value>,
    /// Policy options retained without changing their numeric representation.
    #[serde(flatten)]
    pub options: BTreeMap<String, Value>,
}

impl DiscoveryPolicy {
    /// Decode the source while preserving exact numbers and opaque object keys.
    ///
    /// # Errors
    /// Rejects non-object sources and missing or non-array policy collections.
    pub fn from_value(source: &Value) -> Result<Self, RegistryError> {
        let mut options = source
            .as_object()
            .ok_or_else(|| RegistryError::invalid("", "Discovery policy must be an object"))?
            .clone();
        let Some(Value::Array(families)) = options.remove("families") else {
            return Err(RegistryError::invalid(
                "/families",
                "Discovery families must be an array",
            ));
        };
        let Some(Value::Array(tombstones)) = options.remove("retired_models") else {
            return Err(RegistryError::invalid(
                "/retired_models",
                "Curation tombstones must be an array",
            ));
        };
        Ok(Self {
            families,
            tombstones,
            options: options.into_iter().collect(),
        })
    }

    /// Read exclusions without granting authority to remove provider inventory.
    ///
    /// # Errors
    /// Rejects duplicate or noncanonical IDs and incomplete source evidence.
    pub fn tombstone_ids(&self) -> Result<BTreeSet<&str>, RegistryError> {
        let mut ids = BTreeSet::new();
        for (index, record) in self.tombstones.iter().enumerate() {
            let path = format!("/retired_models/{index}");
            let id = record["id"]
                .as_str()
                .filter(|id| canonical_id(id))
                .ok_or_else(|| RegistryError::invalid(&path, "Invalid curation tombstone ID"))?;
            let source = record["source_url"]
                .as_str()
                .and_then(|value| url::Url::parse(value).ok());
            if !ids.insert(id)
                || !source.is_some_and(|url| {
                    url.scheme() == "https"
                        && url.host_str().is_some()
                        && url.username().is_empty()
                        && url.password().is_none()
                })
                || record["recorded_at"].as_str().and_then(timestamp).is_none()
                || record["reason"].as_str().is_none_or(str::is_empty)
            {
                return Err(RegistryError::invalid(
                    path,
                    "Invalid curation tombstone evidence",
                ));
            }
        }
        Ok(ids)
    }
}

/// Publication diagnostics for a candidate's canonical identity bindings.
#[derive(Debug, Serialize)]
pub struct IdentityAudit<'a> {
    /// Whether all candidate references have valid identity evidence.
    pub outcome: &'static str,
    /// Number of candidate references checked.
    pub checked_references: usize,
    /// Number of distinct canonical IDs checked.
    pub distinct_checked_ids: usize,
    /// Native offers without a canonical binding.
    pub unbound_offers: usize,
    /// Invalid references, with their source paths and identities.
    pub mismatches: Vec<Value>,
    /// Ledger identities absent from the current complete observation.
    #[serde(rename = "historical_ids")]
    pub unobserved_ids: Vec<&'a str>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use registry_json::json;

    fn policy() -> DiscoveryPolicy {
        DiscoveryPolicy {
            tombstones: vec![json!({
                "id": "org/model",
                "source_url": "https://example.com/models",
                "recorded_at": "2026-10-22T00:00:00Z",
                "reason": "Publisher withdrew the model"
            })],
            ..Default::default()
        }
    }

    #[test]
    fn source_codec_preserves_exclusion_evidence_and_options()
    -> Result<(), Box<dyn std::error::Error>> {
        let input = crate::document::parse_json(
            br#"{
            "families": [],
            "retired_models": [{
                "id": "org/model",
                "source_url": "https://example.com/models",
                "recorded_at": "2026-10-22T00:00:00Z",
                "reason": "Publisher withdrew the model"
            }],
            "retain_versions": 2,
            "future": {
                "weight": 0.100000000000000000000000001,
                "opaque": {"$serde_json_lenient::private::Number": "text", "other": true}
            }
        }"#,
        )?;
        let decoded = DiscoveryPolicy::from_value(&input)?;
        assert_eq!(decoded.tombstone_ids()?, BTreeSet::from(["org/model"]));
        assert_eq!(
            decoded.options["future"]["weight"].to_string(),
            "0.100000000000000000000000001"
        );
        assert_eq!(registry_json::to_value(decoded)?, input);
        Ok(())
    }

    #[test]
    fn identity_audit_preserves_report_fields() -> Result<(), Box<dyn std::error::Error>> {
        let report = IdentityAudit {
            outcome: "passed",
            checked_references: 2,
            distinct_checked_ids: 1,
            unbound_offers: 0,
            mismatches: vec![],
            unobserved_ids: vec!["org/model"],
        };
        assert_eq!(
            registry_json::to_value(report)?,
            json!({
                "outcome": "passed",
                "checked_references": 2,
                "distinct_checked_ids": 1,
                "unbound_offers": 0,
                "mismatches": [],
                "historical_ids": ["org/model"]
            })
        );
        Ok(())
    }

    #[test]
    fn exclusions_require_unique_canonical_ids_and_source_evidence() {
        let mut duplicate = policy();
        duplicate.tombstones.push(duplicate.tombstones[0].clone());
        assert!(duplicate.tombstone_ids().is_err());
        for (field, value) in [
            ("id", "not-canonical"),
            ("source_url", "http://example.com/models"),
            ("source_url", "https://secret@example.com/models"),
            ("recorded_at", "not-a-timestamp"),
            ("reason", ""),
        ] {
            let mut invalid = policy();
            invalid.tombstones[0][field] = json!(value);
            assert!(invalid.tombstone_ids().is_err(), "{field}");
        }
    }
}
