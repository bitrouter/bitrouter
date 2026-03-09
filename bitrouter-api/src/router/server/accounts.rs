//! Warp filters for account and API-key management endpoints.

use std::sync::Arc;

use bitrouter_core::server::{
    accounts::{
        Account, AccountService, AccountStatus, AdminBootstrapService, ApiKeyRecord, ApiKeyService,
        CreateAccountRequest, CreateApiKeyResponse, KeyPolicy, SubKeySpec,
    },
    auth::AuthScope,
    ids::AccountId,
    pagination::{CursorPage, PageRequest},
};
use serde::{Deserialize, Serialize};
use warp::Filter;

use crate::error::ServerRejection;

// ---------------------------------------------------------------------------
// Request / response DTOs
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateAccountBody {
    name: String,
}

#[derive(Deserialize)]
struct CreateKeyBody {
    name: String,
    scopes: Vec<String>,
    #[serde(default)]
    rate_limit_per_minute: Option<u32>,
    #[serde(default)]
    spend_limit_cents: Option<u64>,
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_limit() -> u32 {
    20
}

#[derive(Serialize)]
struct AccountResponse {
    id: String,
    name: String,
    status: String,
    created_at: i64,
    updated_at: i64,
}

#[derive(Serialize)]
struct ApiKeyRecordResponse {
    id: String,
    account_id: String,
    name: String,
    prefix: String,
    scopes: Vec<String>,
    rate_limit_per_minute: Option<u32>,
    spend_limit_cents: Option<u64>,
    created_at: i64,
    revoked_at: Option<i64>,
}

#[derive(Serialize)]
struct CreateApiKeyResponseBody {
    record: ApiKeyRecordResponse,
    plaintext_key: String,
}

#[derive(Serialize)]
struct PageResponse<T: Serialize> {
    items: Vec<T>,
    next_cursor: Option<String>,
    has_more: bool,
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn account_status_str(s: &AccountStatus) -> &'static str {
    match s {
        AccountStatus::Active => "active",
        AccountStatus::Suspended => "suspended",
        AccountStatus::Closed => "closed",
    }
}

fn account_to_response(a: Account) -> AccountResponse {
    AccountResponse {
        id: a.id.to_string(),
        name: a.name,
        status: account_status_str(&a.status).to_owned(),
        created_at: a.created_at.as_secs(),
        updated_at: a.updated_at.as_secs(),
    }
}

fn scope_to_string(s: &AuthScope) -> String {
    match s {
        AuthScope::Inference => "inference".to_owned(),
        AuthScope::Admin => "admin".to_owned(),
        AuthScope::AccountRead => "account:read".to_owned(),
        AuthScope::AccountWrite => "account:write".to_owned(),
        AuthScope::SessionRead => "session:read".to_owned(),
        AuthScope::SessionWrite => "session:write".to_owned(),
        AuthScope::BlobRead => "blob:read".to_owned(),
        AuthScope::BlobWrite => "blob:write".to_owned(),
    }
}

fn string_to_scope(s: &str) -> Option<AuthScope> {
    match s {
        "inference" => Some(AuthScope::Inference),
        "admin" => Some(AuthScope::Admin),
        "account:read" => Some(AuthScope::AccountRead),
        "account:write" => Some(AuthScope::AccountWrite),
        "session:read" => Some(AuthScope::SessionRead),
        "session:write" => Some(AuthScope::SessionWrite),
        "blob:read" => Some(AuthScope::BlobRead),
        "blob:write" => Some(AuthScope::BlobWrite),
        _ => None,
    }
}

fn key_record_to_response(r: ApiKeyRecord) -> ApiKeyRecordResponse {
    ApiKeyRecordResponse {
        id: r.id.to_string(),
        account_id: r.account_id.to_string(),
        name: r.name,
        prefix: r.prefix,
        scopes: r.scopes.iter().map(scope_to_string).collect(),
        rate_limit_per_minute: r.policy.rate_limit_per_minute,
        spend_limit_cents: r.policy.spend_limit_cents,
        created_at: r.created_at.as_secs(),
        revoked_at: r.revoked_at.map(|t| t.as_secs()),
    }
}

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

/// POST /v1/accounts — create an account.
pub fn create_account_filter<S>(
    service: Arc<S>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    S: AccountService + Send + Sync + 'static,
{
    warp::path!("v1" / "accounts")
        .and(warp::post())
        .and(warp::body::json::<CreateAccountBody>())
        .and(warp::any().map(move || service.clone()))
        .and_then(handle_create_account)
}

/// GET /v1/accounts/:id — get a single account.
pub fn get_account_filter<S>(
    service: Arc<S>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    S: AccountService + Send + Sync + 'static,
{
    warp::path!("v1" / "accounts" / String)
        .and(warp::get())
        .and(warp::any().map(move || service.clone()))
        .and_then(handle_get_account)
}

/// GET /v1/accounts — list accounts with cursor pagination.
pub fn list_accounts_filter<S>(
    service: Arc<S>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    S: AccountService + Send + Sync + 'static,
{
    warp::path!("v1" / "accounts")
        .and(warp::get())
        .and(warp::query::<ListQuery>())
        .and(warp::any().map(move || service.clone()))
        .and_then(handle_list_accounts)
}

/// POST /v1/accounts/:id/keys — create an API key for an account.
pub fn create_key_filter<K>(
    service: Arc<K>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    K: ApiKeyService + Send + Sync + 'static,
{
    warp::path!("v1" / "accounts" / String / "keys")
        .and(warp::post())
        .and(warp::body::json::<CreateKeyBody>())
        .and(warp::any().map(move || service.clone()))
        .and_then(handle_create_key)
}

/// GET /v1/accounts/:id/keys — list API keys for an account.
pub fn list_keys_filter<K>(
    service: Arc<K>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    K: ApiKeyService + Send + Sync + 'static,
{
    warp::path!("v1" / "accounts" / String / "keys")
        .and(warp::get())
        .and(warp::query::<ListQuery>())
        .and(warp::any().map(move || service.clone()))
        .and_then(handle_list_keys)
}

/// DELETE /v1/keys/:id — revoke an API key.
pub fn revoke_key_filter<K>(
    service: Arc<K>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    K: ApiKeyService + Send + Sync + 'static,
{
    warp::path!("v1" / "keys" / String)
        .and(warp::delete())
        .and(warp::any().map(move || service.clone()))
        .and_then(handle_revoke_key)
}

/// POST /v1/admin/bootstrap — initial admin bootstrap.
pub fn bootstrap_filter<B>(
    service: Arc<B>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    B: AdminBootstrapService + Send + Sync + 'static,
{
    warp::path!("v1" / "admin" / "bootstrap")
        .and(warp::post())
        .and(warp::body::json::<CreateAccountBody>())
        .and(warp::any().map(move || service.clone()))
        .and_then(handle_bootstrap)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn handle_create_account<S>(
    body: CreateAccountBody,
    service: Arc<S>,
) -> Result<impl warp::Reply, warp::Rejection>
where
    S: AccountService + Send + Sync + 'static,
{
    let account = service
        .create_account(CreateAccountRequest { name: body.name })
        .await
        .map_err(|e| warp::reject::custom(ServerRejection(e)))?;

    Ok(warp::reply::with_status(
        warp::reply::json(&account_to_response(account)),
        warp::http::StatusCode::CREATED,
    ))
}

async fn handle_get_account<S>(
    id: String,
    service: Arc<S>,
) -> Result<impl warp::Reply, warp::Rejection>
where
    S: AccountService + Send + Sync + 'static,
{
    let account = service
        .get_account(&AccountId::new(id))
        .await
        .map_err(|e| warp::reject::custom(ServerRejection(e)))?;

    Ok(warp::reply::json(&account_to_response(account)))
}

async fn handle_list_accounts<S>(
    query: ListQuery,
    service: Arc<S>,
) -> Result<impl warp::Reply, warp::Rejection>
where
    S: AccountService + Send + Sync + 'static,
{
    let page = service
        .list_accounts(PageRequest {
            cursor: query.cursor,
            limit: query.limit,
        })
        .await
        .map_err(|e| warp::reject::custom(ServerRejection(e)))?;

    Ok(warp::reply::json(&cursor_page_response(
        page,
        account_to_response,
    )))
}

async fn handle_create_key<K>(
    account_id: String,
    body: CreateKeyBody,
    service: Arc<K>,
) -> Result<impl warp::Reply, warp::Rejection>
where
    K: ApiKeyService + Send + Sync + 'static,
{
    let scopes: Vec<AuthScope> = body
        .scopes
        .iter()
        .filter_map(|s| string_to_scope(s))
        .collect();

    let resp = service
        .create_key(
            &AccountId::new(account_id),
            SubKeySpec {
                name: body.name,
                scopes,
                policy: KeyPolicy {
                    rate_limit_per_minute: body.rate_limit_per_minute,
                    spend_limit_cents: body.spend_limit_cents,
                },
            },
        )
        .await
        .map_err(|e| warp::reject::custom(ServerRejection(e)))?;

    Ok(warp::reply::with_status(
        warp::reply::json(&create_key_response(resp)),
        warp::http::StatusCode::CREATED,
    ))
}

async fn handle_list_keys<K>(
    account_id: String,
    query: ListQuery,
    service: Arc<K>,
) -> Result<impl warp::Reply, warp::Rejection>
where
    K: ApiKeyService + Send + Sync + 'static,
{
    let page = service
        .list_keys(
            &AccountId::new(account_id),
            PageRequest {
                cursor: query.cursor,
                limit: query.limit,
            },
        )
        .await
        .map_err(|e| warp::reject::custom(ServerRejection(e)))?;

    Ok(warp::reply::json(&cursor_page_response(
        page,
        key_record_to_response,
    )))
}

async fn handle_revoke_key<K>(
    key_id: String,
    service: Arc<K>,
) -> Result<impl warp::Reply, warp::Rejection>
where
    K: ApiKeyService + Send + Sync + 'static,
{
    service
        .revoke_key(&bitrouter_core::server::ids::ApiKeyId::new(key_id))
        .await
        .map_err(|e| warp::reject::custom(ServerRejection(e)))?;

    Ok(warp::reply::with_status(
        warp::reply::json(&serde_json::json!({})),
        warp::http::StatusCode::NO_CONTENT,
    ))
}

async fn handle_bootstrap<B>(
    body: CreateAccountBody,
    service: Arc<B>,
) -> Result<impl warp::Reply, warp::Rejection>
where
    B: AdminBootstrapService + Send + Sync + 'static,
{
    let resp = service
        .bootstrap(CreateAccountRequest { name: body.name })
        .await
        .map_err(|e| warp::reject::custom(ServerRejection(e)))?;

    Ok(warp::reply::with_status(
        warp::reply::json(&create_key_response(resp)),
        warp::http::StatusCode::CREATED,
    ))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn create_key_response(resp: CreateApiKeyResponse) -> CreateApiKeyResponseBody {
    CreateApiKeyResponseBody {
        record: key_record_to_response(resp.record),
        plaintext_key: resp.plaintext_key,
    }
}

fn cursor_page_response<T, R: Serialize>(
    page: CursorPage<T>,
    convert: fn(T) -> R,
) -> PageResponse<R> {
    PageResponse {
        items: page.items.into_iter().map(convert).collect(),
        next_cursor: page.next_cursor,
        has_more: page.has_more,
    }
}
