use std::future::Future;

use super::{
    auth::AuthScope,
    errors::Result,
    ids::{AccountId, ApiKeyId},
    pagination::{Page, PaginationRequest},
    time::{LifecycleState, Timestamp},
    usage::CreditAmount,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountStatus {
    Active,
    Suspended,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPolicy {
    pub scopes: Vec<AuthScope>,
    pub expires_at: Option<Timestamp>,
    pub max_requests_per_minute: Option<u32>,
    pub max_spend: Option<CreditAmount>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubKeySpec {
    pub label: String,
    pub policy: KeyPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSummary {
    pub account_id: AccountId,
    pub display_name: String,
    pub status: AccountStatus,
    pub lifecycle: LifecycleState,
    pub created_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeySummary {
    pub api_key_id: ApiKeyId,
    pub account_id: AccountId,
    pub label: String,
    pub policy: KeyPolicy,
    pub created_at: Timestamp,
    pub revoked_at: Option<Timestamp>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetAccountRequest {
    pub account_id: AccountId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateAccountRequest {
    pub display_name: String,
    pub status: AccountStatus,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListAccountsRequest {
    pub pagination: PaginationRequest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateApiKeyRequest {
    pub account_id: AccountId,
    pub spec: SubKeySpec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokeApiKeyRequest {
    pub api_key_id: ApiKeyId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListApiKeysRequest {
    pub account_id: AccountId,
    pub pagination: PaginationRequest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapAdminRequest {
    pub display_name: String,
    pub key_spec: SubKeySpec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootstrapAdminResult {
    pub account: AccountSummary,
    pub api_key: ApiKeySummary,
    pub secret: String,
}

pub trait AccountService {
    fn get_account(
        &self,
        request: GetAccountRequest,
    ) -> impl Future<Output = Result<AccountSummary>> + Send;

    fn create_account(
        &self,
        request: CreateAccountRequest,
    ) -> impl Future<Output = Result<AccountSummary>> + Send;

    fn list_accounts(
        &self,
        request: ListAccountsRequest,
    ) -> impl Future<Output = Result<Page<AccountSummary>>> + Send;
}

pub trait ApiKeyService {
    fn create_api_key(
        &self,
        request: CreateApiKeyRequest,
    ) -> impl Future<Output = Result<ApiKeySummary>> + Send;

    fn list_api_keys(
        &self,
        request: ListApiKeysRequest,
    ) -> impl Future<Output = Result<Page<ApiKeySummary>>> + Send;

    fn revoke_api_key(
        &self,
        request: RevokeApiKeyRequest,
    ) -> impl Future<Output = Result<()>> + Send;
}

pub trait AdminBootstrapService {
    fn bootstrap_admin(
        &self,
        request: BootstrapAdminRequest,
    ) -> impl Future<Output = Result<BootstrapAdminResult>> + Send;
}
