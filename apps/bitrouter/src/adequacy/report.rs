use std::path::Path;

use bitrouter_sdk::config::AdequacyConfig;
use bitrouter_sdk::{BitrouterError, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::reliability::{
    ProviderReliabilityLedger, ReliabilityKey, ReliabilityObservation, ReliabilitySnapshot,
};
use super::store::PersistedReliabilityEvent;

const REPORT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReliabilityReportConfig {
    pub window_size: u32,
    pub consecutive_failure_threshold: u32,
    pub error_rate_percent: u32,
    pub cooldown_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReliabilityReportEvent {
    pub sequence: i64,
    pub request_id: String,
    pub route_key: String,
    pub endpoint: ReliabilityKey,
    pub observation: ReliabilityObservation,
    pub half_open_probe: bool,
    pub observed_at_unix: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReliabilityRouteReport {
    pub route_key: String,
    pub snapshot: ReliabilitySnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReliabilityEndpointReport {
    pub endpoint: ReliabilityKey,
    pub snapshot: ReliabilitySnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReliabilityReport {
    pub schema_version: u32,
    pub event_count: usize,
    pub event_sha256: String,
    pub config: ReliabilityReportConfig,
    pub routes: Vec<ReliabilityRouteReport>,
    pub endpoints: Vec<ReliabilityEndpointReport>,
    pub events: Vec<ReliabilityReportEvent>,
}

impl ReliabilityReport {
    pub fn build(config: &AdequacyConfig, rows: &[PersistedReliabilityEvent]) -> Result<Self> {
        let mut ordered = rows.to_vec();
        ordered.sort_by_key(|row| row.sequence);
        if ordered
            .windows(2)
            .any(|pair| pair[0].sequence == pair[1].sequence)
        {
            return Err(BitrouterError::bad_request(
                "reliability event sequence must be unique",
            ));
        }
        let events = ordered
            .iter()
            .map(|row| ReliabilityReportEvent {
                sequence: row.sequence,
                request_id: row.event.request_id.clone(),
                route_key: row.event.route_key.clone(),
                endpoint: row.event.endpoint_key.clone(),
                observation: row.event.observation,
                half_open_probe: row.event.half_open_probe,
                observed_at_unix: row.event.observed_at_unix,
            })
            .collect::<Vec<_>>();
        let canonical_events = serde_json::to_vec(&events).map_err(|error| {
            BitrouterError::internal(format!("serialize reliability events: {error}"))
        })?;
        let event_sha256 = format!("sha256:{}", hex::encode(Sha256::digest(&canonical_events)));
        let replay_events = ordered
            .into_iter()
            .map(|persisted| persisted.event)
            .collect::<Vec<_>>();
        let ledger = ProviderReliabilityLedger::replay(
            config.reliability_window_size as usize,
            config.reliability_consecutive_failures,
            config.reliability_error_rate_percent,
            config.reliability_cooldown_secs,
            &replay_events,
        )?;
        let snapshots = ledger.snapshots();
        let routes = snapshots
            .routes
            .into_iter()
            .map(|(route_key, snapshot)| ReliabilityRouteReport {
                route_key,
                snapshot,
            })
            .collect();
        let endpoints = snapshots
            .endpoints
            .into_iter()
            .map(|(endpoint, snapshot)| ReliabilityEndpointReport { endpoint, snapshot })
            .collect();
        Ok(Self {
            schema_version: REPORT_SCHEMA_VERSION,
            event_count: events.len(),
            event_sha256,
            config: ReliabilityReportConfig {
                window_size: config.reliability_window_size,
                consecutive_failure_threshold: config.reliability_consecutive_failures,
                error_rate_percent: config.reliability_error_rate_percent,
                cooldown_secs: config.reliability_cooldown_secs,
            },
            routes,
            endpoints,
            events,
        })
    }

    pub fn to_pretty_json(&self) -> Result<String> {
        let mut json = serde_json::to_string_pretty(self).map_err(|error| {
            BitrouterError::internal(format!("serialize reliability report: {error}"))
        })?;
        json.push('\n');
        Ok(json)
    }

    pub fn write(&self, path: &Path) -> Result<()> {
        let json = self.to_pretty_json()?;
        std::fs::write(path, json).map_err(|error| {
            BitrouterError::internal(format!(
                "write reliability report {}: {error}",
                path.display()
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adequacy::reliability::{ReliabilityEvent, ReliabilityKey, ReliabilityObservation};
    use crate::adequacy::store::PersistedReliabilityEvent;

    fn persisted_event(
        sequence: i64,
        request_id: &str,
        route_key: &str,
        provider: &str,
        observation: ReliabilityObservation,
    ) -> PersistedReliabilityEvent {
        PersistedReliabilityEvent {
            sequence,
            event: ReliabilityEvent {
                request_id: request_id.to_string(),
                route_key: route_key.to_string(),
                endpoint_key: ReliabilityKey {
                    provider: provider.to_string(),
                    model: "economy-model".to_string(),
                    credential_class: "default:oauth".to_string(),
                    endpoint_scope: "api.example.test:443".to_string(),
                    protocol: "responses".to_string(),
                },
                observation,
                half_open_probe: false,
                observed_at_unix: 100 + sequence as u64,
            },
        }
    }

    #[test]
    fn reliability_report_is_deterministic_sorted_and_content_free() {
        let config = AdequacyConfig {
            reliability_window_size: 23,
            reliability_consecutive_failures: 2,
            reliability_error_rate_percent: 35,
            reliability_cooldown_secs: 300,
            ..Default::default()
        };
        let rows = vec![
            persisted_event(
                2,
                "request-2",
                "z-provider:economy-model",
                "z-provider",
                ReliabilityObservation::Success,
            ),
            persisted_event(
                1,
                "request-1",
                "a-provider:economy-model",
                "a-provider",
                ReliabilityObservation::TransientFailure,
            ),
        ];

        let first = ReliabilityReport::build(&config, &rows).unwrap();
        let mut reversed = rows.clone();
        reversed.reverse();
        let second = ReliabilityReport::build(&config, &reversed).unwrap();
        let first_json = first.to_pretty_json().unwrap();
        let second_json = second.to_pretty_json().unwrap();

        assert_eq!(first_json, second_json);
        assert_eq!(first.event_sha256, second.event_sha256);
        assert_eq!(first.events[0].sequence, 1);
        assert_eq!(first.routes[0].route_key, "a-provider:economy-model");
        assert_eq!(first.endpoints[0].endpoint.provider, "a-provider");
        assert!(first.event_sha256.starts_with("sha256:"));
        assert!(first_json.ends_with('\n'));
        for forbidden in [
            "\"api_key\":",
            "\"prompt\":",
            "\"response\":",
            "\"command\":",
        ] {
            assert!(!first_json.contains(forbidden));
        }
    }
}
