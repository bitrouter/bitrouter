use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bitrouter_core::server::accounts::{
    Account, AccountService, AccountStatus, AdminBootstrapService, ApiKeyRecord, ApiKeyService,
    CreateAccountRequest, CreateApiKeyResponse, KeyPolicy, SubKeySpec,
};
use bitrouter_core::server::auth::{
    AuthContext, AuthDecision, AuthScope, AuthSubject, Authenticator,
};
use bitrouter_core::server::errors::{ServerError, ServerResult};
#[cfg(test)]
use bitrouter_core::server::ids::RequestId;
use bitrouter_core::server::ids::{AccountId, ApiKeyId};
use bitrouter_core::server::pagination::{CursorPage, PageRequest};
use bitrouter_core::server::time::Timestamp;
use tokio::sync::RwLock;

static ID_COUNTER: AtomicU64 = AtomicU64::new(1);

fn next_id(prefix: &str) -> String {
    let n = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{n:016x}")
}

fn generate_key_plaintext(prefix: &str) -> String {
    let n = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is before UNIX epoch")
        .as_nanos() as u64;
    format!("{prefix}-{ts:016x}{n:016x}")
}

// ---------------------------------------------------------------------------
// InMemoryAuthenticator
// ---------------------------------------------------------------------------

pub struct InMemoryAuthenticator {
    keys: RwLock<HashMap<String, AuthContext>>,
}

impl InMemoryAuthenticator {
    pub fn new() -> Self {
        Self {
            keys: RwLock::new(HashMap::new()),
        }
    }

    pub async fn add_key(&self, key_string: String, context: AuthContext) {
        self.keys.write().await.insert(key_string, context);
    }
}

impl Default for InMemoryAuthenticator {
    fn default() -> Self {
        Self::new()
    }
}

impl Authenticator for InMemoryAuthenticator {
    async fn authenticate(&self, subject: &AuthSubject) -> ServerResult<AuthContext> {
        let raw = match subject {
            AuthSubject::ApiKey(k) => k,
            AuthSubject::Bearer(t) => t,
        };
        let guard = self.keys.read().await;
        guard
            .get(raw)
            .cloned()
            .ok_or_else(|| ServerError::Unauthorized {
                message: "invalid credentials".into(),
            })
    }

    async fn check_scope(
        &self,
        context: &AuthContext,
        required: &AuthScope,
    ) -> ServerResult<AuthDecision> {
        if context.scopes.contains(required) {
            Ok(AuthDecision::Allow)
        } else {
            Ok(AuthDecision::Deny {
                reason: format!("missing required scope: {required:?}"),
            })
        }
    }
}

// ---------------------------------------------------------------------------
// InMemoryAccountService
// ---------------------------------------------------------------------------

pub struct InMemoryAccountService {
    accounts: RwLock<HashMap<String, Account>>,
}

impl InMemoryAccountService {
    pub fn new() -> Self {
        Self {
            accounts: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryAccountService {
    fn default() -> Self {
        Self::new()
    }
}

impl AccountService for InMemoryAccountService {
    async fn create_account(&self, request: CreateAccountRequest) -> ServerResult<Account> {
        let id = AccountId::new(next_id("acct"));
        let now = Timestamp::now();
        let account = Account {
            id: id.clone(),
            name: request.name,
            status: AccountStatus::Active,
            created_at: now,
            updated_at: now,
        };
        self.accounts
            .write()
            .await
            .insert(id.as_str().to_owned(), account.clone());
        Ok(account)
    }

    async fn get_account(&self, id: &AccountId) -> ServerResult<Account> {
        let guard = self.accounts.read().await;
        guard
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| ServerError::NotFound {
                entity: "account".into(),
                id: id.to_string(),
            })
    }

    async fn list_accounts(&self, _page: PageRequest) -> ServerResult<CursorPage<Account>> {
        let guard = self.accounts.read().await;
        let items: Vec<Account> = guard.values().cloned().collect();
        Ok(CursorPage {
            items,
            next_cursor: None,
            has_more: false,
        })
    }

    async fn suspend_account(&self, id: &AccountId) -> ServerResult<Account> {
        let mut guard = self.accounts.write().await;
        let account = guard
            .get_mut(id.as_str())
            .ok_or_else(|| ServerError::NotFound {
                entity: "account".into(),
                id: id.to_string(),
            })?;
        account.status = AccountStatus::Suspended;
        account.updated_at = Timestamp::now();
        Ok(account.clone())
    }
}

// ---------------------------------------------------------------------------
// InMemoryApiKeyService
// ---------------------------------------------------------------------------

pub struct InMemoryApiKeyService {
    keys: RwLock<HashMap<String, ApiKeyRecord>>,
}

impl InMemoryApiKeyService {
    pub fn new() -> Self {
        Self {
            keys: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryApiKeyService {
    fn default() -> Self {
        Self::new()
    }
}

impl ApiKeyService for InMemoryApiKeyService {
    async fn create_key(
        &self,
        account_id: &AccountId,
        spec: SubKeySpec,
    ) -> ServerResult<CreateApiKeyResponse> {
        let key_id = ApiKeyId::new(next_id("key"));
        let prefix = format!("br_{:08x}", ID_COUNTER.load(Ordering::Relaxed));
        let plaintext = generate_key_plaintext(&prefix);
        let now = Timestamp::now();

        let record = ApiKeyRecord {
            id: key_id.clone(),
            account_id: account_id.clone(),
            name: spec.name,
            prefix: prefix.clone(),
            scopes: spec.scopes,
            policy: spec.policy,
            created_at: now,
            revoked_at: None,
        };

        self.keys
            .write()
            .await
            .insert(key_id.as_str().to_owned(), record.clone());

        Ok(CreateApiKeyResponse {
            record,
            plaintext_key: plaintext,
        })
    }

    async fn list_keys(
        &self,
        account_id: &AccountId,
        _page: PageRequest,
    ) -> ServerResult<CursorPage<ApiKeyRecord>> {
        let guard = self.keys.read().await;
        let items: Vec<ApiKeyRecord> = guard
            .values()
            .filter(|r| r.account_id == *account_id)
            .cloned()
            .collect();
        Ok(CursorPage {
            items,
            next_cursor: None,
            has_more: false,
        })
    }

    async fn revoke_key(&self, key_id: &ApiKeyId) -> ServerResult<()> {
        let mut guard = self.keys.write().await;
        let record = guard
            .get_mut(key_id.as_str())
            .ok_or_else(|| ServerError::NotFound {
                entity: "api_key".into(),
                id: key_id.to_string(),
            })?;
        record.revoked_at = Some(Timestamp::now());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// InMemoryAdminBootstrapService
// ---------------------------------------------------------------------------

pub struct InMemoryAdminBootstrapService {
    bootstrapped: AtomicBool,
    account_service: InMemoryAccountService,
    key_service: InMemoryApiKeyService,
}

impl InMemoryAdminBootstrapService {
    pub fn new(
        account_service: InMemoryAccountService,
        key_service: InMemoryApiKeyService,
    ) -> Self {
        Self {
            bootstrapped: AtomicBool::new(false),
            account_service,
            key_service,
        }
    }
}

impl AdminBootstrapService for InMemoryAdminBootstrapService {
    async fn is_bootstrapped(&self) -> ServerResult<bool> {
        Ok(self.bootstrapped.load(Ordering::SeqCst))
    }

    async fn bootstrap(&self, request: CreateAccountRequest) -> ServerResult<CreateApiKeyResponse> {
        if self.bootstrapped.load(Ordering::SeqCst) {
            return Err(ServerError::AlreadyExists {
                entity: "admin".into(),
                id: "bootstrap".into(),
            });
        }

        let account = self.account_service.create_account(request).await?;

        let spec = SubKeySpec {
            name: "admin-bootstrap-key".into(),
            scopes: vec![AuthScope::Admin, AuthScope::Inference],
            policy: KeyPolicy {
                rate_limit_per_minute: None,
                spend_limit_cents: None,
            },
        };

        let response = self.key_service.create_key(&account.id, spec).await?;
        self.bootstrapped.store(true, Ordering::SeqCst);
        Ok(response)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_auth_context(account_id: &str, scopes: Vec<AuthScope>) -> AuthContext {
        AuthContext {
            account_id: AccountId::new(account_id),
            key_id: ApiKeyId::new("test-key"),
            request_id: RequestId::new("test-request"),
            scopes,
        }
    }

    // -- Authenticator tests --

    #[tokio::test]
    async fn authenticator_valid_api_key() {
        let auth = InMemoryAuthenticator::new();
        let ctx = test_auth_context("acct-1", vec![AuthScope::Inference]);
        auth.add_key("sk-valid".into(), ctx).await;

        let result = auth
            .authenticate(&AuthSubject::ApiKey("sk-valid".into()))
            .await;
        assert!(result.is_ok());
        let ctx = result.unwrap();
        assert_eq!(ctx.account_id, AccountId::new("acct-1"));
    }

    #[tokio::test]
    async fn authenticator_invalid_api_key() {
        let auth = InMemoryAuthenticator::new();

        let result = auth
            .authenticate(&AuthSubject::ApiKey("sk-bogus".into()))
            .await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ServerError::Unauthorized { .. }
        ));
    }

    #[tokio::test]
    async fn authenticator_valid_bearer() {
        let auth = InMemoryAuthenticator::new();
        let ctx = test_auth_context("acct-2", vec![AuthScope::Admin]);
        auth.add_key("bearer-token".into(), ctx).await;

        let result = auth
            .authenticate(&AuthSubject::Bearer("bearer-token".into()))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn check_scope_allow() {
        let auth = InMemoryAuthenticator::new();
        let ctx = test_auth_context("acct-1", vec![AuthScope::Inference, AuthScope::Admin]);

        let decision = auth.check_scope(&ctx, &AuthScope::Admin).await.unwrap();
        assert!(matches!(decision, AuthDecision::Allow));
    }

    #[tokio::test]
    async fn check_scope_deny() {
        let auth = InMemoryAuthenticator::new();
        let ctx = test_auth_context("acct-1", vec![AuthScope::Inference]);

        let decision = auth.check_scope(&ctx, &AuthScope::Admin).await.unwrap();
        assert!(matches!(decision, AuthDecision::Deny { .. }));
    }

    // -- AccountService tests --

    #[tokio::test]
    async fn account_create_and_get() {
        let svc = InMemoryAccountService::new();
        let account = svc
            .create_account(CreateAccountRequest {
                name: "Test Org".into(),
            })
            .await
            .unwrap();

        assert_eq!(account.name, "Test Org");
        assert_eq!(account.status, AccountStatus::Active);

        let fetched = svc.get_account(&account.id).await.unwrap();
        assert_eq!(fetched.id, account.id);
    }

    #[tokio::test]
    async fn account_get_not_found() {
        let svc = InMemoryAccountService::new();
        let result = svc.get_account(&AccountId::new("nonexistent")).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), ServerError::NotFound { .. }));
    }

    #[tokio::test]
    async fn account_list() {
        let svc = InMemoryAccountService::new();
        svc.create_account(CreateAccountRequest {
            name: "Org A".into(),
        })
        .await
        .unwrap();
        svc.create_account(CreateAccountRequest {
            name: "Org B".into(),
        })
        .await
        .unwrap();

        let page = svc.list_accounts(PageRequest::default()).await.unwrap();
        assert_eq!(page.items.len(), 2);
        assert!(!page.has_more);
    }

    #[tokio::test]
    async fn account_suspend() {
        let svc = InMemoryAccountService::new();
        let account = svc
            .create_account(CreateAccountRequest {
                name: "Suspend Me".into(),
            })
            .await
            .unwrap();

        let suspended = svc.suspend_account(&account.id).await.unwrap();
        assert_eq!(suspended.status, AccountStatus::Suspended);

        let fetched = svc.get_account(&account.id).await.unwrap();
        assert_eq!(fetched.status, AccountStatus::Suspended);
    }

    #[tokio::test]
    async fn account_suspend_not_found() {
        let svc = InMemoryAccountService::new();
        let result = svc.suspend_account(&AccountId::new("ghost")).await;
        assert!(matches!(result.unwrap_err(), ServerError::NotFound { .. }));
    }

    // -- ApiKeyService tests --

    #[tokio::test]
    async fn api_key_create_and_list() {
        let svc = InMemoryApiKeyService::new();
        let acct = AccountId::new("acct-1");
        let spec = SubKeySpec {
            name: "my-key".into(),
            scopes: vec![AuthScope::Inference],
            policy: KeyPolicy {
                rate_limit_per_minute: Some(100),
                spend_limit_cents: None,
            },
        };

        let resp = svc.create_key(&acct, spec).await.unwrap();
        assert!(!resp.plaintext_key.is_empty());
        assert_eq!(resp.record.name, "my-key");

        let page = svc.list_keys(&acct, PageRequest::default()).await.unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, resp.record.id);
    }

    #[tokio::test]
    async fn api_key_list_filters_by_account() {
        let svc = InMemoryApiKeyService::new();
        let acct_a = AccountId::new("acct-a");
        let acct_b = AccountId::new("acct-b");

        let spec_a = SubKeySpec {
            name: "key-a".into(),
            scopes: vec![AuthScope::Inference],
            policy: KeyPolicy {
                rate_limit_per_minute: None,
                spend_limit_cents: None,
            },
        };
        let spec_b = SubKeySpec {
            name: "key-b".into(),
            scopes: vec![AuthScope::Admin],
            policy: KeyPolicy {
                rate_limit_per_minute: None,
                spend_limit_cents: None,
            },
        };

        svc.create_key(&acct_a, spec_a).await.unwrap();
        svc.create_key(&acct_b, spec_b).await.unwrap();

        let page_a = svc
            .list_keys(&acct_a, PageRequest::default())
            .await
            .unwrap();
        assert_eq!(page_a.items.len(), 1);
        assert_eq!(page_a.items[0].name, "key-a");
    }

    #[tokio::test]
    async fn api_key_revoke() {
        let svc = InMemoryApiKeyService::new();
        let acct = AccountId::new("acct-1");
        let spec = SubKeySpec {
            name: "revocable".into(),
            scopes: vec![AuthScope::Inference],
            policy: KeyPolicy {
                rate_limit_per_minute: None,
                spend_limit_cents: None,
            },
        };

        let resp = svc.create_key(&acct, spec).await.unwrap();
        assert!(resp.record.revoked_at.is_none());

        svc.revoke_key(&resp.record.id).await.unwrap();

        let page = svc.list_keys(&acct, PageRequest::default()).await.unwrap();
        assert!(page.items[0].revoked_at.is_some());
    }

    #[tokio::test]
    async fn api_key_revoke_not_found() {
        let svc = InMemoryApiKeyService::new();
        let result = svc.revoke_key(&ApiKeyId::new("ghost")).await;
        assert!(matches!(result.unwrap_err(), ServerError::NotFound { .. }));
    }

    // -- AdminBootstrapService tests --

    #[tokio::test]
    async fn bootstrap_first_call_succeeds() {
        let bootstrap = InMemoryAdminBootstrapService::new(
            InMemoryAccountService::new(),
            InMemoryApiKeyService::new(),
        );

        assert!(!bootstrap.is_bootstrapped().await.unwrap());

        let resp = bootstrap
            .bootstrap(CreateAccountRequest {
                name: "Admin Org".into(),
            })
            .await;
        assert!(resp.is_ok());
        assert!(bootstrap.is_bootstrapped().await.unwrap());
    }

    #[tokio::test]
    async fn bootstrap_second_call_fails() {
        let bootstrap = InMemoryAdminBootstrapService::new(
            InMemoryAccountService::new(),
            InMemoryApiKeyService::new(),
        );

        bootstrap
            .bootstrap(CreateAccountRequest {
                name: "Admin Org".into(),
            })
            .await
            .unwrap();

        let result = bootstrap
            .bootstrap(CreateAccountRequest {
                name: "Again".into(),
            })
            .await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ServerError::AlreadyExists { .. }
        ));
    }
}
