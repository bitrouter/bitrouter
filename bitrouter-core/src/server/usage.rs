use std::future::Future;

use super::{
    ids::{AccountId, ApiKeyId, RequestId},
    errors::Result,
    time::Timestamp,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CreditAmount(pub i64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaSnapshot {
    pub account_id: Option<AccountId>,
    pub api_key_id: Option<ApiKeyId>,
    pub remaining_requests: Option<u64>,
    pub remaining_credits: Option<CreditAmount>,
    pub reset_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageEvent {
    pub request_id: RequestId,
    pub account_id: Option<AccountId>,
    pub api_key_id: Option<ApiKeyId>,
    pub model_id: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cost: Option<CreditAmount>,
    pub recorded_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageCheckRequest {
    pub request_id: RequestId,
    pub account_id: Option<AccountId>,
    pub api_key_id: Option<ApiKeyId>,
}

pub trait UsageMeter {
    fn record_usage(&self, event: UsageEvent) -> impl Future<Output = Result<()>> + Send;
}

pub trait RateLimiter {
    fn check_rate_limit(
        &self,
        request: UsageCheckRequest,
    ) -> impl Future<Output = Result<QuotaSnapshot>> + Send;
}

pub trait SpendLimitChecker {
    fn check_spend_limit(
        &self,
        request: UsageCheckRequest,
    ) -> impl Future<Output = Result<QuotaSnapshot>> + Send;
}
