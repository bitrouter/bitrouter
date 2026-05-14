//! Caller identity and funding context — shared library code (crate root).
//!
//! `CallerContext` is one of the shared sub-structs embedded by every
//! protocol's `*PipelineContext` (see 003 §0).

use serde::{Deserialize, Serialize};

/// How a caller pays for requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentMethod {
    /// Pre-funded credit balance.
    Credits,
    /// Micropayment protocol channel.
    Mpp,
    /// Caller brings their own provider key — BitRouter does not charge.
    Byok,
    /// Local / unauthenticated use (`server.skip_auth`).
    None,
}

/// Which funding source actually settled a request. Distinct from
/// [`PaymentMethod`] — a caller may be `Credits`-capable but a given request
/// could be BYOK (free) because a BYOK key was applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FundingSource {
    /// No charge was claimed by any `ChargeStrategy`.
    #[default]
    Unsettled,
    /// Charged against a credit balance.
    Credits,
    /// Settled through an MPP channel.
    Mpp,
    /// BYOK — free, no charge.
    Byok,
}

/// The authenticated (or synthesised) caller of a request.
///
/// Populated at Stage 0. Read-only for the rest of the pipeline. When
/// `server.skip_auth` is on and a request carries no credentials, the SDK
/// synthesises a local `CallerContext` with [`PaymentMethod::None`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallerContext {
    api_key_id: String,
    user_id: String,
    payment_method: PaymentMethod,
    /// Monthly spend ceiling in micro-USD, if the caller's policy sets one.
    spend_limit_micro_usd: Option<u64>,
    /// Requests-per-minute ceiling, if set.
    rpm_limit: Option<u32>,
    /// True when synthesised by `server.skip_auth` rather than authenticated.
    local: bool,
}

impl CallerContext {
    /// Construct a caller context from authenticated identity.
    pub fn new(
        api_key_id: impl Into<String>,
        user_id: impl Into<String>,
        payment_method: PaymentMethod,
    ) -> Self {
        Self {
            api_key_id: api_key_id.into(),
            user_id: user_id.into(),
            payment_method,
            spend_limit_micro_usd: None,
            rpm_limit: None,
            local: false,
        }
    }

    /// The synthesised local caller used when `server.skip_auth` is on.
    pub fn local() -> Self {
        Self {
            api_key_id: "local".to_string(),
            user_id: "local".to_string(),
            payment_method: PaymentMethod::None,
            spend_limit_micro_usd: None,
            rpm_limit: None,
            local: true,
        }
    }

    /// A pre-auth placeholder caller. Used when `skip_auth` is off — an
    /// `AuthHook` is expected to validate credentials and replace it via
    /// [`crate::language_model::PipelineContext::set_caller`]. If no `AuthHook`
    /// upgrades it, downstream hooks see an anonymous caller.
    pub fn anonymous() -> Self {
        Self {
            api_key_id: "anonymous".to_string(),
            user_id: "anonymous".to_string(),
            payment_method: PaymentMethod::None,
            spend_limit_micro_usd: None,
            rpm_limit: None,
            local: false,
        }
    }

    /// Whether this is the pre-auth anonymous placeholder.
    pub fn is_anonymous(&self) -> bool {
        !self.local && self.api_key_id == "anonymous"
    }

    /// Set the monthly spend limit (builder-style).
    pub fn with_spend_limit(mut self, micro_usd: u64) -> Self {
        self.spend_limit_micro_usd = Some(micro_usd);
        self
    }

    /// Set the requests-per-minute limit (builder-style).
    pub fn with_rpm_limit(mut self, rpm: u32) -> Self {
        self.rpm_limit = Some(rpm);
        self
    }

    /// The API key id this caller authenticated with.
    pub fn api_key_id(&self) -> &str {
        &self.api_key_id
    }

    /// The owning user id.
    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    /// The caller's payment method.
    pub fn payment_method(&self) -> PaymentMethod {
        self.payment_method
    }

    /// The caller's monthly spend ceiling, if any.
    pub fn spend_limit(&self) -> Option<u64> {
        self.spend_limit_micro_usd
    }

    /// The caller's RPM ceiling, if any.
    pub fn rpm_limit(&self) -> Option<u32> {
        self.rpm_limit
    }

    /// Whether this caller was synthesised locally (`skip_auth`).
    pub fn is_local(&self) -> bool {
        self.local
    }
}
