//! Deterministic compilation of observed evidence into policy lock artifacts.

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::adequacy::reliability::ReliabilityEvent;
use crate::adequacy::store::{
    AdequacyStore, LegacyPin, PersistedExplorationState, PersistedReliabilityEvent,
    PersistedSemanticSuccess,
};

/// A point-in-time, ordered view of every pre-v2 learned-state table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LegacyAdequacySnapshot {
    pub snapshot_time_unix_ms: i64,
    pub pins: Vec<LegacyPin>,
    pub exploration: Vec<PersistedExplorationState>,
    pub semantic_successes: Vec<PersistedSemanticSuccess>,
    pub reliability_events: Vec<PersistedReliabilityEvent>,
}

impl LegacyAdequacySnapshot {
    pub async fn load(store: &AdequacyStore, snapshot_time_unix_ms: i64) -> Result<Self> {
        if snapshot_time_unix_ms < 0 {
            anyhow::bail!("legacy snapshot time cannot be negative");
        }
        let mut pins = store.load_pins().await?;
        let mut exploration = store.load_exploration_all().await?;
        let mut semantic_successes = store.load_semantic_successes().await?;
        let mut reliability_events = store.load_reliability_events().await?;
        pins.sort_by(|left, right| left.fingerprint.cmp(&right.fingerprint));
        exploration.sort_by(|left, right| left.fingerprint.cmp(&right.fingerprint));
        semantic_successes.sort_by(|left, right| left.evidence_id.cmp(&right.evidence_id));
        reliability_events
            .sort_by(|left, right| left.event.request_id.cmp(&right.event.request_id));
        Ok(Self {
            snapshot_time_unix_ms,
            pins,
            exploration,
            semantic_successes,
            reliability_events,
        })
    }

    pub fn semantic_digest(&self) -> Result<String> {
        #[derive(Serialize)]
        struct DigestInput<'a> {
            snapshot_time_unix_ms: i64,
            pins: &'a [LegacyPin],
            exploration: &'a [PersistedExplorationState],
            semantic_successes: &'a [PersistedSemanticSuccess],
            reliability_events: Vec<&'a ReliabilityEvent>,
        }

        let canonical = serde_json::to_vec(&DigestInput {
            snapshot_time_unix_ms: self.snapshot_time_unix_ms,
            pins: &self.pins,
            exploration: &self.exploration,
            semantic_successes: &self.semantic_successes,
            reliability_events: self
                .reliability_events
                .iter()
                .map(|row| &row.event)
                .collect(),
        })
        .context("serializing legacy adequacy snapshot")?;
        Ok(format!("sha256:{}", hex::encode(Sha256::digest(canonical))))
    }

    pub fn is_empty(&self) -> bool {
        self.pins.is_empty()
            && self.exploration.is_empty()
            && self.semantic_successes.is_empty()
            && self.reliability_events.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use crate::adequacy::reliability::{ReliabilityEvent, ReliabilityKey, ReliabilityObservation};
    use crate::adequacy::store::AdequacyStore;
    use crate::db;

    async fn populated_store(order: [&str; 2]) -> anyhow::Result<AdequacyStore> {
        let db = db::connect("sqlite::memory:").await?;
        db::run_migrations(&db).await?;
        let store = AdequacyStore::new(db);
        for key in order {
            let fingerprint = format!("auto\0agent_trace/v1|{key}|normal");
            store.upsert_pin(&fingerprint, 1_700_000_000).await?;
            store.upsert_exploration(&fingerprint, 8, 4, true).await?;
            store
                .record_semantic_success(&fingerprint, &format!("task-{key}"))
                .await?;
            store
                .append_reliability_event(&ReliabilityEvent {
                    request_id: format!("request-{key}"),
                    route_key: fingerprint,
                    endpoint_key: ReliabilityKey {
                        provider: "provider".into(),
                        model: format!("model-{key}"),
                        credential_class: "shared".into(),
                        endpoint_scope: "default".into(),
                        protocol: "responses".into(),
                    },
                    observation: ReliabilityObservation::Success,
                    half_open_probe: false,
                    observed_at_unix: 1_700_000_001,
                })
                .await?;
        }
        Ok(store)
    }

    #[tokio::test]
    async fn legacy_snapshot_digest_is_order_independent_and_complete() -> anyhow::Result<()> {
        let first = populated_store(["edit", "test"]).await?;
        let second = populated_store(["test", "edit"]).await?;

        let left = super::LegacyAdequacySnapshot::load(&first, 1_785_369_600_000).await?;
        let right = super::LegacyAdequacySnapshot::load(&second, 1_785_369_600_000).await?;

        assert_eq!(left.semantic_digest()?, right.semantic_digest()?);
        assert!(!left.is_empty());
        let mut incomplete = left.clone();
        incomplete.reliability_events.clear();
        assert_ne!(left.semantic_digest()?, incomplete.semantic_digest()?);
        Ok(())
    }
}
