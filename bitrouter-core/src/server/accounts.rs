use std::future::Future;

use super::{
    errors::Result,
    ids::{AccountId, ApiKeyId},
    pagination::{Page, PaginationRequest},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountStatus {
    Active,
    Suspended,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSummary {
    pub account_id: AccountId,
    pub status: AccountStatus,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeySummary {
    pub api_key_id: ApiKeyId,
    pub account_id: AccountId,
    pub label: Option<String>,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPolicy {
    pub scopes: Vec<String>,
    pub max_requests_per_minute: Option<u64>,
    pub monthly_credit_limit: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubKeySpec {
    pub account_id: AccountId,
    pub label: Option<String>,
    pub policy: KeyPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminBootstrapRequest {
    pub label: Option<String>,
    pub policy: KeyPolicy,
}

pub trait AccountService {
    fn get_account(
        &self,
        account_id: AccountId,
    ) -> impl Future<Output = Result<AccountSummary>> + Send;
    fn list_accounts(
        &self,
        pagination: PaginationRequest,
    ) -> impl Future<Output = Result<Page<AccountSummary>>> + Send;
    fn update_status(
        &self,
        account_id: AccountId,
        status: AccountStatus,
    ) -> impl Future<Output = Result<AccountSummary>> + Send;
}

pub trait ApiKeyService {
    fn create_sub_key(
        &self,
        spec: SubKeySpec,
    ) -> impl Future<Output = Result<ApiKeySummary>> + Send;
    fn revoke_key(&self, api_key_id: ApiKeyId) -> impl Future<Output = Result<()>> + Send;
    fn list_account_keys(
        &self,
        account_id: AccountId,
        pagination: PaginationRequest,
    ) -> impl Future<Output = Result<Page<ApiKeySummary>>> + Send;
}

pub trait AdminBootstrapService {
    fn bootstrap_admin(
        &self,
        request: AdminBootstrapRequest,
    ) -> impl Future<Output = Result<ApiKeySummary>> + Send;
}
