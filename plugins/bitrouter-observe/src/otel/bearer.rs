//! Live bearer resolution for authenticated OTLP exports.
//!
//! The exporter authenticates an OTLP/HTTP export with an
//! `Authorization: Bearer <token>` header. A *static* header (see
//! [`crate::otel::config::OtelConfig::bearer_token`]) suffices for a long-lived
//! collector key, but an account-attributed export uses a short-lived access
//! token that must be refreshed over the daemon's lifetime. This trait lets the
//! transport resolve a *fresh* bearer on every export without `bitrouter-observe`
//! taking a dependency on the cloud SDK that owns the credential store: the app
//! layer implements the trait and hands it in.

/// Resolves the bearer token attached to each authenticated OTLP export.
///
/// Implementations are expected to refresh-if-needed and be cheap to call on the
/// export path (the OTLP batch processor calls this once per export). Resolution
/// is **best-effort**: a `None` return (or an internal error mapped to `None`)
/// means the export proceeds *anonymously* rather than being dropped — telemetry
/// must never break for want of a token.
#[async_trait::async_trait]
pub trait TelemetryBearer: std::fmt::Debug + Send + Sync {
    /// Resolve the current bearer token for the telemetry export, refreshing
    /// if needed. `None` ⇒ export anonymously. Best-effort: never panics.
    async fn bearer(&self) -> Option<String>;
}
