use std::{convert::Infallible, sync::Arc};

use bitrouter_core::server::{
    accounts::{AccountService, AdminBootstrapService, ApiKeyService},
    auth::Authenticator,
    blobs::{BlobStore, ObjectCatalog},
    sessions::{SessionQueryService, SessionService, SessionWriteService},
    usage::{RateLimiter, SpendLimitChecker, UsageMeter},
};
use warp::Filter;

pub fn with_authenticator<A>(
    authenticator: Arc<A>,
) -> impl Filter<Extract = (Arc<A>,), Error = Infallible> + Clone
where
    A: Authenticator + Send + Sync + 'static,
{
    warp::any().map(move || authenticator.clone())
}

pub fn with_rate_limiter<R>(
    rate_limiter: Arc<R>,
) -> impl Filter<Extract = (Arc<R>,), Error = Infallible> + Clone
where
    R: RateLimiter + Send + Sync + 'static,
{
    warp::any().map(move || rate_limiter.clone())
}

pub fn with_spend_limit_checker<S>(
    spend_limit_checker: Arc<S>,
) -> impl Filter<Extract = (Arc<S>,), Error = Infallible> + Clone
where
    S: SpendLimitChecker + Send + Sync + 'static,
{
    warp::any().map(move || spend_limit_checker.clone())
}

pub fn with_usage_meter<M>(
    usage_meter: Arc<M>,
) -> impl Filter<Extract = (Arc<M>,), Error = Infallible> + Clone
where
    M: UsageMeter + Send + Sync + 'static,
{
    warp::any().map(move || usage_meter.clone())
}

pub fn with_account_service<S>(
    account_service: Arc<S>,
) -> impl Filter<Extract = (Arc<S>,), Error = Infallible> + Clone
where
    S: AccountService + Send + Sync + 'static,
{
    warp::any().map(move || account_service.clone())
}

pub fn with_api_key_service<S>(
    api_key_service: Arc<S>,
) -> impl Filter<Extract = (Arc<S>,), Error = Infallible> + Clone
where
    S: ApiKeyService + Send + Sync + 'static,
{
    warp::any().map(move || api_key_service.clone())
}

pub fn with_admin_bootstrap_service<S>(
    admin_bootstrap_service: Arc<S>,
) -> impl Filter<Extract = (Arc<S>,), Error = Infallible> + Clone
where
    S: AdminBootstrapService + Send + Sync + 'static,
{
    warp::any().map(move || admin_bootstrap_service.clone())
}

pub fn with_session_service<S>(
    session_service: Arc<S>,
) -> impl Filter<Extract = (Arc<S>,), Error = Infallible> + Clone
where
    S: SessionService + Send + Sync + 'static,
{
    warp::any().map(move || session_service.clone())
}

pub fn with_session_query_service<S>(
    session_query_service: Arc<S>,
) -> impl Filter<Extract = (Arc<S>,), Error = Infallible> + Clone
where
    S: SessionQueryService + Send + Sync + 'static,
{
    warp::any().map(move || session_query_service.clone())
}

pub fn with_session_write_service<S>(
    session_write_service: Arc<S>,
) -> impl Filter<Extract = (Arc<S>,), Error = Infallible> + Clone
where
    S: SessionWriteService + Send + Sync + 'static,
{
    warp::any().map(move || session_write_service.clone())
}

pub fn with_object_catalog<S>(
    object_catalog: Arc<S>,
) -> impl Filter<Extract = (Arc<S>,), Error = Infallible> + Clone
where
    S: ObjectCatalog + Send + Sync + 'static,
{
    warp::any().map(move || object_catalog.clone())
}

pub fn with_blob_store<S>(
    blob_store: Arc<S>,
) -> impl Filter<Extract = (Arc<S>,), Error = Infallible> + Clone
where
    S: BlobStore + Send + Sync + 'static,
{
    warp::any().map(move || blob_store.clone())
}

#[cfg(test)]
mod tests {
    use std::future::{Future, ready};

    use bitrouter_core::server::{
        accounts::{AccountService, AccountStatus, AccountSummary},
        auth::{AuthContext, AuthDecision, Authenticator},
        errors::Result,
        ids::{AccountId, RequestId},
        pagination::{Page, PaginationRequest},
    };
    use warp::{Filter as _, http::Method, test::request};

    use super::{with_account_service, with_authenticator};

    #[derive(Clone)]
    struct MockAuthenticator;

    impl Authenticator for MockAuthenticator {
        fn authenticate(
            &self,
            _context: AuthContext,
        ) -> impl Future<Output = Result<AuthDecision>> + Send {
            ready(Ok(AuthDecision::Deny {
                reason: "not configured".to_owned(),
            }))
        }
    }

    #[derive(Clone)]
    struct MockAccountService;

    impl AccountService for MockAccountService {
        fn get_account(
            &self,
            account_id: AccountId,
        ) -> impl Future<Output = Result<AccountSummary>> + Send {
            ready(Ok(AccountSummary {
                account_id,
                status: AccountStatus::Active,
                display_name: Some("Test".to_owned()),
            }))
        }

        fn list_accounts(
            &self,
            _pagination: PaginationRequest,
        ) -> impl Future<Output = Result<Page<AccountSummary>>> + Send {
            ready(Ok(Page {
                items: Vec::new(),
                next_cursor: None,
            }))
        }

        fn update_status(
            &self,
            account_id: AccountId,
            _status: AccountStatus,
        ) -> impl Future<Output = Result<AccountSummary>> + Send {
            ready(Ok(AccountSummary {
                account_id,
                status: AccountStatus::Active,
                display_name: Some("Test".to_owned()),
            }))
        }
    }

    #[tokio::test]
    async fn with_authenticator_provides_service_to_filter_chain() {
        let auth = std::sync::Arc::new(MockAuthenticator);

        let filter = warp::path!("auth-check")
            .and(with_authenticator(auth))
            .and_then(|auth: std::sync::Arc<MockAuthenticator>| async move {
                let decision = auth
                    .authenticate(AuthContext {
                        request_id: RequestId::new("req_1"),
                        method: Method::GET,
                        path: "/auth-check".to_owned(),
                        presented_api_key: None,
                    })
                    .await
                    .expect("mock auth should not fail");
                Ok::<_, warp::Rejection>(format!("{decision:?}"))
            });

        let response = request().path("/auth-check").reply(&filter).await;
        assert_eq!(response.status(), 200);
        assert!(
            std::str::from_utf8(response.body())
                .expect("response should be utf-8")
                .contains("Deny")
        );
    }

    #[tokio::test]
    async fn with_account_service_provides_service_to_filter_chain() {
        let service = std::sync::Arc::new(MockAccountService);

        let filter = warp::path!("accounts" / String)
            .and(with_account_service(service))
            .and_then(
                |id: String, service: std::sync::Arc<MockAccountService>| async move {
                    let account = service
                        .get_account(AccountId::new(id))
                        .await
                        .expect("mock service should not fail");
                    Ok::<_, warp::Rejection>(account.account_id.to_string())
                },
            );

        let response = request().path("/accounts/acct_123").reply(&filter).await;
        assert_eq!(response.status(), 200);
        assert_eq!(response.body(), "acct_123");
    }
}
