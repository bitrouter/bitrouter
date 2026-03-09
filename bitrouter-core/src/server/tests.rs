use super::{
    auth::{AuthContext, AuthDecision, AuthScope, AuthSubject, Authenticator},
    errors::ServerError,
    ids::{AccountId, RequestId},
    pagination::{Page, PaginationRequest},
};

struct MockAuthenticator;

impl Authenticator for MockAuthenticator {
    async fn authenticate(&self, context: AuthContext) -> super::errors::Result<AuthDecision> {
        if context.authorization.is_some() {
            Ok(AuthDecision::allow(
                AuthSubject::Account {
                    account_id: AccountId::from("acct_123"),
                },
                context.required_scopes,
            ))
        } else {
            Ok(AuthDecision::deny(ServerError::unauthorized(
                "missing token",
            )))
        }
    }
}

#[test]
fn authenticator_contract_supports_async_impls() {
    let authenticator = MockAuthenticator;
    let future = authenticator.authenticate(AuthContext {
        request_id: RequestId::from("req_123"),
        method: http::Method::GET,
        path: "/v1/sessions".to_owned(),
        authorization: Some("Bearer test".to_owned()),
        remote_addr: None,
        required_scopes: vec![AuthScope::SessionsRead],
    });

    std::mem::drop(future);
}

#[test]
fn pagination_page_keeps_items_and_cursor() {
    let page = Page {
        items: vec!["a".to_owned(), "b".to_owned()],
        next_cursor: Some("cursor-2".to_owned()),
    };

    assert_eq!(page.items.len(), 2);
    assert_eq!(page.next_cursor.as_deref(), Some("cursor-2"));
    assert_eq!(PaginationRequest::default().limit, None);
}
