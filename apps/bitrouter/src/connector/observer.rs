//! Bounded, read-only collection. No exporter payloads or transport error URLs
//! are copied into diagnostics.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use futures::future::join_all;
use reqwest::{Client, header::HeaderValue};
use serde::Serialize;
use thiserror::Error;

use super::{
    capabilities::{Capabilities, capabilities},
    config::{ConfigError, ConnectorConfig, MemberConfig, ObservationConfig},
    metrics::{self, MetricError},
};

#[derive(Debug, Error)]
pub enum ObserverError {
    #[error(transparent)]
    Configuration(#[from] ConfigError),
    #[error("could not initialize the connector HTTP client")]
    HttpClient,
}

#[derive(Clone, Debug, Serialize)]
pub struct ScopeEvidence {
    pub coverage: &'static str,
    pub physical_membership_verified: bool,
    pub router_binding_verified: bool,
    pub inference_health_verified: bool,
}

pub(super) fn scope_evidence() -> ScopeEvidence {
    ScopeEvidence {
        coverage: "operator_declared_static_members",
        physical_membership_verified: false,
        router_binding_verified: false,
        inference_health_verified: false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationStatus {
    Observed,
    Unknown,
    Stale,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum UnknownReason {
    Timeout,
    Network,
    Redirect,
    HttpStatus { status: u16 },
    BodyTooLarge,
    InvalidUtf8,
    AuthenticationUnavailable,
    MissingMetric,
    ScopeMismatch,
    MalformedMetric,
    NonFiniteMetric,
    OutOfRangeMetric,
    StaleSample,
    FutureSample,
    AmbiguousMetric,
    CollectionStale,
    ProcessRestart,
    ClockRegression,
}

impl From<MetricError> for UnknownReason {
    fn from(error: MetricError) -> Self {
        match error {
            MetricError::MissingMetric => Self::MissingMetric,
            MetricError::ScopeMismatch => Self::ScopeMismatch,
            MetricError::MalformedMetric => Self::MalformedMetric,
            MetricError::NonFiniteMetric => Self::NonFiniteMetric,
            MetricError::OutOfRangeMetric => Self::OutOfRangeMetric,
            MetricError::StaleSample => Self::StaleSample,
            MetricError::FutureSample => Self::FutureSample,
            MetricError::AmbiguousMetric => Self::AmbiguousMetric,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceFreshness {
    /// All required gauges carry explicit Prometheus timestamps; status and age
    /// determine whether they remain recent.
    ExplicitTimestamps,
    /// HTTP completed recently; exporter sample age is not established.
    CollectionOnly,
    /// Only a subset of gauges have timestamps; full sample age is unknown.
    PartialTimestamps,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
pub struct Freshness {
    pub collection_completed_at_ms: Option<i64>,
    pub collection_age_ms: Option<u64>,
    pub oldest_source_sample_at_ms: Option<i64>,
    pub oldest_source_sample_age_ms: Option<u64>,
    pub source: SourceFreshness,
}

#[derive(Clone, Debug, Serialize)]
pub struct MemberMetrics {
    pub running_requests: u64,
    pub waiting_requests: u64,
    /// Maximum per-engine fraction; no claim of a weighted pool-wide ratio.
    pub kv_cache_usage_fraction_max: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct MemberSnapshot {
    pub member_id: String,
    pub declared_engines: Vec<String>,
    pub metrics_scope_validated: bool,
    pub status: ObservationStatus,
    pub reason: Option<UnknownReason>,
    pub freshness: Freshness,
    pub metrics: Option<MemberMetrics>,
    /// Optional API-exporter process evidence, not an engine restart guarantee.
    pub process_started_at_seconds: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PoolSummary {
    pub declared_members: usize,
    pub observed_members: usize,
    /// Absent unless every declared member has a complete observation.
    pub metrics: Option<MemberMetrics>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PoolSnapshot {
    pub pool_id: String,
    pub model: String,
    pub observed_at_ms: i64,
    pub scope_evidence: ScopeEvidence,
    pub capabilities: Capabilities,
    pub members: Vec<MemberSnapshot>,
    pub summary: PoolSummary,
}

impl PoolSnapshot {
    /// Complete collection in the operator-declared scope at snapshot creation.
    /// Consumers must recheck age before using a retained snapshot later.
    /// This does not mean execution, idle capacity, or admission is ready.
    pub fn is_ready(&self) -> bool {
        self.summary.metrics.is_some()
            && self
                .members
                .iter()
                .all(|member| member.status == ObservationStatus::Observed)
    }
}

pub struct Observer {
    config: ConnectorConfig,
    client: Client,
    process_epochs: BTreeMap<String, f64>,
}

impl Observer {
    pub fn new(config: ConnectorConfig) -> Result<Self, ObserverError> {
        config.validate()?;
        let timeout = Duration::from_millis(config.observation.timeout_ms);
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .connect_timeout(timeout)
            .timeout(timeout)
            .build()
            .map_err(|_| ObserverError::HttpClient)?;
        Ok(Self {
            config,
            client,
            process_epochs: BTreeMap::new(),
        })
    }

    pub async fn observe(&mut self) -> PoolSnapshot {
        let results = join_all(self.config.pool.members.iter().map(|member| {
            collect(
                &self.client,
                member,
                &self.config.pool.model,
                &self.config.observation,
            )
        }))
        .await;
        let now = chrono::Utc::now().timestamp_millis();
        let max_age = self.config.observation.max_sample_age_ms;
        let mut members = Vec::with_capacity(results.len());
        for (mut member, completed_instant) in results {
            if let Some(epoch) = member.process_started_at_seconds
                && self
                    .process_epochs
                    .insert(member.member_id.clone(), epoch)
                    .is_some_and(|previous| previous != epoch)
            {
                invalidate(&mut member, UnknownReason::ProcessRestart);
            }
            let collection_age = completed_instant.map(|instant| {
                u64::try_from(instant.elapsed().as_millis()).map_or(u64::MAX, |millis| millis)
            });
            refresh_freshness(&mut member, now, collection_age, max_age);
            members.push(member);
        }
        let observed_members = members
            .iter()
            .filter(|member| member.status == ObservationStatus::Observed)
            .count();
        let metrics = if observed_members == members.len() {
            Some(MemberMetrics {
                running_requests: members
                    .iter()
                    .filter_map(|member| member.metrics.as_ref())
                    .map(|metrics| metrics.running_requests)
                    .sum(),
                waiting_requests: members
                    .iter()
                    .filter_map(|member| member.metrics.as_ref())
                    .map(|metrics| metrics.waiting_requests)
                    .sum(),
                kv_cache_usage_fraction_max: members
                    .iter()
                    .filter_map(|member| member.metrics.as_ref())
                    .map(|metrics| metrics.kv_cache_usage_fraction_max)
                    .fold(0.0, f64::max),
            })
        } else {
            None
        };
        PoolSnapshot {
            pool_id: self.config.pool.id.clone(),
            model: self.config.pool.model.clone(),
            observed_at_ms: now,
            scope_evidence: scope_evidence(),
            capabilities: capabilities(),
            summary: PoolSummary {
                declared_members: members.len(),
                observed_members,
                metrics,
            },
            members,
        }
    }
}

pub(super) fn refresh_freshness(
    member: &mut MemberSnapshot,
    now: i64,
    collection_age: Option<u64>,
    max_age: u64,
) {
    if let Some(completed) = member.freshness.collection_completed_at_ms {
        member.freshness.collection_age_ms = collection_age;
        if now < completed || collection_age.is_none() {
            invalidate(member, UnknownReason::ClockRegression);
        } else if collection_age.is_some_and(|age| age > max_age) {
            invalidate(member, UnknownReason::CollectionStale);
        }
    }
    if let Some(sample) = member.freshness.oldest_source_sample_at_ms {
        if now < sample {
            member.freshness.oldest_source_sample_age_ms = None;
            invalidate(member, UnknownReason::ClockRegression);
        } else {
            let age = now.saturating_sub(sample) as u64;
            member.freshness.oldest_source_sample_age_ms = Some(age);
            if age > max_age {
                invalidate(member, UnknownReason::StaleSample);
            }
        }
    }
}

fn invalidate(member: &mut MemberSnapshot, reason: UnknownReason) {
    member.status = if matches!(
        reason,
        UnknownReason::StaleSample | UnknownReason::CollectionStale
    ) {
        ObservationStatus::Stale
    } else {
        ObservationStatus::Unknown
    };
    member.metrics = None;
    member.reason = Some(reason);
}

async fn collect(
    client: &Client,
    member: &MemberConfig,
    model: &str,
    limits: &ObservationConfig,
) -> (MemberSnapshot, Option<Instant>) {
    let mut snapshot = MemberSnapshot {
        member_id: member.id.clone(),
        declared_engines: member.engines.clone(),
        metrics_scope_validated: false,
        status: ObservationStatus::Unknown,
        reason: None,
        freshness: Freshness {
            collection_completed_at_ms: None,
            collection_age_ms: None,
            oldest_source_sample_at_ms: None,
            oldest_source_sample_age_ms: None,
            source: SourceFreshness::Unknown,
        },
        metrics: None,
        process_started_at_seconds: None,
    };
    let text = match fetch(client, member, limits.max_body_bytes).await {
        Ok(text) => text,
        Err(reason) => {
            snapshot.reason = Some(reason);
            return (snapshot, None);
        }
    };
    let completed_instant = Instant::now();
    let now = chrono::Utc::now().timestamp_millis();
    snapshot.freshness.collection_completed_at_ms = Some(now);
    match metrics::parse(&text, model, member, now, limits.max_sample_age_ms) {
        Ok(reading) => {
            snapshot.metrics_scope_validated = true;
            snapshot.status = ObservationStatus::Observed;
            snapshot.freshness.oldest_source_sample_at_ms = reading.oldest_sample_at_ms;
            snapshot.freshness.source = if reading.all_samples_timestamped {
                SourceFreshness::ExplicitTimestamps
            } else if reading.oldest_sample_at_ms.is_some() {
                SourceFreshness::PartialTimestamps
            } else {
                SourceFreshness::CollectionOnly
            };
            snapshot.process_started_at_seconds = reading.process_started_at_seconds;
            snapshot.metrics = Some(MemberMetrics {
                running_requests: reading.running,
                waiting_requests: reading.waiting,
                kv_cache_usage_fraction_max: reading.kv_fraction_max,
            });
        }
        Err(error) => invalidate(&mut snapshot, error.into()),
    }
    (snapshot, Some(completed_instant))
}

async fn fetch(
    client: &Client,
    member: &MemberConfig,
    max_body: usize,
) -> Result<String, UnknownReason> {
    let mut request = client
        .get(&member.metrics_endpoint)
        .header(reqwest::header::ACCEPT, "text/plain; version=0.0.4");
    if let Some(name) = &member.metrics_bearer_env {
        let secret = std::env::var(name).map_err(|_| UnknownReason::AuthenticationUnavailable)?;
        if secret.is_empty() {
            return Err(UnknownReason::AuthenticationUnavailable);
        }
        let mut value = HeaderValue::from_str(&format!("Bearer {secret}"))
            .map_err(|_| UnknownReason::AuthenticationUnavailable)?;
        value.set_sensitive(true);
        request = request.header(reqwest::header::AUTHORIZATION, value);
    }
    let mut response = request.send().await.map_err(transport_error)?;
    if response.status().is_redirection() {
        return Err(UnknownReason::Redirect);
    }
    if !response.status().is_success() {
        return Err(UnknownReason::HttpStatus {
            status: response.status().as_u16(),
        });
    }
    if response
        .content_length()
        .is_some_and(|length| length > max_body as u64)
    {
        return Err(UnknownReason::BodyTooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
        if chunk.len() > max_body.saturating_sub(body.len()) {
            return Err(UnknownReason::BodyTooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).map_err(|_| UnknownReason::InvalidUtf8)
}

fn transport_error(error: reqwest::Error) -> UnknownReason {
    if error.is_timeout() {
        UnknownReason::Timeout
    } else {
        UnknownReason::Network
    }
}
