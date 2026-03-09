use std::future::Future;

use crate::models::shared::types::JsonValue;

use super::{
    errors::Result,
    ids::{AccountId, SessionId},
    pagination::{Page, PaginationRequest},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub account_id: AccountId,
    pub title: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCursor {
    pub value: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SessionMutation {
    AppendEntry { entry: JsonValue },
    ReplaceContent { content: JsonValue },
    SetTitle { title: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionCreateRequest {
    pub account_id: AccountId,
    pub title: Option<String>,
    pub content: JsonValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionListRequest {
    pub account_id: AccountId,
    pub pagination: PaginationRequest,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionUpdateRequest {
    pub session_id: SessionId,
    pub mutation: SessionMutation,
}

pub trait SessionQueryService {
    fn get(&self, session_id: SessionId) -> impl Future<Output = Result<JsonValue>> + Send;
    fn list(
        &self,
        request: SessionListRequest,
    ) -> impl Future<Output = Result<Page<SessionSummary>>> + Send;
}

pub trait SessionWriteService {
    fn create(
        &self,
        request: SessionCreateRequest,
    ) -> impl Future<Output = Result<SessionSummary>> + Send;
    fn update(
        &self,
        request: SessionUpdateRequest,
    ) -> impl Future<Output = Result<SessionSummary>> + Send;
    fn delete(&self, session_id: SessionId) -> impl Future<Output = Result<()>> + Send;
}

pub trait SessionService: SessionQueryService + SessionWriteService {}
