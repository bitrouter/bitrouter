//! ACP stdio proxy — implements the `Agent` trait facing a downstream
//! consumer (e.g. an editor) while forwarding to an upstream agent
//! subprocess via `AcpAgentProvider`.
//!
//! All ACP types are `!Send` and must live on a dedicated OS thread
//! with a single-threaded tokio runtime and `LocalSet`, matching the
//! pattern established in `connection.rs`.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use acp::Client as _;
use agent_client_protocol as acp;
use tokio::sync::mpsc;

use bitrouter_core::agents::event::{
    AgentEvent, PermissionOutcome, PermissionResponse, StopReason,
};
use bitrouter_core::agents::provider::AgentProvider;
use bitrouter_core::auth::claims::BitrouterClaims;
use bitrouter_core::auth::token as jwt_token;
use bitrouter_core::observe::{
    AgentObserveCallback, AgentRequestContext, AgentTurnFailureEvent, AgentTurnSuccessEvent,
    CallerContext,
};

use super::provider::AcpAgentProvider;

/// Configuration for the proxy agent.
pub struct ProxyConfig {
    /// Agent name for logging and spend tracking.
    pub agent_name: String,
    /// Pre-authenticated token (from CLI `--token` flag). When set, the
    /// ACP authenticate step is a no-op — the consumer is already trusted.
    pub pre_auth_token: Option<String>,
    /// The operator's CAIP-10 identity for JWT issuer verification.
    /// When `None`, issuer check is skipped (open proxy mode).
    pub operator_caip10: Option<String>,
    /// Optional observer for agent turn events (spend tracking, metrics).
    /// When `None`, no observation events are emitted.
    pub observer: Option<Arc<dyn AgentObserveCallback>>,
}

/// Shared handle to the consumer-facing ACP connection.
///
/// Set to `Some` after `AgentSideConnection::new()` returns. The proxy
/// uses this to send `session_notification` and `request_permission`
/// calls back to the downstream consumer.
///
/// The inner `Rc<AgentSideConnection>` allows cloning the handle out
/// of the `RefCell` borrow so the guard is dropped before any `.await`.
type ConsumerConn = Rc<RefCell<Option<Rc<acp::AgentSideConnection>>>>;

/// ACP proxy agent — sits between a downstream consumer and an upstream
/// ACP agent subprocess, intercepting auth, spend tracking, and forwarding
/// all protocol messages.
pub struct ProxyAgent {
    /// Upstream agent provider (Send + Sync, manages subprocess sessions).
    provider: Arc<AcpAgentProvider>,
    /// Consumer-facing ACP connection (set after construction).
    consumer_conn: ConsumerConn,
    /// Proxy configuration.
    config: ProxyConfig,
    /// Upstream session ID (set after `new_session` succeeds).
    upstream_session_id: RefCell<Option<String>>,
    /// Whether authentication has completed successfully.
    authenticated: RefCell<bool>,
    /// Resolved JWT claims from authentication (for spend tracking).
    caller_context: RefCell<CallerContext>,
}

impl ProxyAgent {
    /// Create a new proxy agent.
    ///
    /// The `consumer_conn` slot starts empty — the caller must fill it
    /// after [`acp::AgentSideConnection::new()`] returns.
    pub fn new(
        provider: Arc<AcpAgentProvider>,
        consumer_conn: ConsumerConn,
        config: ProxyConfig,
    ) -> Self {
        // If a pre-auth token is provided, start in authenticated state.
        let authenticated = config.pre_auth_token.is_some();
        let caller_context = if let Some(ref token) = config.pre_auth_token {
            claims_to_caller_context(jwt_token::verify(token).ok().as_ref())
        } else {
            CallerContext::default()
        };

        Self {
            provider,
            consumer_conn,
            config,
            upstream_session_id: RefCell::new(None),
            authenticated: RefCell::new(authenticated),
            caller_context: RefCell::new(caller_context),
        }
    }

    /// Send an ACP `SessionNotification` to the downstream consumer.
    ///
    /// Borrows the consumer connection immutably. Safe to call from
    /// async context because we never hold a mutable borrow concurrently.
    async fn notify_consumer(
        &self,
        session_id: acp::SessionId,
        update: acp::SessionUpdate,
    ) -> acp::Result<()> {
        let conn = self.consumer_conn.borrow().clone();
        let Some(conn) = conn else {
            return Err(acp::Error::internal_error());
        };
        conn.session_notification(acp::SessionNotification::new(session_id, update))
            .await
    }

    /// Forward a `request_permission` call to the downstream consumer.
    async fn forward_permission_to_consumer(
        &self,
        req: acp::RequestPermissionRequest,
    ) -> acp::Result<acp::RequestPermissionResponse> {
        let conn = self.consumer_conn.borrow().clone();
        let Some(conn) = conn else {
            return Err(acp::Error::internal_error());
        };
        conn.request_permission(req).await
    }

    /// Convert an `AgentEvent` stream from the upstream provider into ACP
    /// notifications sent to the downstream consumer, then return the final
    /// `PromptResponse`.
    async fn relay_events(
        &self,
        consumer_session_id: acp::SessionId,
        mut rx: mpsc::Receiver<AgentEvent>,
    ) -> acp::Result<acp::PromptResponse> {
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::TurnDone { stop_reason } => {
                    return Ok(acp::PromptResponse::new(to_acp_stop_reason(stop_reason)));
                }
                AgentEvent::Error { message } => {
                    return Err(acp::Error::new(
                        i32::from(acp::ErrorCode::InternalError),
                        message,
                    ));
                }
                AgentEvent::Disconnected => {
                    return Err(acp::Error::new(
                        i32::from(acp::ErrorCode::InternalError),
                        "upstream agent disconnected",
                    ));
                }
                AgentEvent::MessageChunk { text } => {
                    let update = acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                        acp::ContentBlock::Text(acp::TextContent::new(text)),
                    ));
                    self.notify_consumer(consumer_session_id.clone(), update)
                        .await?;
                }
                AgentEvent::ThoughtChunk { text } => {
                    let update = acp::SessionUpdate::AgentThoughtChunk(acp::ContentChunk::new(
                        acp::ContentBlock::Text(acp::TextContent::new(text)),
                    ));
                    self.notify_consumer(consumer_session_id.clone(), update)
                        .await?;
                }
                AgentEvent::NonTextContent { description } => {
                    // Map non-text content as a text annotation.
                    let update = acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk::new(
                        acp::ContentBlock::Text(acp::TextContent::new(description)),
                    ));
                    self.notify_consumer(consumer_session_id.clone(), update)
                        .await?;
                }
                AgentEvent::ToolCall {
                    tool_call_id,
                    title,
                    status,
                } => {
                    let update = acp::SessionUpdate::ToolCall(
                        acp::ToolCall::new(tool_call_id, title)
                            .status(convert_tool_call_status_to_acp(status)),
                    );
                    self.notify_consumer(consumer_session_id.clone(), update)
                        .await?;
                }
                AgentEvent::ToolCallUpdate {
                    tool_call_id,
                    title,
                    status,
                } => {
                    let mut fields = acp::ToolCallUpdateFields::new();
                    if let Some(title) = title {
                        fields = fields.title(title);
                    }
                    if let Some(status) = status {
                        fields = fields.status(convert_tool_call_status_to_acp(status));
                    }
                    let tc_update = acp::ToolCallUpdate::new(tool_call_id, fields);
                    let update = acp::SessionUpdate::ToolCallUpdate(tc_update);
                    self.notify_consumer(consumer_session_id.clone(), update)
                        .await?;
                }
                AgentEvent::PermissionRequest { id, request } => {
                    // Build ACP permission request from protocol-neutral type.
                    let options: Vec<acp::PermissionOption> = request
                        .options
                        .iter()
                        .map(|opt| {
                            acp::PermissionOption::new(
                                opt.id.clone(),
                                opt.title.clone(),
                                acp::PermissionOptionKind::AllowOnce,
                            )
                        })
                        .collect();
                    let fields = acp::ToolCallUpdateFields::new()
                        .title(request.title.clone())
                        .status(acp::ToolCallStatus::Pending);
                    let tool_call_update = acp::ToolCallUpdate::new("permission", fields);
                    let acp_req = acp::RequestPermissionRequest::new(
                        consumer_session_id.clone(),
                        tool_call_update,
                        options,
                    );
                    let acp_resp = self.forward_permission_to_consumer(acp_req).await?;

                    // Convert consumer's response back to protocol-neutral type.
                    let response = match acp_resp.outcome {
                        acp::RequestPermissionOutcome::Selected(sel) => PermissionResponse {
                            outcome: PermissionOutcome::Allowed {
                                selected_option: sel.option_id.to_string(),
                            },
                        },
                        _ => PermissionResponse {
                            outcome: PermissionOutcome::Denied,
                        },
                    };

                    // Resolve the upstream permission request.
                    let session_id = self.upstream_session_id.borrow().clone();
                    if let Some(ref session_id) = session_id {
                        let _ = self
                            .provider
                            .respond_permission(session_id, id, response)
                            .await;
                    }
                }
                AgentEvent::HistoryReplayDone => {
                    // Only emitted on the load_session replay receiver,
                    // never on the per-turn submit() stream the proxy
                    // pumps. Ignore defensively rather than fail.
                }
            }
        }

        // Stream ended without TurnDone — upstream channel was dropped.
        Err(acp::Error::new(
            i32::from(acp::ErrorCode::InternalError),
            "upstream event stream ended unexpectedly",
        ))
    }
}

#[async_trait::async_trait(?Send)]
impl acp::Agent for ProxyAgent {
    async fn initialize(
        &self,
        _args: acp::InitializeRequest,
    ) -> acp::Result<acp::InitializeResponse> {
        let mut auth_methods = Vec::new();

        // Only advertise auth if not pre-authenticated.
        if !*self.authenticated.borrow() {
            auth_methods.push(acp::AuthMethod::Agent(acp::AuthMethodAgent::new(
                "bitrouter-jwt",
                "BitRouter JWT",
            )));
        }

        Ok(acp::InitializeResponse::new(acp::ProtocolVersion::V1)
            .agent_capabilities(acp::AgentCapabilities::default())
            .auth_methods(auth_methods)
            .agent_info(
                acp::Implementation::new("bitrouter", env!("CARGO_PKG_VERSION"))
                    .title("BitRouter Agent Proxy"),
            ))
    }

    async fn authenticate(
        &self,
        args: acp::AuthenticateRequest,
    ) -> acp::Result<acp::AuthenticateResponse> {
        // Already authenticated (pre-auth token).
        if *self.authenticated.borrow() {
            return Ok(acp::AuthenticateResponse::new());
        }

        // Extract the JWT credential from the _meta field.
        // ACP `AuthMethodAgent` authentication: the credential is
        // conventionally passed in `_meta.token`.
        let token = args
            .meta
            .as_ref()
            .and_then(|m| m.get("token"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                acp::Error::new(
                    i32::from(acp::ErrorCode::InvalidParams),
                    "missing token in _meta.token",
                )
            })?;

        // Verify JWT signature.
        let claims = jwt_token::verify(token).map_err(|e| {
            acp::Error::new(
                i32::from(acp::ErrorCode::InvalidParams),
                format!("invalid JWT: {e}"),
            )
        })?;

        // Check expiration.
        jwt_token::check_expiration(&claims).map_err(|_| {
            acp::Error::new(i32::from(acp::ErrorCode::InvalidParams), "JWT expired")
        })?;

        // Verify issuer matches operator wallet (if configured).
        if let Some(ref expected) = self.config.operator_caip10
            && claims.iss != *expected
        {
            return Err(acp::Error::new(
                i32::from(acp::ErrorCode::InvalidParams),
                "JWT issuer does not match operator wallet",
            ));
        }

        // Store caller context for spend tracking.
        *self.caller_context.borrow_mut() = claims_to_caller_context(Some(&claims));
        *self.authenticated.borrow_mut() = true;

        Ok(acp::AuthenticateResponse::new())
    }

    async fn new_session(
        &self,
        args: acp::NewSessionRequest,
    ) -> acp::Result<acp::NewSessionResponse> {
        if !*self.authenticated.borrow() {
            return Err(acp::Error::new(
                i32::from(acp::ErrorCode::InvalidRequest),
                "not authenticated",
            ));
        }

        // Connect to upstream agent (spawns subprocess, performs handshake).
        // Forward the consumer's requested cwd verbatim — the proxy is a
        // transparent passthrough.
        let session_info = self.provider.connect(&args.cwd).await.map_err(|e| {
            acp::Error::new(
                i32::from(acp::ErrorCode::InternalError),
                format!("upstream connect failed: {e}"),
            )
        })?;

        let upstream_id = session_info.session_id.clone();
        *self.upstream_session_id.borrow_mut() = Some(upstream_id);

        // Return the session ID to the consumer. We use the upstream session
        // ID directly — no remapping needed for the single-session proxy case.
        Ok(acp::NewSessionResponse::new(session_info.session_id))
    }

    async fn prompt(&self, args: acp::PromptRequest) -> acp::Result<acp::PromptResponse> {
        let upstream_session_id = self.upstream_session_id.borrow().clone().ok_or_else(|| {
            acp::Error::new(
                i32::from(acp::ErrorCode::InvalidRequest),
                "no session — call new_session first",
            )
        })?;

        // Extract text from the prompt content blocks.
        let text = extract_prompt_text(&args.prompt);

        let turn_start = Instant::now();

        // Submit to upstream provider.
        let rx = self
            .provider
            .submit(&upstream_session_id, text)
            .await
            .map_err(|e| {
                acp::Error::new(
                    i32::from(acp::ErrorCode::InternalError),
                    format!("upstream submit failed: {e}"),
                )
            })?;

        // Relay events from upstream to consumer, returning the final response.
        let result = self.relay_events(args.session_id, rx).await;

        let latency_ms = turn_start.elapsed().as_millis() as u64;

        // Emit spend tracking / observation event.
        if let Some(ref observer) = self.config.observer {
            let ctx = AgentRequestContext {
                agent_name: self.config.agent_name.clone(),
                protocol: "acp".to_owned(),
                session_id: self.upstream_session_id.borrow().clone(),
                caller: self.caller_context.borrow().clone(),
                latency_ms,
            };
            match &result {
                Ok(_) => {
                    observer
                        .on_agent_turn_success(AgentTurnSuccessEvent { ctx })
                        .await;
                }
                Err(e) => {
                    observer
                        .on_agent_turn_failure(AgentTurnFailureEvent {
                            ctx,
                            error: e.message.clone(),
                        })
                        .await;
                }
            }
        }

        result
    }

    async fn cancel(&self, _args: acp::CancelNotification) -> acp::Result<()> {
        // Cancellation is not directly supported by the provider's
        // `AgentCommand` enum today. For now, acknowledge silently.
        // The upstream turn will complete or timeout on its own.
        Ok(())
    }
}

// ── conversion helpers ────────────────────────────────────────

/// Extract plaintext from ACP prompt content blocks.
///
/// Concatenates all `Text` blocks with newlines. Non-text blocks are
/// rendered as descriptive placeholders.
fn extract_prompt_text(blocks: &[acp::ContentBlock]) -> String {
    let mut parts = Vec::new();
    for block in blocks {
        match block {
            acp::ContentBlock::Text(tc) => parts.push(tc.text.clone()),
            acp::ContentBlock::Image(_) => parts.push("[image]".to_owned()),
            acp::ContentBlock::Audio(_) => parts.push("[audio]".to_owned()),
            acp::ContentBlock::ResourceLink(rl) => {
                parts.push(format!("[{}]({})", rl.name, rl.uri));
            }
            acp::ContentBlock::Resource(_) => parts.push("[resource]".to_owned()),
            _ => parts.push("[unknown content]".to_owned()),
        }
    }
    parts.join("\n")
}

/// Convert a protocol-neutral `StopReason` to ACP `StopReason`.
fn to_acp_stop_reason(reason: StopReason) -> acp::StopReason {
    match reason {
        StopReason::EndTurn => acp::StopReason::EndTurn,
        StopReason::MaxTokens => acp::StopReason::MaxTokens,
        StopReason::StopSequence | StopReason::ToolUse | StopReason::Other(_) => {
            acp::StopReason::EndTurn
        }
    }
}

/// Convert a protocol-neutral `ToolCallStatus` to ACP `ToolCallStatus`.
fn convert_tool_call_status_to_acp(
    status: bitrouter_core::agents::event::ToolCallStatus,
) -> acp::ToolCallStatus {
    match status {
        bitrouter_core::agents::event::ToolCallStatus::Pending => acp::ToolCallStatus::Pending,
        bitrouter_core::agents::event::ToolCallStatus::InProgress => {
            acp::ToolCallStatus::InProgress
        }
        bitrouter_core::agents::event::ToolCallStatus::Completed => acp::ToolCallStatus::Completed,
        bitrouter_core::agents::event::ToolCallStatus::Failed => acp::ToolCallStatus::Failed,
    }
}

/// Build a `CallerContext` from JWT claims.
fn claims_to_caller_context(claims: Option<&BitrouterClaims>) -> CallerContext {
    let Some(claims) = claims else {
        return CallerContext::default();
    };
    CallerContext {
        account_id: Some(claims.iss.clone()),
        key_id: claims.id.clone(),
        models: claims.mdl.clone(),
        budget: claims.bgt,
        budget_scope: claims.bsc,
        issued_at: claims.iat,
        key: claims.key.clone(),
        chain: None,
        policy_id: claims.pol.clone(),
    }
}

// ── public entry point ────────────────────────────────────────

/// Run the ACP stdio proxy on a dedicated OS thread.
///
/// This function blocks the calling thread until the consumer
/// disconnects or the upstream agent exits. It spawns a single-threaded
/// tokio runtime with `LocalSet` to confine all `!Send` ACP types.
///
/// # Arguments
///
/// * `provider` — upstream agent connection manager (Send + Sync)
/// * `config` — proxy configuration (agent name, auth settings)
pub fn run_stdio_proxy(provider: Arc<AcpAgentProvider>, config: ProxyConfig) -> Result<(), String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("failed to create runtime: {e}"))?;

    let local = tokio::task::LocalSet::new();
    rt.block_on(local.run_until(run_proxy_local(provider, config)))
}

async fn run_proxy_local(
    provider: Arc<AcpAgentProvider>,
    config: ProxyConfig,
) -> Result<(), String> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    // tokio-util compat adapters for AsyncRead/AsyncWrite
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
    let stdin = stdin.compat();
    let stdout = stdout.compat_write();

    // Create the shared consumer connection slot.
    let consumer_conn: ConsumerConn = Rc::new(RefCell::new(None));

    let proxy = ProxyAgent::new(provider, consumer_conn.clone(), config);

    let (conn, io_future) = acp::AgentSideConnection::new(proxy, stdout, stdin, |fut| {
        tokio::task::spawn_local(fut);
    });

    // Fill the connection slot so the ProxyAgent can send notifications.
    *consumer_conn.borrow_mut() = Some(Rc::new(conn));

    // Drive I/O until the consumer disconnects or an error occurs.
    let result = io_future.await;

    // Break the Rc reference cycle (ProxyAgent → consumer_conn → AgentSideConnection → ProxyAgent).
    consumer_conn.borrow_mut().take();

    result.map_err(|e| format!("proxy I/O error: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_text_from_content_blocks() {
        let blocks = vec![
            acp::ContentBlock::Text(acp::TextContent::new("Hello")),
            acp::ContentBlock::Text(acp::TextContent::new("World")),
        ];
        assert_eq!(extract_prompt_text(&blocks), "Hello\nWorld");
    }

    #[test]
    fn extract_text_empty_blocks() {
        let blocks: Vec<acp::ContentBlock> = Vec::new();
        assert_eq!(extract_prompt_text(&blocks), "");
    }

    #[test]
    fn stop_reason_round_trip() {
        assert!(matches!(
            to_acp_stop_reason(StopReason::EndTurn),
            acp::StopReason::EndTurn
        ));
        assert!(matches!(
            to_acp_stop_reason(StopReason::MaxTokens),
            acp::StopReason::MaxTokens
        ));
        assert!(matches!(
            to_acp_stop_reason(StopReason::Other("custom".into())),
            acp::StopReason::EndTurn
        ));
    }

    #[test]
    fn claims_to_context_with_none() {
        let ctx = claims_to_caller_context(None);
        assert!(ctx.account_id.is_none());
        assert!(ctx.key_id.is_none());
    }
}
