use super::{
    errors::ServerResult,
    ids::{AccountId, SessionId},
    pagination::{CursorPage, PageRequest},
    time::Timestamp,
};

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub id: SessionId,
    pub account_id: AccountId,
    pub title: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

#[derive(Debug, Clone)]
pub struct SessionDetail {
    pub summary: SessionSummary,
    /// Session content in Bitrouter Core model format (JSON value).
    pub content: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct CreateSessionRequest {
    pub account_id: AccountId,
    pub title: Option<String>,
    pub content: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct SessionMutation {
    pub title: Option<String>,
    pub content: Option<serde_json::Value>,
}

/// Read-side session queries.
pub trait SessionQueryService {
    fn get_session(
        &self,
        id: &SessionId,
    ) -> impl Future<Output = ServerResult<SessionDetail>> + Send;

    fn list_sessions(
        &self,
        account_id: &AccountId,
        page: PageRequest,
    ) -> impl Future<Output = ServerResult<CursorPage<SessionSummary>>> + Send;
}

/// Write-side session mutations.
pub trait SessionWriteService {
    fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> impl Future<Output = ServerResult<SessionDetail>> + Send;

    fn update_session(
        &self,
        id: &SessionId,
        mutation: SessionMutation,
    ) -> impl Future<Output = ServerResult<SessionDetail>> + Send;

    fn delete_session(
        &self,
        id: &SessionId,
    ) -> impl Future<Output = ServerResult<()>> + Send;
}

/// Combined session service (convenience trait).
pub trait SessionService: SessionQueryService + SessionWriteService {}
impl<T: SessionQueryService + SessionWriteService> SessionService for T {}
