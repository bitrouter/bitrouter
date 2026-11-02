//! Trusted, per-call tenant dispatch over the gateway's existing connection pool.
//!
//! Bind explicitly observed capabilities with `TenantCapabilities::bind`, then
//! call `gateway.tenant(credential, &binding)?.chat_reply(&request).await`.
//! Credentials must come from trusted application state. Neither tenant nor
//! principal identity is accepted here; the service derives both from authority.
//! Each helper is consumed by one call. Correlation is never an idempotency key.
//! No discovery, capability refresh, retry, replay, or remote cancellation occurs.
//!
//! ```no_run
//! use bitrouter_ai::{
//!     protocol::chat_completions::{ChatRequest, ChatResponse, ChatChunk},
//!     tenancy::operation::DispatchBinding,
//! };
//! use bitrouter_sdk::gateway::{GatewayClient, tenancy::{Reply, Body, Error}};
//!
//! async fn dispatch(gateway: &GatewayClient, trusted_credential: &str,
//!                   binding: &DispatchBinding, request: &ChatRequest)
//!     -> Result<Reply<Body<ChatResponse, ChatChunk>>, Error>
//! {
//!     gateway.tenant(trusted_credential, binding)?.chat_reply(request).await
//! }
//! ```

use std::{fmt, pin::Pin};

use bitrouter_ai::{
    decisions::DecisionRequest,
    gateway::GatewayErrorEnvelope,
    protocol::{
        chat_completions::{ChatChunk, ChatRequest, ChatResponse},
        decisions::DecisionsResponse,
        messages::{MessagesEvent, MessagesRequest, MessagesResponse},
        responses::{ResponsesEvent, ResponsesRequest, ResponsesResponse},
    },
    stream::StreamItem,
    tenancy::{
        identity::ExecutionId,
        operation::{DispatchBinding, DispatchState, Error as TenancyError, ErrorCode},
        validation::Validate,
    },
};
use futures_core::Stream;
use futures_util::StreamExt;
use reqwest::{
    Response,
    header::{self, HeaderMap, HeaderValue},
};
use serde::{Serialize, de::DeserializeOwned};

use super::{GatewayClient, execution::media_type};
use crate::{error::Error as SdkError, stream::Protocol, transport};

/// Validated tenant response metadata. No raw headers or control URLs escape.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Metadata {
    /// Observed HTTP status, not an execution outcome or durable receipt state.
    pub status: u16,
    /// Tenant execution identity from `x-bitrouter-request-id`, not correlation.
    /// Missing on preadmission failures; never inferred from provider output.
    pub execution_id: Option<ExecutionId>,
}

/// A response with its identity retained before streaming or after JSON decoding.
pub struct Reply<T> {
    /// Validated HTTP metadata; it grants no replay or cancellation authority.
    pub metadata: Metadata,
    /// Typed protocol output.
    pub body: T,
}

/// Same-protocol output, selected by the typed request's stream flag.
pub enum Body<T, E> {
    /// Fully decoded, bounded JSON.
    Json(T),
    /// Pull-driven events with an explicit terminal or an error.
    Stream(EventStream<E>),
}

/// Dropping this stream cancels local transport consumption only. It does not
/// prove provider cancellation, completion, receipt durability, or safe replay.
pub type EventStream<T> = Pin<Box<dyn Stream<Item = Result<StreamItem<T>, Error>> + Send>>;

/// Why a response could not establish a trustworthy protocol result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Problem {
    /// Missing, duplicate, malformed, or inconsistent execution metadata.
    Metadata,
    /// A success status other than 200, including 202, is not model output.
    Status,
    /// Missing, ambiguous, malformed, or unsupported representation metadata.
    ContentType,
    /// Reading failed after response headers; execution outcome remains unknown.
    Transport,
    /// A body or SSE event exceeded the configured byte budget.
    Limit,
    /// The protocol's explicit terminal was not observed.
    IncompleteStream,
    /// The public JSON or event contract was not satisfied.
    Contract,
}

/// A response failure, retaining only safe metadata and typed public errors.
#[derive(Debug)]
pub struct Failure {
    /// HTTP status and any validated tenant execution identity.
    pub metadata: Metadata,
    /// Validated tenancy error, including when no execution ID is known.
    pub tenancy_error: Option<TenancyError>,
    /// Ordinary gateway or protocol stream error, when supplied by the service.
    pub gateway_error: Option<GatewayErrorEnvelope>,
    /// Validation or read failure. Structured errors remain inspectable, but
    /// must not be treated as dispatch evidence when this is present.
    pub problem: Option<Problem>,
}

/// Tenant dispatch failures never trigger another request.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Local validation failed before sending anything.
    #[error("invalid tenant dispatch request: {0}")]
    Request(&'static str),
    /// No trustworthy HTTP response was received. No request data is retained.
    #[error("tenant transport failed; execution outcome is unknown")]
    Transport,
    /// HTTP metadata is retained even when body consumption or validation fails.
    #[error("tenant response failed (HTTP {}); inspect dispatch evidence", .0.metadata.status)]
    Response(Box<Failure>),
}

impl Error {
    /// Conservative provider-dispatch evidence, never a retry recommendation.
    pub fn dispatch_state(&self) -> DispatchState {
        match self {
            Self::Request(_) => DispatchState::NotStarted,
            Self::Response(failure) if failure.problem.is_none() => failure
                .tenancy_error
                .as_ref()
                .map_or(DispatchState::Unknown, |error| error.dispatch_state),
            _ => DispatchState::Unknown,
        }
    }
}

/// One trusted submission, borrowing the gateway pool without mutating defaults.
pub struct TenantGateway<'a> {
    client: &'a GatewayClient,
    headers: HeaderMap,
}

impl fmt::Debug for TenantGateway<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TenantGateway").finish_non_exhaustive()
    }
}

impl GatewayClient {
    /// Prepare one tenant call using trusted authority and an explicit binding.
    /// The credential overrides this request's default authorization only.
    ///
    /// # Errors
    /// Rejects invalid bindings or Bearer credentials before network I/O.
    pub fn tenant(
        &self,
        credential: &str,
        binding: &DispatchBinding,
    ) -> Result<TenantGateway<'_>, Error> {
        binding
            .validate()
            .map_err(|_| Error::Request("invalid strict dispatch binding"))?;
        // RFC 6750 b64token: padding is permitted only at the end.
        // https://www.rfc-editor.org/rfc/rfc6750#section-2.1
        let bare = credential.trim_end_matches('=');
        if !(1..=4096).contains(&credential.len())
            || bare.is_empty()
            || !bare
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~+/".contains(&b))
        {
            return Err(Error::Request("invalid Bearer credential"));
        }
        let mut token = HeaderValue::from_str(&format!("Bearer {credential}"))
            .map_err(|_| Error::Request("invalid Bearer credential"))?;
        token.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, token);
        for (name, value) in [
            (
                "bitrouter-tenancy-version",
                binding.contract_version.as_str(),
            ),
            (
                "bitrouter-installation-id",
                binding.installation_id.as_str(),
            ),
            ("bitrouter-instance-id", binding.instance_id.as_str()),
            (
                "bitrouter-capability-revision",
                binding.capability_revision.as_str(),
            ),
            (
                "bitrouter-execution-revision",
                binding.execution_revision.as_str(),
            ),
            ("bitrouter-correlation-id", binding.correlation_id.as_str()),
            ("bitrouter-receipt-mode", binding.receipt_mode.as_str()),
        ] {
            headers.insert(
                name,
                HeaderValue::from_str(value)
                    .map_err(|_| Error::Request("invalid dispatch header"))?,
            );
        }
        Ok(TenantGateway {
            client: self,
            headers,
        })
    }
}

impl TenantGateway<'_> {
    /// Dispatch Chat Completions as JSON or SSE, retaining explicit stream ends.
    ///
    /// # Errors
    /// Returns local validation, transport, service, or protocol failures.
    pub async fn chat_reply(
        self,
        request: &ChatRequest,
    ) -> Result<Reply<Body<ChatResponse, ChatChunk>>, Error> {
        reject_stream_shadow(&request.extra)?;
        self.relay(
            "v1/chat/completions",
            request,
            request.stream,
            Protocol::Chat,
        )
        .await
    }

    /// Dispatch Messages as JSON or SSE.
    ///
    /// # Errors
    /// Returns local validation, transport, service, or protocol failures.
    pub async fn messages_reply(
        self,
        request: &MessagesRequest,
    ) -> Result<Reply<Body<MessagesResponse, MessagesEvent>>, Error> {
        reject_stream_shadow(&request.extra)?;
        self.relay("v1/messages", request, request.stream, Protocol::Messages)
            .await
    }

    /// Dispatch Responses as JSON or SSE, preserving completed/incomplete/failed ends.
    ///
    /// # Errors
    /// Returns local validation, transport, service, or protocol failures.
    pub async fn responses_reply(
        self,
        request: &ResponsesRequest,
    ) -> Result<Reply<Body<ResponsesResponse, ResponsesEvent>>, Error> {
        reject_stream_shadow(&request.extra)?;
        self.relay("v1/responses", request, request.stream, Protocol::Responses)
            .await
    }

    /// Dispatch native Decisions JSON without generation-protocol conversion.
    ///
    /// # Errors
    /// Returns transport, service, or protocol failures, without retry.
    pub async fn decisions(
        self,
        request: &DecisionRequest,
    ) -> Result<Reply<DecisionsResponse>, Error> {
        let (response, metadata) = self
            .send("v1/decisions", request, "application/json")
            .await?;
        let body = self.json(response, &metadata).await?;
        Ok(Reply { metadata, body })
    }

    async fn relay<T: DeserializeOwned, E: DeserializeOwned + Send + 'static>(
        self,
        path: &str,
        request: &impl Serialize,
        streaming: bool,
        protocol: Protocol,
    ) -> Result<Reply<Body<T, E>>, Error> {
        let accept = if streaming {
            "text/event-stream"
        } else {
            "application/json"
        };
        let (response, metadata) = self.send(path, request, accept).await?;
        let body = if streaming {
            let context = metadata.clone();
            let stream = crate::stream::decode_relay(
                response,
                self.client.options.max_event_bytes,
                protocol,
            );
            Body::Stream(Box::pin(stream.map(move |item| {
                item.map_err(|error| response_error(&context, error))
            })))
        } else {
            Body::Json(self.json(response, &metadata).await?)
        };
        Ok(Reply { metadata, body })
    }

    async fn json<T: DeserializeOwned>(
        &self,
        response: Response,
        metadata: &Metadata,
    ) -> Result<T, Error> {
        let bytes = transport::body(response, self.client.options.max_response_bytes)
            .await
            .map_err(|error| response_error(metadata, error))?;
        serde_json::from_slice(&bytes).map_err(|_| failure(metadata, Problem::Contract))
    }

    async fn send(
        &self,
        path: &str,
        request: &impl Serialize,
        accept: &str,
    ) -> Result<(Response, Metadata), Error> {
        let url = self
            .client
            .url(path)
            .map_err(|_| Error::Request("invalid dispatch path"))?;
        // Build before sending so serialization errors remain definite local failures.
        let request = self
            .client
            .http
            .post(url)
            .headers(self.headers.clone())
            .header(header::ACCEPT, accept)
            .json(request)
            .build()
            .map_err(|_| Error::Request("invalid request encoding"))?;
        let response = self
            .client
            .http
            .execute(request)
            .await
            .map_err(|_| Error::Transport)?;
        let status = response.status().as_u16();
        let identity = execution_id(response.headers());
        let metadata_problem = identity.is_err().then_some(Problem::Metadata);
        let metadata = Metadata {
            status,
            execution_id: identity.ok().flatten(),
        };
        if status != 200 {
            return Err(read_failure(
                response,
                metadata,
                metadata_problem,
                self.client.options.max_response_bytes,
            )
            .await);
        }
        if metadata_problem.is_some() || metadata.execution_id.is_none() {
            return Err(failure(&metadata, Problem::Metadata));
        }
        if !representation(response.headers(), accept) {
            return Err(failure(&metadata, Problem::ContentType));
        }
        Ok((response, metadata))
    }
}

fn reject_stream_shadow(
    extra: &std::collections::HashMap<String, serde_json::Value>,
) -> Result<(), Error> {
    if extra.contains_key("stream") {
        return Err(Error::Request(
            "extensions must not override the typed stream field",
        ));
    }
    Ok(())
}

fn execution_id(headers: &HeaderMap) -> Result<Option<ExecutionId>, ()> {
    let mut values = headers.get_all("x-bitrouter-request-id").iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(());
    }
    ExecutionId::try_from(value.to_str().map_err(|_| ())?)
        .map(Some)
        .map_err(|_| ())
}

fn representation(headers: &HeaderMap, expected: &str) -> bool {
    let mut values = headers.get_all(header::CONTENT_TYPE).iter();
    let matches = values
        .next()
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| media_type::matches(v, expected));
    matches
        && values.next().is_none()
        && headers
            .get_all(header::CONTENT_ENCODING)
            .iter()
            .all(|v| v == "identity")
}

fn failure(metadata: &Metadata, problem: Problem) -> Error {
    Error::Response(Box::new(Failure {
        metadata: metadata.clone(),
        tenancy_error: None,
        gateway_error: None,
        problem: Some(problem),
    }))
}

fn response_error(metadata: &Metadata, error: SdkError) -> Error {
    if let SdkError::Stream(stream) = error {
        let problem = stream.body_error.map(problem);
        return Error::Response(Box::new(Failure {
            metadata: metadata.clone(),
            tenancy_error: None,
            gateway_error: stream.body,
            problem,
        }));
    }
    failure(metadata, problem(error))
}

fn problem(error: SdkError) -> Problem {
    match error {
        SdkError::Transport(_) => Problem::Transport,
        SdkError::Limit => Problem::Limit,
        SdkError::IncompleteStream => Problem::IncompleteStream,
        _ => Problem::Contract,
    }
}

async fn read_failure(
    response: Response,
    metadata: Metadata,
    metadata_problem: Option<Problem>,
    limit: usize,
) -> Error {
    let mut failure = Failure {
        metadata,
        tenancy_error: None,
        gateway_error: None,
        problem: metadata_problem,
    };
    if (200..300).contains(&failure.metadata.status) {
        failure.problem = failure.problem.or(Some(Problem::Status));
    }
    if !representation(response.headers(), "application/json") {
        failure.problem = failure.problem.or(Some(Problem::ContentType));
        return Error::Response(Box::new(failure));
    }
    match transport::body(response, limit).await {
        Err(error) => failure.problem = failure.problem.or(Some(problem(error))),
        Ok(bytes) => {
            // Decode directly to retain duplicate-field and strict-shape checks.
            if let Ok(body) = serde_json::from_slice::<TenancyError>(&bytes) {
                if body.validate().is_err() {
                    failure.problem = failure.problem.or(Some(Problem::Contract));
                } else {
                    if body.operation_id.is_some()
                        || !error_status(body.code, failure.metadata.status)
                    {
                        failure.problem = failure.problem.or(Some(Problem::Contract));
                    }
                    if failure.metadata.execution_id != body.execution_id {
                        failure.problem = failure.problem.or(Some(Problem::Metadata));
                    }
                    failure.tenancy_error = Some(body);
                }
            } else if let Ok(body) = serde_json::from_slice::<GatewayErrorEnvelope>(&bytes) {
                failure.gateway_error = Some(body);
            } else {
                failure.problem = failure.problem.or(Some(Problem::Contract));
            }
        }
    }
    Error::Response(Box::new(failure))
}

fn error_status(code: ErrorCode, status: u16) -> bool {
    matches!(
        (status, code),
        (400, ErrorCode::InvalidRequest)
            | (401, ErrorCode::Unauthorized)
            | (403, ErrorCode::Forbidden | ErrorCode::TenantInactive)
            | (404, ErrorCode::NotFound)
            | (
                409,
                ErrorCode::RevisionConflict
                    | ErrorCode::OperationConflict
                    | ErrorCode::CapabilityMismatch
            )
            | (410, ErrorCode::OperationExpired)
            | (413, ErrorCode::PayloadTooLarge)
            | (429, ErrorCode::CapacityExceeded)
            | (
                503,
                ErrorCode::AuthorityUnavailable | ErrorCode::StorageUnavailable
            )
    )
}
