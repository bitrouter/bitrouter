use std::collections::HashMap;
use std::sync::Arc;

use bitrouter_core::server::{
    accounts::{
        Account, AccountService, AccountStatus, AdminBootstrapService, ApiKeyRecord, ApiKeyService,
        CreateAccountRequest, CreateApiKeyResponse, KeyPolicy, SubKeySpec,
    },
    auth::{AuthContext, AuthDecision, AuthScope, AuthSubject, Authenticator},
    blobs::{
        BlobContent, BlobMetadata, BlobStore, ObjectBinding, ObjectCatalog, ObjectEntry,
        PutBlobRequest,
    },
    errors::{ServerError, ServerResult},
    ids::{AccountId, ApiKeyId, BlobId, RequestId, SessionId},
    pagination::{CursorPage, PageRequest},
    sessions::{
        CreateSessionRequest, SessionDetail, SessionMutation, SessionQueryService, SessionSummary,
        SessionWriteService,
    },
    time::Timestamp,
};

use super::accounts;
use super::blobs;
use super::filters::rejection_handler;
use super::sessions;

// ── Mock implementations ────────────────────────────────────────────────────

struct MockAuthenticator;
impl Authenticator for MockAuthenticator {
    async fn authenticate(&self, subject: &AuthSubject) -> ServerResult<AuthContext> {
        match subject {
            AuthSubject::ApiKey(k) if k == "test-key" => Ok(AuthContext {
                account_id: AccountId::new("acc_1"),
                key_id: ApiKeyId::new("key_1"),
                request_id: RequestId::new("req_1"),
                scopes: vec![AuthScope::Admin],
            }),
            AuthSubject::Bearer(t) if t == "good-token" => Ok(AuthContext {
                account_id: AccountId::new("acc_1"),
                key_id: ApiKeyId::new("key_1"),
                request_id: RequestId::new("req_1"),
                scopes: vec![AuthScope::Inference],
            }),
            _ => Err(ServerError::unauthorized("invalid credentials")),
        }
    }
    async fn check_scope(
        &self,
        ctx: &AuthContext,
        required: &AuthScope,
    ) -> ServerResult<AuthDecision> {
        if ctx.scopes.contains(required) {
            Ok(AuthDecision::Allow)
        } else {
            Ok(AuthDecision::Deny {
                reason: "missing scope".to_owned(),
            })
        }
    }
}

struct MockAccountService;
impl AccountService for MockAccountService {
    async fn create_account(&self, req: CreateAccountRequest) -> ServerResult<Account> {
        Ok(Account {
            id: AccountId::new("acc_new"),
            name: req.name,
            status: AccountStatus::Active,
            created_at: Timestamp::from(1000),
            updated_at: Timestamp::from(1000),
        })
    }
    async fn get_account(&self, id: &AccountId) -> ServerResult<Account> {
        if id.as_str() == "acc_1" {
            Ok(Account {
                id: id.clone(),
                name: "Test Account".to_owned(),
                status: AccountStatus::Active,
                created_at: Timestamp::from(1000),
                updated_at: Timestamp::from(1000),
            })
        } else {
            Err(ServerError::not_found("account", id.as_str()))
        }
    }
    async fn list_accounts(&self, _page: PageRequest) -> ServerResult<CursorPage<Account>> {
        Ok(CursorPage {
            items: vec![Account {
                id: AccountId::new("acc_1"),
                name: "Test Account".to_owned(),
                status: AccountStatus::Active,
                created_at: Timestamp::from(1000),
                updated_at: Timestamp::from(1000),
            }],
            next_cursor: None,
            has_more: false,
        })
    }
    async fn suspend_account(&self, id: &AccountId) -> ServerResult<Account> {
        Ok(Account {
            id: id.clone(),
            name: "Test Account".to_owned(),
            status: AccountStatus::Suspended,
            created_at: Timestamp::from(1000),
            updated_at: Timestamp::from(2000),
        })
    }
}

struct MockApiKeyService;
impl ApiKeyService for MockApiKeyService {
    async fn create_key(
        &self,
        account_id: &AccountId,
        spec: SubKeySpec,
    ) -> ServerResult<CreateApiKeyResponse> {
        Ok(CreateApiKeyResponse {
            record: ApiKeyRecord {
                id: ApiKeyId::new("key_new"),
                account_id: account_id.clone(),
                name: spec.name,
                prefix: "br_test".to_owned(),
                scopes: spec.scopes,
                policy: spec.policy,
                created_at: Timestamp::from(1000),
                revoked_at: None,
            },
            plaintext_key: "br_test_secret123".to_owned(),
        })
    }
    async fn list_keys(
        &self,
        _account_id: &AccountId,
        _page: PageRequest,
    ) -> ServerResult<CursorPage<ApiKeyRecord>> {
        Ok(CursorPage {
            items: vec![],
            next_cursor: None,
            has_more: false,
        })
    }
    async fn revoke_key(&self, _key_id: &ApiKeyId) -> ServerResult<()> {
        Ok(())
    }
}

struct MockBootstrapService {
    done: std::sync::atomic::AtomicBool,
}
impl AdminBootstrapService for MockBootstrapService {
    async fn is_bootstrapped(&self) -> ServerResult<bool> {
        Ok(self.done.load(std::sync::atomic::Ordering::Relaxed))
    }
    async fn bootstrap(&self, req: CreateAccountRequest) -> ServerResult<CreateApiKeyResponse> {
        if self.done.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return Err(ServerError::already_exists("bootstrap", "admin"));
        }
        Ok(CreateApiKeyResponse {
            record: ApiKeyRecord {
                id: ApiKeyId::new("key_admin"),
                account_id: AccountId::new("acc_admin"),
                name: req.name,
                prefix: "br_adm".to_owned(),
                scopes: vec![AuthScope::Admin],
                policy: KeyPolicy {
                    rate_limit_per_minute: None,
                    spend_limit_cents: None,
                },
                created_at: Timestamp::from(1000),
                revoked_at: None,
            },
            plaintext_key: "br_adm_secret".to_owned(),
        })
    }
}

struct MockSessionQuery {
    data: tokio::sync::RwLock<HashMap<String, SessionDetail>>,
}
impl MockSessionQuery {
    fn new() -> Self {
        let mut map = HashMap::new();
        map.insert(
            "sess_1".to_owned(),
            SessionDetail {
                summary: SessionSummary {
                    id: SessionId::new("sess_1"),
                    account_id: AccountId::new("acc_1"),
                    title: Some("Test Session".to_owned()),
                    created_at: Timestamp::from(1000),
                    updated_at: Timestamp::from(1000),
                },
                content: serde_json::json!({"messages": []}),
            },
        );
        Self {
            data: tokio::sync::RwLock::new(map),
        }
    }
}
impl SessionQueryService for MockSessionQuery {
    async fn get_session(&self, id: &SessionId) -> ServerResult<SessionDetail> {
        self.data
            .read()
            .await
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| ServerError::not_found("session", id.as_str()))
    }
    async fn list_sessions(
        &self,
        _account_id: &AccountId,
        _page: PageRequest,
    ) -> ServerResult<CursorPage<SessionSummary>> {
        let items: Vec<SessionSummary> = self
            .data
            .read()
            .await
            .values()
            .map(|d| d.summary.clone())
            .collect();
        Ok(CursorPage {
            items,
            next_cursor: None,
            has_more: false,
        })
    }
}

struct MockSessionWriter;
impl SessionWriteService for MockSessionWriter {
    async fn create_session(&self, req: CreateSessionRequest) -> ServerResult<SessionDetail> {
        Ok(SessionDetail {
            summary: SessionSummary {
                id: SessionId::new("sess_new"),
                account_id: req.account_id,
                title: req.title,
                created_at: Timestamp::from(1000),
                updated_at: Timestamp::from(1000),
            },
            content: req.content,
        })
    }
    async fn update_session(
        &self,
        id: &SessionId,
        mutation: SessionMutation,
    ) -> ServerResult<SessionDetail> {
        Ok(SessionDetail {
            summary: SessionSummary {
                id: id.clone(),
                account_id: AccountId::new("acc_1"),
                title: mutation.title,
                created_at: Timestamp::from(1000),
                updated_at: Timestamp::from(2000),
            },
            content: mutation.content.unwrap_or(serde_json::json!(null)),
        })
    }
    async fn delete_session(&self, _id: &SessionId) -> ServerResult<()> {
        Ok(())
    }
}

struct MockBlobStore;
impl BlobStore for MockBlobStore {
    async fn put_blob(&self, req: PutBlobRequest, data: Vec<u8>) -> ServerResult<BlobMetadata> {
        Ok(BlobMetadata {
            id: BlobId::new("blob_new"),
            account_id: req.account_id,
            content_type: req.content_type,
            size_bytes: data.len() as u64,
            created_at: Timestamp::from(1000),
        })
    }
    async fn get_blob(&self, id: &BlobId) -> ServerResult<BlobContent> {
        if id.as_str() == "blob_1" {
            Ok(BlobContent {
                metadata: BlobMetadata {
                    id: id.clone(),
                    account_id: AccountId::new("acc_1"),
                    content_type: "text/plain".to_owned(),
                    size_bytes: 5,
                    created_at: Timestamp::from(1000),
                },
                data: b"hello".to_vec(),
            })
        } else {
            Err(ServerError::not_found("blob", id.as_str()))
        }
    }
    async fn delete_blob(&self, _id: &BlobId) -> ServerResult<()> {
        Ok(())
    }
}

struct MockObjectCatalog;
impl ObjectCatalog for MockObjectCatalog {
    async fn bind_object(
        &self,
        _account_id: &AccountId,
        _binding: ObjectBinding,
    ) -> ServerResult<()> {
        Ok(())
    }
    async fn get_object(&self, _account_id: &AccountId, name: &str) -> ServerResult<ObjectEntry> {
        Ok(ObjectEntry {
            name: name.to_owned(),
            blob_id: BlobId::new("blob_1"),
            content_type: "text/plain".to_owned(),
            size_bytes: 5,
            created_at: Timestamp::from(1000),
        })
    }
    async fn list_objects(
        &self,
        _account_id: &AccountId,
        _page: PageRequest,
    ) -> ServerResult<CursorPage<ObjectEntry>> {
        Ok(CursorPage {
            items: vec![],
            next_cursor: None,
            has_more: false,
        })
    }
    async fn unbind_object(&self, _account_id: &AccountId, _name: &str) -> ServerResult<()> {
        Ok(())
    }
}

// ── Account tests ───────────────────────────────────────────────────────────

#[tokio::test]
async fn test_create_account() {
    let svc = Arc::new(MockAccountService);
    let filter = accounts::create_account_filter(svc);

    let res = warp::test::request()
        .method("POST")
        .path("/v1/accounts")
        .json(&serde_json::json!({"name": "Acme Corp"}))
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 201);
    let body: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(body["name"], "Acme Corp");
    assert_eq!(body["status"], "active");
    assert!(body["id"].is_string());
}

#[tokio::test]
async fn test_get_account() {
    let svc = Arc::new(MockAccountService);
    let filter = accounts::get_account_filter(svc);

    let res = warp::test::request()
        .method("GET")
        .path("/v1/accounts/acc_1")
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    let body: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(body["id"], "acc_1");
    assert_eq!(body["name"], "Test Account");
}

#[tokio::test]
async fn test_list_accounts() {
    let svc = Arc::new(MockAccountService);
    let filter = accounts::list_accounts_filter(svc);

    let res = warp::test::request()
        .method("GET")
        .path("/v1/accounts?limit=10")
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    let body: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert!(body["items"].is_array());
    assert_eq!(body["has_more"], false);
}

#[tokio::test]
async fn test_bootstrap() {
    let svc = Arc::new(MockBootstrapService {
        done: std::sync::atomic::AtomicBool::new(false),
    });
    let filter = accounts::bootstrap_filter(svc);

    let res = warp::test::request()
        .method("POST")
        .path("/v1/admin/bootstrap")
        .json(&serde_json::json!({"name": "Admin"}))
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 201);
    let body: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert!(body["plaintext_key"].is_string());
}

// ── Session tests ───────────────────────────────────────────────────────────

#[tokio::test]
async fn test_create_session() {
    let svc = Arc::new(MockSessionWriter);
    let filter = sessions::create_session_filter(svc);

    let res = warp::test::request()
        .method("POST")
        .path("/v1/sessions")
        .json(&serde_json::json!({
            "account_id": "acc_1",
            "title": "My Chat",
            "content": {"messages": []}
        }))
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 201);
    let body: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(body["title"], "My Chat");
    assert!(body["id"].is_string());
}

#[tokio::test]
async fn test_get_session() {
    let svc = Arc::new(MockSessionQuery::new());
    let filter = sessions::get_session_filter(svc);

    let res = warp::test::request()
        .method("GET")
        .path("/v1/sessions/sess_1")
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    let body: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(body["id"], "sess_1");
    assert_eq!(body["title"], "Test Session");
}

#[tokio::test]
async fn test_delete_session() {
    let svc = Arc::new(MockSessionWriter);
    let filter = sessions::delete_session_filter(svc);

    let res = warp::test::request()
        .method("DELETE")
        .path("/v1/sessions/sess_1")
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 204);
}

// ── Blob tests ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_upload_blob() {
    let store = Arc::new(MockBlobStore);
    let filter = blobs::upload_blob_filter(store);

    let res = warp::test::request()
        .method("POST")
        .path("/v1/blobs?account_id=acc_1&content_type=text/plain")
        .body("hello world")
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 201);
    let body: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(body["content_type"], "text/plain");
    assert_eq!(body["size_bytes"], 11);
}

#[tokio::test]
async fn test_get_blob() {
    let store = Arc::new(MockBlobStore);
    let filter = blobs::get_blob_filter(store);

    let res = warp::test::request()
        .method("GET")
        .path("/v1/blobs/blob_1")
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    assert_eq!(res.body(), b"hello");
}

// ── Rejection handler tests ─────────────────────────────────────────────────

#[tokio::test]
async fn test_rejection_not_found() {
    use warp::Filter;
    let filter = warp::any()
        .and_then(|| async {
            Err::<String, _>(warp::reject::custom(crate::error::ServerRejection(
                ServerError::not_found("account", "acc_999"),
            )))
        })
        .recover(rejection_handler);

    let res = warp::test::request().reply(&filter).await;
    assert_eq!(res.status(), 404);
    let body: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(body["error"]["type"], "not_found_error");
}

#[tokio::test]
async fn test_rejection_unauthorized() {
    use warp::Filter;
    let filter = warp::any()
        .and_then(|| async {
            Err::<String, _>(warp::reject::custom(crate::error::ServerRejection(
                ServerError::unauthorized("bad token"),
            )))
        })
        .recover(rejection_handler);

    let res = warp::test::request().reply(&filter).await;
    assert_eq!(res.status(), 401);
    let body: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(body["error"]["type"], "authentication_error");
}

// ── Auth filter tests ───────────────────────────────────────────────────────

#[tokio::test]
async fn test_auth_filter_valid_key() {
    use warp::Filter;
    let auth = Arc::new(MockAuthenticator);
    let filter = super::auth::with_auth(auth).map(|ctx: AuthContext| {
        warp::reply::json(&serde_json::json!({"account_id": ctx.account_id.as_str()}))
    });

    let res = warp::test::request()
        .header("authorization", "test-key")
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    let body: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(body["account_id"], "acc_1");
}

#[tokio::test]
async fn test_auth_filter_bearer_token() {
    use warp::Filter;
    let auth = Arc::new(MockAuthenticator);
    let filter = super::auth::with_auth(auth)
        .map(|ctx: AuthContext| {
            warp::reply::json(&serde_json::json!({"account_id": ctx.account_id.as_str()}))
        })
        .recover(rejection_handler);

    let res = warp::test::request()
        .header("authorization", "Bearer good-token")
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn test_auth_filter_invalid_key() {
    use warp::Filter;
    let auth = Arc::new(MockAuthenticator);
    let filter = super::auth::with_auth(auth)
        .map(|_: AuthContext| warp::reply())
        .recover(rejection_handler);

    let res = warp::test::request()
        .header("authorization", "bad-key")
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 401);
}
