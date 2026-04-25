//! The `AgentProvider` trait — the service primitive for interactive agents.
//!
//! Parallel to [`LanguageModel`](crate::models::language::language_model::LanguageModel)
//! for models and [`ToolProvider`](crate::tools::provider::ToolProvider) for tools.
//! Each implementation manages one agent subprocess or remote connection.

use std::path::Path;
use std::sync::Arc;

use dynosaur::dynosaur;
use tokio::sync::mpsc;

use crate::errors::Result;

use super::event::{AgentEvent, PermissionRequestId, PermissionResponse};
use super::session::{AgentCapabilities, AgentSessionInfo};

/// A provider that manages an interactive agent session.
///
/// Each implementation represents one agent subprocess or connection
/// (e.g. one ACP coding agent, one A2A remote agent). The lifecycle is:
///
/// 1. [`connect`](AgentProvider::connect) — spawn the process / open the
///    connection and perform the protocol handshake.
/// 2. [`submit`](AgentProvider::submit) — send a prompt and receive a
///    per-turn `mpsc::Receiver<AgentEvent>`. Only one prompt should be
///    in-flight at a time.
/// 3. [`respond_permission`](AgentProvider::respond_permission) — resolve
///    a pending [`AgentEvent::PermissionRequest`] by its ID.
/// 4. [`disconnect`](AgentProvider::disconnect) — gracefully tear down
///    the session.
///
/// Dropping the provider should also clean up resources (subprocess, TCP
/// connection) as a fallback, but [`disconnect`](AgentProvider::disconnect)
/// is the preferred explicit shutdown path.
#[dynosaur(pub DynAgentProvider = dyn(box) AgentProvider)]
pub trait AgentProvider: Send + Sync {
    /// The agent name, e.g. `"claude-code"`, `"codex"`.
    fn agent_name(&self) -> &str;

    /// The wire protocol this provider speaks, e.g. `"acp"`, `"a2a"`.
    fn protocol_name(&self) -> &str;

    /// Establish the agent session (spawn subprocess, handshake).
    ///
    /// `cwd` is the working directory advertised to the agent in the ACP
    /// `session/new` request; it is also used as the spawned subprocess's
    /// current dir so filesystem tools resolve relative paths the same way
    /// the agent sees them. Callers must pass an absolute path.
    fn connect(&self, cwd: &Path) -> impl Future<Output = Result<AgentSessionInfo>> + Send;

    /// Load an existing session by its agent-native id and replay its
    /// history. Returns the new [`AgentSessionInfo`] (the same
    /// `external_id` round-tripped) and a receiver that yields
    /// [`AgentEvent`]s for every replayed `session/update` notification,
    /// terminating with [`AgentEvent::HistoryReplayDone`].
    ///
    /// Only meaningful when [`capabilities`](Self::capabilities)
    /// reports `load_session = true`. Implementations should error
    /// otherwise; callers should gate at the call site.
    fn load_session(
        &self,
        cwd: &Path,
        external_id: &str,
    ) -> impl Future<Output = Result<(AgentSessionInfo, mpsc::Receiver<AgentEvent>)>> + Send;

    /// Capability flags advertised by the agent. Returns the cached
    /// value from the most recent successful handshake; before any
    /// [`connect`](Self::connect) or [`load_session`](Self::load_session)
    /// has succeeded the result is `Default::default()` (all flags
    /// false), per the ACP spec's defaults for unknown agents.
    fn capabilities(&self) -> AgentCapabilities;

    /// Submit a prompt and receive a per-turn event stream.
    ///
    /// The returned receiver yields [`AgentEvent`] values until the turn
    /// ends ([`TurnDone`](AgentEvent::TurnDone)), an error occurs, or
    /// the agent disconnects. The sender is dropped at turn end, causing
    /// the receiver to return `None`.
    fn submit(
        &self,
        session_id: &str,
        text: String,
    ) -> impl Future<Output = Result<mpsc::Receiver<AgentEvent>>> + Send;

    /// Resolve a pending permission request.
    ///
    /// `request_id` must match the `id` field from a previously received
    /// [`AgentEvent::PermissionRequest`].
    fn respond_permission(
        &self,
        session_id: &str,
        request_id: PermissionRequestId,
        response: PermissionResponse,
    ) -> impl Future<Output = Result<()>> + Send;

    /// Gracefully shut down the agent session.
    fn disconnect(&self, session_id: &str) -> impl Future<Output = Result<()>> + Send;
}

impl<T: AgentProvider> AgentProvider for Arc<T> {
    fn agent_name(&self) -> &str {
        (**self).agent_name()
    }

    fn protocol_name(&self) -> &str {
        (**self).protocol_name()
    }

    async fn connect(&self, cwd: &Path) -> Result<AgentSessionInfo> {
        (**self).connect(cwd).await
    }

    async fn load_session(
        &self,
        cwd: &Path,
        external_id: &str,
    ) -> Result<(AgentSessionInfo, mpsc::Receiver<AgentEvent>)> {
        (**self).load_session(cwd, external_id).await
    }

    fn capabilities(&self) -> AgentCapabilities {
        (**self).capabilities()
    }

    async fn submit(&self, session_id: &str, text: String) -> Result<mpsc::Receiver<AgentEvent>> {
        (**self).submit(session_id, text).await
    }

    async fn respond_permission(
        &self,
        session_id: &str,
        request_id: PermissionRequestId,
        response: PermissionResponse,
    ) -> Result<()> {
        (**self)
            .respond_permission(session_id, request_id, response)
            .await
    }

    async fn disconnect(&self, session_id: &str) -> Result<()> {
        (**self).disconnect(session_id).await
    }
}
