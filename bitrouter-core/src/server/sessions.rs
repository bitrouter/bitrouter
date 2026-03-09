use std::future::Future;

use crate::models::shared::types::JsonValue;

use super::{
    errors::Result,
    ids::{AccountId, RequestId, SessionId},
    pagination::{Page, PaginationRequest},
    time::{LifecycleState, RetentionPolicy, Timestamp},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCursor(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub account_id: AccountId,
    pub title: Option<String>,
    pub lifecycle: LifecycleState,
    pub retention: RetentionPolicy,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionRecord {
    pub summary: SessionSummary,
    pub content: JsonValue,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionMutation {
    pub session_id: Option<SessionId>,
    pub account_id: AccountId,
    pub title: Option<String>,
    pub content: JsonValue,
    pub request_id: Option<RequestId>,
    pub retention: Option<RetentionPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetSessionRequest {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListSessionsRequest {
    pub account_id: AccountId,
    pub pagination: PaginationRequest,
    pub cursor: Option<SessionCursor>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PutSessionRequest {
    pub mutation: SessionMutation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeleteSessionRequest {
    pub session_id: SessionId,
}

pub trait SessionQueryService {
    fn get_session(
        &self,
        request: GetSessionRequest,
    ) -> impl Future<Output = Result<SessionRecord>> + Send;

    fn list_sessions(
        &self,
        request: ListSessionsRequest,
    ) -> impl Future<Output = Result<Page<SessionSummary>>> + Send;
}

pub trait SessionWriteService {
    fn put_session(
        &self,
        request: PutSessionRequest,
    ) -> impl Future<Output = Result<SessionRecord>> + Send;

    fn delete_session(
        &self,
        request: DeleteSessionRequest,
    ) -> impl Future<Output = Result<()>> + Send;
}

pub trait SessionService: SessionQueryService + SessionWriteService {}

impl<T> SessionService for T where T: SessionQueryService + SessionWriteService {}
