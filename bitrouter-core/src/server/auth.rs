use std::future::Future;

use http::Method;

use super::{
    errors::Result,
    ids::{AccountId, ApiKeyId, RequestId},
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AuthScope {
    Inference,
    SessionsRead,
    SessionsWrite,
    AccountsAdmin,
    BlobsRead,
    BlobsWrite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthSubject {
    Account(AccountId),
    ApiKey(ApiKeyId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthContext {
    pub request_id: RequestId,
    pub method: Method,
    pub path: String,
    pub presented_api_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthDecision {
    Allow {
        subject: AuthSubject,
        scopes: Vec<AuthScope>,
    },
    Deny {
        reason: String,
    },
}

pub trait Authenticator {
    fn authenticate(
        &self,
        context: AuthContext,
    ) -> impl Future<Output = Result<AuthDecision>> + Send;
}
