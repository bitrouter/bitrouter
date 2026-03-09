use super::{
    errors::ServerResult,
    ids::{AccountId, ApiKeyId, RequestId},
};

/// The subject of an authentication request.
#[derive(Debug, Clone)]
pub enum AuthSubject {
    ApiKey(String),
    Bearer(String),
}

/// A scope that an authenticated context can access.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthScope {
    Inference,
    Admin,
    AccountRead,
    AccountWrite,
    SessionRead,
    SessionWrite,
    BlobRead,
    BlobWrite,
}

/// The result of an authentication decision.
#[derive(Debug, Clone)]
pub enum AuthDecision {
    Allow,
    Deny { reason: String },
}

/// The authenticated context for a request.
#[derive(Debug, Clone)]
pub struct AuthContext {
    pub account_id: AccountId,
    pub key_id: ApiKeyId,
    pub request_id: RequestId,
    pub scopes: Vec<AuthScope>,
}

/// Authenticates incoming requests and produces an [`AuthContext`].
pub trait Authenticator {
    fn authenticate(
        &self,
        subject: &AuthSubject,
    ) -> impl Future<Output = ServerResult<AuthContext>> + Send;

    fn check_scope(
        &self,
        context: &AuthContext,
        required: &AuthScope,
    ) -> impl Future<Output = ServerResult<AuthDecision>> + Send;
}
