use super::{
    errors::ServerResult,
    ids::{AccountId, ApiKeyId, RequestId},
    time::Timestamp,
};

#[derive(Debug, Clone)]
pub struct CreditAmount {
    /// Amount in units of 1/100th of a cent.
    /// For example, 100 = 1 cent, 10_000 = 1 dollar.
    pub microcents: u64,
}

#[derive(Debug, Clone)]
pub struct QuotaSnapshot {
    pub account_id: AccountId,
    pub used_credits: CreditAmount,
    pub limit_credits: Option<CreditAmount>,
    pub requests_this_minute: u32,
    pub rate_limit_per_minute: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct UsageEvent {
    pub request_id: RequestId,
    pub account_id: AccountId,
    pub key_id: ApiKeyId,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost: CreditAmount,
    pub recorded_at: Timestamp,
}

/// Records usage events.
pub trait UsageMeter {
    fn record(&self, event: UsageEvent) -> impl Future<Output = ServerResult<()>> + Send;
}

/// Enforces per-key or per-account rate limits.
pub trait RateLimiter {
    fn check_rate(
        &self,
        account_id: &AccountId,
        key_id: &ApiKeyId,
    ) -> impl Future<Output = ServerResult<()>> + Send;
}

/// Enforces spending caps.
pub trait SpendLimitChecker {
    fn check_spend(
        &self,
        account_id: &AccountId,
        key_id: &ApiKeyId,
    ) -> impl Future<Output = ServerResult<()>> + Send;
}
