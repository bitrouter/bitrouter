use super::{
    errors::ServerResult,
    ids::{AccountId, ApiKeyId},
    pagination::{CursorPage, PageRequest},
    time::Timestamp,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountStatus {
    Active,
    Suspended,
    Closed,
}

#[derive(Debug, Clone)]
pub struct Account {
    pub id: AccountId,
    pub name: String,
    pub status: AccountStatus,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone)]
pub struct CreateAccountRequest {
    pub name: String,
}

/// Sub-key creation specification.
#[derive(Debug, Clone)]
pub struct SubKeySpec {
    pub name: String,
    pub scopes: Vec<super::auth::AuthScope>,
    pub policy: KeyPolicy,
}

#[derive(Debug, Clone)]
pub struct KeyPolicy {
    pub rate_limit_per_minute: Option<u32>,
    pub spend_limit_cents: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ApiKeyRecord {
    pub id: ApiKeyId,
    pub account_id: AccountId,
    pub name: String,
    pub prefix: String,
    pub scopes: Vec<super::auth::AuthScope>,
    pub policy: KeyPolicy,
    pub created_at: Timestamp,
    pub revoked_at: Option<Timestamp>,
}

#[derive(Debug, Clone)]
pub struct CreateApiKeyResponse {
    pub record: ApiKeyRecord,
    /// The full plaintext key. Only available at creation time.
    pub plaintext_key: String,
}

/// Service for managing accounts.
pub trait AccountService {
    fn create_account(
        &self,
        request: CreateAccountRequest,
    ) -> impl Future<Output = ServerResult<Account>> + Send;

    fn get_account(
        &self,
        id: &AccountId,
    ) -> impl Future<Output = ServerResult<Account>> + Send;

    fn list_accounts(
        &self,
        page: PageRequest,
    ) -> impl Future<Output = ServerResult<CursorPage<Account>>> + Send;

    fn suspend_account(
        &self,
        id: &AccountId,
    ) -> impl Future<Output = ServerResult<Account>> + Send;
}

/// Service for managing API keys.
pub trait ApiKeyService {
    fn create_key(
        &self,
        account_id: &AccountId,
        spec: SubKeySpec,
    ) -> impl Future<Output = ServerResult<CreateApiKeyResponse>> + Send;

    fn list_keys(
        &self,
        account_id: &AccountId,
        page: PageRequest,
    ) -> impl Future<Output = ServerResult<CursorPage<ApiKeyRecord>>> + Send;

    fn revoke_key(
        &self,
        key_id: &ApiKeyId,
    ) -> impl Future<Output = ServerResult<()>> + Send;
}

/// Service for initial admin bootstrap (first-run setup).
pub trait AdminBootstrapService {
    fn is_bootstrapped(&self) -> impl Future<Output = ServerResult<bool>> + Send;

    fn bootstrap(
        &self,
        request: CreateAccountRequest,
    ) -> impl Future<Output = ServerResult<CreateApiKeyResponse>> + Send;
}
