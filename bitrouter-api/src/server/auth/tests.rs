use std::sync::Arc;

use bitrouter_core::server::{
    auth::{AuthContext, AuthDecision, Authenticator, AuthScope, AuthSubject},
    errors::{Result, ServerError},
    ids::{AccountId, RequestId},
};
use warp::Filter;

use super::filters::{auth_context_filter, rejection_handler};

struct MockAuthenticator;

impl Authenticator for MockAuthenticator {
    async fn authenticate(&self, context: AuthContext) -> Result<AuthDecision> {
        let is_authorized = context.authorization.as_deref() == Some("Bearer token");

        if is_authorized {
            Ok(AuthDecision::allow(
                AuthSubject::Account {
                    account_id: AccountId::from("acct_123"),
                },
                context.required_scopes,
            ))
        } else {
            Ok(AuthDecision::deny(ServerError::unauthorized("missing bearer token")))
        }
    }
}

#[tokio::test]
async fn auth_context_filter_allows_authenticated_requests() {
    let authenticator = Arc::new(MockAuthenticator);
    let filter = warp::path("protected")
        .and(auth_context_filter(
            authenticator,
            vec![AuthScope::Inference, AuthScope::UsageWrite],
        ))
        .map(|decision: AuthDecision| {
            let (subject, granted_scopes) = match decision {
                AuthDecision::Allow {
                    subject,
                    granted_scopes,
                } => (subject, granted_scopes),
                AuthDecision::Deny(_) => unreachable!(),
            };

            let subject = match subject {
                AuthSubject::Account { account_id } => account_id.to_string(),
                _ => unreachable!(),
            };

            warp::reply::json(&serde_json::json!({
                "subject": subject,
                "scope_count": granted_scopes.len(),
            }))
        });

    let response = warp::test::request()
        .method("GET")
        .path("/protected")
        .header("authorization", "Bearer token")
        .header("x-request-id", RequestId::from("req_123").to_string())
        .reply(&filter)
        .await;

    assert_eq!(response.status(), 200);

    let json: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
    assert_eq!(json["subject"], "acct_123");
    assert_eq!(json["scope_count"], 2);
}

#[tokio::test]
async fn auth_context_filter_maps_domain_denials_to_http_errors() {
    let authenticator = Arc::new(MockAuthenticator);
    let filter = warp::path("protected")
        .and(auth_context_filter(authenticator, vec![AuthScope::Inference]))
        .map(|_: AuthDecision| warp::reply())
        .recover(rejection_handler);

    let response = warp::test::request()
        .method("GET")
        .path("/protected")
        .reply(&filter)
        .await;

    assert_eq!(response.status(), 401);

    let json: serde_json::Value = serde_json::from_slice(response.body()).unwrap();
    assert_eq!(json["error"]["type"], "unauthorized");
    assert_eq!(json["error"]["message"], "missing bearer token");
}
