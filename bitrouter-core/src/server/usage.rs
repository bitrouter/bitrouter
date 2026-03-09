use std::future::Future;

use super::{
    errors::Result,
    ids::{AccountId, RequestId},
};

pub type CreditAmount = i64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaSnapshot {
    pub account_id: AccountId,
    pub remaining_requests: Option<u64>,
    pub remaining_credits: Option<CreditAmount>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageEvent {
    pub request_id: RequestId,
    pub account_id: AccountId,
    pub model: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cost: Option<CreditAmount>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitRequest {
    pub account_id: AccountId,
    pub request_id: RequestId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateLimitDecision {
    Allow,
    Deny { retry_after_seconds: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpendLimitRequest {
    pub account_id: AccountId,
    pub projected_cost: CreditAmount,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpendLimitDecision {
    Allow,
    Deny { remaining_credits: CreditAmount },
}

pub trait UsageMeter {
    fn record(&self, event: UsageEvent) -> impl Future<Output = Result<()>> + Send;
}

pub trait RateLimiter {
    fn check(
        &self,
        request: RateLimitRequest,
    ) -> impl Future<Output = Result<RateLimitDecision>> + Send;
}

pub trait SpendLimitChecker {
    fn check(
        &self,
        request: SpendLimitRequest,
    ) -> impl Future<Output = Result<SpendLimitDecision>> + Send;
}
