use std::{future::Future, net::IpAddr};

use http::Method;

use super::{
    errors::{Result, ServerError},
    ids::{AccountId, ApiKeyId, RequestId},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthScope {
    Inference,
    AccountsRead,
    AccountsWrite,
    SessionsRead,
    SessionsWrite,
    BlobsRead,
    BlobsWrite,
    UsageRead,
    UsageWrite,
    Admin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthSubject {
    Anonymous,
    Account {
        account_id: AccountId,
    },
    ApiKey {
        account_id: AccountId,
        api_key_id: ApiKeyId,
    },
    Service {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthContext {
    pub request_id: RequestId,
    pub method: Method,
    pub path: String,
    pub authorization: Option<String>,
    pub remote_addr: Option<IpAddr>,
    pub required_scopes: Vec<AuthScope>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthDecision {
    Allow {
        subject: AuthSubject,
        granted_scopes: Vec<AuthScope>,
    },
    Deny(ServerError),
}

impl AuthDecision {
    pub fn allow(subject: AuthSubject, granted_scopes: Vec<AuthScope>) -> Self {
        Self::Allow {
            subject,
            granted_scopes,
        }
    }

    pub fn deny(error: ServerError) -> Self {
        Self::Deny(error)
    }
}

pub trait Authenticator {
    fn authenticate(
        &self,
        context: AuthContext,
    ) -> impl Future<Output = Result<AuthDecision>> + Send;
}
