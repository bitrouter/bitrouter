//! Reports contain bounded identifiers and sanitized failure codes, never auth
//! values, response bodies, or transport error strings.

use std::fmt::Write;

use serde::Serialize;

use super::{
    capabilities::{Capabilities, capabilities},
    config::{ConfigError, ConnectorConfig, ObservationConfig},
    observer::{ObservationStatus, PoolSnapshot, ScopeEvidence, SourceFreshness, scope_evidence},
};

#[derive(Clone, Debug, Serialize)]
pub struct DryRunMember {
    pub member_id: String,
    pub declared_engines: Vec<String>,
    pub metrics_authentication: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct DryRunReport {
    pub version: u32,
    pub pool_id: String,
    pub model: String,
    pub validation: &'static str,
    pub network_requests: u32,
    pub scope_evidence: ScopeEvidence,
    pub capabilities: Capabilities,
    pub observation: ObservationConfig,
    pub members: Vec<DryRunMember>,
}

/// Render only validated configuration. No network or environment reads occur.
pub fn dry_run(config: &ConnectorConfig) -> Result<DryRunReport, ConfigError> {
    config.validate()?;
    Ok(DryRunReport {
        version: config.version,
        pool_id: config.pool.id.clone(),
        model: config.pool.model.clone(),
        validation: "configuration_only",
        network_requests: 0,
        scope_evidence: scope_evidence(),
        capabilities: capabilities(),
        observation: config.observation.clone(),
        members: config
            .pool
            .members
            .iter()
            .map(|member| DryRunMember {
                member_id: member.id.clone(),
                declared_engines: member.engines.clone(),
                metrics_authentication: if member.metrics_bearer_env.is_some() {
                    "environment_bearer"
                } else {
                    "none"
                },
            })
            .collect(),
    })
}

pub fn render_dry_run_human(report: &DryRunReport) -> String {
    let mut output = format!(
        "Connector configuration valid: pool {} / model {}\n",
        report.pool_id, report.model
    );
    let _ = writeln!(
        output,
        "Declared direct members: {}. No network requests made.",
        report.members.len()
    );
    for member in &report.members {
        let _ = writeln!(
            output,
            "  {}: engines [{}], metrics auth {}",
            member.member_id,
            member.declared_engines.join(", "),
            member.metrics_authentication
        );
    }
    output.push_str("Coverage is an operator declaration. Physical membership, router binding, and inference health are unverified.\nObservation only; no capacity estimate or admission control.\n");
    output
}

pub fn render_human(snapshot: &PoolSnapshot) -> String {
    let mut output = format!(
        "Pool {} / model {}: {}/{} declared members observed\n",
        snapshot.pool_id,
        snapshot.model,
        snapshot.summary.observed_members,
        snapshot.summary.declared_members
    );
    for member in &snapshot.members {
        let status = match member.status {
            ObservationStatus::Observed => "observed",
            ObservationStatus::Unknown => "unknown",
            ObservationStatus::Stale => "stale",
        };
        let _ = write!(output, "  {}: {status}", member.member_id);
        if let Some(reason) = &member.reason {
            let _ = write!(output, " ({reason:?})");
        }
        if let Some(metrics) = &member.metrics {
            let _ = write!(
                output,
                ", running {}, waiting {}, max engine KV {:.3}",
                metrics.running_requests,
                metrics.waiting_requests,
                metrics.kv_cache_usage_fraction_max
            );
        }
        let freshness = match member.freshness.source {
            SourceFreshness::ExplicitTimestamps => "explicit source timestamps",
            SourceFreshness::CollectionOnly => "collection only; source sample age unverified",
            SourceFreshness::PartialTimestamps => {
                "partial source timestamps; full sample age unverified"
            }
            SourceFreshness::Unknown => "source sample age unknown",
        };
        let _ = writeln!(output, "; {freshness}");
    }
    if !snapshot.is_ready() {
        output.push_str("Pool totals unavailable: declared-scope observation is incomplete.\n");
    }
    output.push_str("Coverage is operator-declared; physical membership, router binding, and inference health are unverified.\nObserved utilization is not idle capacity, a reservation, or an admission guarantee.\n");
    output
}
