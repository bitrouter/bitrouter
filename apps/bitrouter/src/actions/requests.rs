//! Recent-request inspection shared by local CLI and remote control HTTP.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::daemon::{self, DaemonCommand, DaemonResponse};
use crate::metering::store::TimeWindow;
use crate::output::reports::requests::{
    DaemonView, MeteringAvailability, MeteringComponentAvailability, RequestFilterView,
    RequestReportData, RequestsReport,
};
use crate::paths::ConfigSource;

/// Maximum number of rows any interactive/read API returns.
pub const MAX_REQUEST_ROWS: u64 = 500;

/// Maximum duration of a caller-supplied request interval.
pub const MAX_REQUEST_WINDOW_DAYS: i64 = 7;

const DAEMON_PROBE_TIMEOUT: Duration = Duration::from_millis(750);

/// Input accepted by local and remote request-inspection surfaces.
///
/// Leaving both time fields unset selects today from the server's UTC clock.
/// Supplying either one requires the other, so the database always receives an
/// explicit half-open interval rather than a client-local relative time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestFilters {
    /// Maximum rows to return. The action validates `1..=500`.
    #[serde(default = "default_request_limit")]
    pub limit: u64,
    /// Inclusive RFC3339 time bound, paired with `until`.
    pub since: Option<String>,
    /// Exclusive RFC3339 time bound, paired with `since`.
    pub until: Option<String>,
    /// Resolved model identifier to inspect.
    pub model: Option<String>,
    /// Serving provider identifier to inspect.
    pub provider: Option<String>,
}

impl Default for RequestFilters {
    fn default() -> Self {
        Self {
            limit: default_request_limit(),
            since: None,
            until: None,
            model: None,
            provider: None,
        }
    }
}

impl RequestFilters {
    /// Start from the default server-relative filter with an explicit page size.
    pub fn with_limit(limit: u64) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }

    fn resolve(&self) -> Result<ResolvedRequestFilters> {
        self.resolve_at(Utc::now())
    }

    fn resolve_at(&self, now: DateTime<Utc>) -> Result<ResolvedRequestFilters> {
        if !(1..=MAX_REQUEST_ROWS).contains(&self.limit) {
            anyhow::bail!("request limit must be between 1 and {MAX_REQUEST_ROWS}");
        }
        validate_optional_filter(self.model.as_deref(), "model")?;
        validate_optional_filter(self.provider.as_deref(), "provider")?;

        let (since, until, window) = match (&self.since, &self.until) {
            (None, None) => (utc_midnight(now), now, "today"),
            (Some(since), Some(until)) => {
                let since = parse_timestamp(since, "since")?;
                let until = parse_timestamp(until, "until")?;
                if since >= until {
                    anyhow::bail!("request since must precede until");
                }
                if until.signed_duration_since(since)
                    > chrono::Duration::days(MAX_REQUEST_WINDOW_DAYS)
                {
                    anyhow::bail!("request time range must not exceed seven days");
                }
                (since, until, "custom")
            }
            _ => anyhow::bail!("request since and until must be supplied together"),
        };

        Ok(ResolvedRequestFilters {
            window: TimeWindow::Custom {
                start: since,
                end: until,
            },
            window_label: window,
            since,
            until,
            model: self.model.clone(),
            provider: self.provider.clone(),
            limit: self.limit,
        })
    }
}

fn default_request_limit() -> u64 {
    MAX_REQUEST_ROWS
}

fn utc_midnight(now: DateTime<Utc>) -> DateTime<Utc> {
    now.date_naive().and_time(chrono::NaiveTime::MIN).and_utc()
}

fn parse_timestamp(value: &str, field: &str) -> Result<DateTime<Utc>> {
    if value.len() > 256 {
        anyhow::bail!("request {field} must be an RFC3339 timestamp");
    }
    DateTime::parse_from_rfc3339(value)
        .map(|timestamp| timestamp.with_timezone(&Utc))
        .map_err(|_| anyhow::anyhow!("request {field} must be an RFC3339 timestamp"))
}

fn validate_optional_filter(value: Option<&str>, field: &str) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        anyhow::bail!("request {field} filter must contain 1–256 bytes and no control characters");
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct ResolvedRequestFilters {
    window: TimeWindow,
    window_label: &'static str,
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    model: Option<String>,
    provider: Option<String>,
    limit: u64,
}

impl ResolvedRequestFilters {
    fn report_filters(&self) -> RequestFilterView {
        RequestFilterView::new(
            self.since.to_rfc3339(),
            self.until.to_rfc3339(),
            self.model.clone(),
            self.provider.clone(),
        )
    }
}

pub struct RequestsAction {
    source: ConfigSource,
    socket: PathBuf,
}

impl RequestsAction {
    pub fn new(source: ConfigSource, socket: PathBuf) -> Self {
        Self { source, socket }
    }

    /// Build one host-wide request snapshot.
    ///
    /// Metering reads remain best-effort as on the original local CLI: an
    /// unavailable store retains daemon state and labels every metering
    /// component unavailable rather than presenting placeholders as observed
    /// zero usage. Transport/auth/version failures are handled outside this
    /// action and remain hard errors for remote clients.
    pub async fn report(&self, limit: u64) -> Result<RequestsReport> {
        self.report_filtered(RequestFilters::with_limit(limit))
            .await
    }

    /// Build one host-wide filtered request snapshot.
    pub async fn report_filtered(&self, filters: RequestFilters) -> Result<RequestsReport> {
        let filters = filters.resolve()?;
        let daemon = daemon_view(&self.socket).await;
        let Some(store) = crate::metering::reader::open_readonly(&self.source).await else {
            return Ok(RequestsReport::new_filtered(
                daemon,
                RequestReportData {
                    window: filters.window_label.to_string(),
                    filters: filters.report_filters(),
                    summary: Default::default(),
                    rate: Default::default(),
                    rows: Vec::new(),
                    metering: MeteringAvailability::unavailable(),
                    truncated: false,
                },
            ));
        };
        let (summary, summary_availability) = match store
            .spend_summary_filtered(
                filters.window,
                filters.model.as_deref(),
                filters.provider.as_deref(),
            )
            .await
        {
            Ok(summary) => (summary, MeteringComponentAvailability::Available),
            Err(error) => {
                tracing::debug!(%error, "request-summary read failed");
                (
                    Default::default(),
                    MeteringComponentAvailability::Unavailable,
                )
            }
        };
        let (rate, rate_availability) = match store.get_total_rate().await {
            Ok(rate) => (rate, MeteringComponentAvailability::Available),
            Err(error) => {
                tracing::debug!(%error, "request-rate read failed");
                (
                    Default::default(),
                    MeteringComponentAvailability::Unavailable,
                )
            }
        };
        let (rows, truncated, rows_availability) = match store
            .recent_requests_filtered(
                filters.window,
                filters.model.as_deref(),
                filters.provider.as_deref(),
                filters.limit,
            )
            .await
        {
            Ok(page) => (
                page.rows,
                page.truncated,
                MeteringComponentAvailability::Available,
            ),
            Err(error) => {
                tracing::debug!(%error, "recent-request read failed");
                (
                    Vec::new(),
                    false,
                    MeteringComponentAvailability::Unavailable,
                )
            }
        };
        Ok(RequestsReport::new_filtered(
            daemon,
            RequestReportData {
                window: filters.window_label.to_string(),
                filters: filters.report_filters(),
                summary,
                rate,
                rows,
                metering: MeteringAvailability {
                    summary: summary_availability,
                    rate: rate_availability,
                    rows: rows_availability,
                },
                truncated,
            },
        ))
    }
}

async fn daemon_view(socket: &std::path::Path) -> Option<DaemonView> {
    let probe = daemon::send_command(socket, &DaemonCommand::Status);
    let Ok(response) = tokio::time::timeout(DAEMON_PROBE_TIMEOUT, probe).await else {
        return None;
    };
    match response {
        Ok(DaemonResponse::Status {
            pid,
            listen,
            models,
            ..
        }) => Some(DaemonView {
            pid,
            listen,
            models,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn empty_store_still_returns_an_empty_snapshot() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let action = RequestsAction::new(
            ConfigSource::Default {
                home: directory.path().to_path_buf(),
            },
            directory.path().join("missing.sock"),
        );
        let report = action.report(10).await?;
        assert!(report.rows.is_empty());
        assert!(report.daemon.is_none());
        assert_eq!(report.metering, MeteringAvailability::unavailable());
        Ok(())
    }

    #[tokio::test]
    async fn row_limit_is_bounded() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let action = RequestsAction::new(
            ConfigSource::Default {
                home: directory.path().to_path_buf(),
            },
            directory.path().join("missing.sock"),
        );
        assert!(action.report(0).await.is_err());
        assert!(action.report(MAX_REQUEST_ROWS + 1).await.is_err());
        Ok(())
    }

    #[test]
    fn filters_require_paired_bounded_rfc3339_times() -> anyhow::Result<()> {
        let now = DateTime::parse_from_rfc3339("2026-09-08T10:30:00Z")?.with_timezone(&Utc);
        let valid = RequestFilters {
            limit: 12,
            since: Some("2026-09-01T10:30:00+00:00".to_string()),
            until: Some("2026-09-08T10:30:00Z".to_string()),
            model: Some("gpt-5".to_string()),
            provider: Some("openai".to_string()),
        }
        .resolve_at(now)?;
        assert_eq!(valid.window_label, "custom");
        assert_eq!(valid.since.to_rfc3339(), "2026-09-01T10:30:00+00:00");
        assert_eq!(valid.until.to_rfc3339(), "2026-09-08T10:30:00+00:00");

        let unpaired = RequestFilters {
            since: Some("2026-09-01T10:30:00Z".to_string()),
            ..RequestFilters::default()
        };
        assert!(unpaired.resolve_at(now).is_err());

        let too_wide = RequestFilters {
            since: Some("2026-09-01T10:30:00Z".to_string()),
            until: Some("2026-09-08T10:30:01Z".to_string()),
            ..RequestFilters::default()
        };
        assert!(too_wide.resolve_at(now).is_err());

        let malformed = RequestFilters {
            since: Some("not-a-time".to_string()),
            until: Some("2026-09-08T10:30:00Z".to_string()),
            ..RequestFilters::default()
        };
        assert!(malformed.resolve_at(now).is_err());

        let unsafe_identifier = RequestFilters {
            model: Some("gpt-5\nignored".to_string()),
            ..RequestFilters::default()
        };
        assert!(unsafe_identifier.resolve_at(now).is_err());
        Ok(())
    }

    #[test]
    fn default_filters_resolve_to_server_today() -> anyhow::Result<()> {
        let now = DateTime::parse_from_rfc3339("2026-09-08T10:30:00Z")?.with_timezone(&Utc);
        let resolved = RequestFilters::with_limit(4).resolve_at(now)?;
        assert_eq!(resolved.window_label, "today");
        assert_eq!(resolved.since.to_rfc3339(), "2026-09-08T00:00:00+00:00");
        assert_eq!(resolved.until, now);
        assert_eq!(resolved.limit, 4);
        Ok(())
    }
}
