//! Explicit HTTP access to the opt-in Thread execution service.
//!
//! The execution bearer and endpoint are distinct from gateway and control
//! credentials. Connecting binds this client to one discovered server instance;
//! a restart requires an explicit new connection, never an automatic replay.

mod observation;

use bitrouter_ai::harness::api::{self, PROTOCOL_VERSION};
use bitrouter_ai::harness::thread::{
    ThreadHistoryPage, ThreadObservation, ThreadSnapshot, ThreadView,
};
use bitrouter_ai::harness::turn::{SteeringReceipt, TurnReceipt, TurnSnapshot};
use reqwest::{RequestBuilder, header};
use serde::{Serialize, de::DeserializeOwned};
use url::Url;

use crate::error::{Error, Result};
use crate::gateway::TransportOptions;
use crate::stream::EventStream;
use crate::transport;

const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// A client bound to one execution service instance.
///
/// No method starts a daemon, accesses its workspace or persists a session.
/// Mutations require a caller-owned idempotency key. A failed transport can
/// leave acceptance unknown; inspect the service rather than blindly retrying.
#[derive(Clone)]
pub struct HarnessClient {
    endpoint: Url,
    http: reqwest::Client,
    options: TransportOptions,
    capabilities: api::CapabilitiesResponse,
    instance: header::HeaderValue,
}

impl std::fmt::Debug for HarnessClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HarnessClient")
            .finish_non_exhaustive()
    }
}

impl HarnessClient {
    /// Discover the existing v2 surface using an explicit execution credential.
    pub async fn connect(endpoint: &str, credential: &str) -> Result<Self> {
        Self::connect_with_options(endpoint, credential, TransportOptions::default()).await
    }

    /// Use the same bounded HTTP transport options as the gateway client.
    /// An origin or an endpoint ending in `/agent/v2` is accepted; a prefix is
    /// preserved. This does not change the server's loopback-only listen policy.
    pub async fn connect_with_options(
        endpoint: &str,
        credential: &str,
        options: TransportOptions,
    ) -> Result<Self> {
        let endpoint = endpoint_url(endpoint)?;
        if credential.trim().is_empty() {
            return Err(Error::Options("execution credential must not be empty"));
        }
        if options.max_response_bytes == 0
            || options.max_event_bytes == 0
            || options.connect_timeout.is_zero()
            || options
                .read_timeout
                .is_some_and(|timeout| timeout.is_zero())
            || options
                .request_timeout
                .is_some_and(|timeout| timeout.is_zero())
        {
            return Err(Error::Options(
                "harness timeouts and byte limits must be positive",
            ));
        }
        let mut bearer = header::HeaderValue::from_str(&format!("Bearer {credential}"))
            .map_err(|_| Error::Options("execution credential is not a valid HTTP header"))?;
        bearer.set_sensitive(true);
        let mut headers = header::HeaderMap::new();
        headers.insert(header::AUTHORIZATION, bearer);
        let http = transport::build_http(headers, &options, false)?;
        let response = http
            .get(
                endpoint
                    .join("capabilities")
                    .map_err(|_| Error::Options("invalid execution endpoint"))?,
            )
            .send()
            .await
            .map_err(Error::transport)?;
        let capabilities: api::CapabilitiesResponse =
            json(response, options.max_response_bytes).await?;
        version(capabilities.version)?;
        if capabilities.runtime.server_instance_id.is_empty() {
            return Err(Error::Response("missing harness instance identity"));
        }
        let instance = header::HeaderValue::from_str(&capabilities.runtime.server_instance_id)
            .map_err(|_| Error::Response("invalid harness instance identity"))?;
        Ok(Self {
            endpoint,
            http,
            options,
            capabilities,
            instance,
        })
    }

    /// The report used to bind this client. It is never refreshed implicitly.
    pub fn capabilities(&self) -> &api::CapabilitiesResponse {
        &self.capabilities
    }

    pub async fn create_thread(
        &self,
        idempotency_key: &str,
        input: &api::CreateThreadRequest,
    ) -> Result<ThreadSnapshot> {
        let response: api::ThreadResponse = self
            .send(self.mutation(&["threads"], idempotency_key, Some(input))?)
            .await?;
        version(response.version)?;
        self.check_instance(&response.thread.server_instance_id)?;
        if response.thread.thread_id.is_empty() {
            return Err(Error::Response("missing Thread identity"));
        }
        Ok(response.thread)
    }

    pub async fn read_thread(&self, thread_id: &str) -> Result<ThreadView> {
        let response: api::ThreadViewResponse =
            self.send(self.get(&["threads", thread_id])?).await?;
        version(response.version)?;
        observation::check_view(&response.view, thread_id, self.instance_id())?;
        Ok(response.view)
    }

    /// Submit one start or enqueue operation. The server owns queue admission.
    pub async fn submit_turn(
        &self,
        thread_id: &str,
        idempotency_key: &str,
        input: &api::TurnRequest,
    ) -> Result<TurnReceipt> {
        self.receipt(
            self.mutation(
                &["threads", thread_id, "turns"],
                idempotency_key,
                Some(input),
            )?,
            thread_id,
            None,
        )
        .await
    }

    pub async fn read_turn(&self, thread_id: &str, turn_id: &str) -> Result<TurnSnapshot> {
        let response: api::TurnResponse = self
            .send(self.get(&["threads", thread_id, "turns", turn_id])?)
            .await?;
        version(response.version)?;
        observation::check_turn(&response.turn, thread_id, self.instance_id())?;
        if response.turn.turn_id != turn_id {
            return Err(Error::Response("harness response belongs to another Turn"));
        }
        Ok(response.turn)
    }

    pub async fn cancel_turn(
        &self,
        thread_id: &str,
        turn_id: &str,
        idempotency_key: &str,
        input: &api::CancelRequest,
    ) -> Result<TurnReceipt> {
        self.receipt(
            self.mutation(
                &["threads", thread_id, "turns", turn_id, "cancel"],
                idempotency_key,
                Some(input),
            )?,
            thread_id,
            Some(turn_id),
        )
        .await
    }

    pub async fn steer(
        &self,
        thread_id: &str,
        idempotency_key: &str,
        input: &api::SteeringRequest,
    ) -> Result<SteeringReceipt> {
        let response: api::SteeringReceiptResponse = self
            .send(self.mutation(
                &["threads", thread_id, "steer"],
                idempotency_key,
                Some(input),
            )?)
            .await?;
        version(response.version)?;
        if response.receipt.turn_id != input.expected_turn_id {
            return Err(Error::Response("harness response belongs to another Turn"));
        }
        Ok(response.receipt)
    }

    /// Answer exactly the input chosen by the caller; observation never approves.
    pub async fn answer_input(
        &self,
        thread_id: &str,
        idempotency_key: &str,
        input: &api::InputRequest,
    ) -> Result<TurnReceipt> {
        self.receipt(
            self.mutation(
                &["threads", thread_id, "inputs"],
                idempotency_key,
                Some(input),
            )?,
            thread_id,
            Some(&input.turn_id),
        )
        .await
    }

    pub async fn resume_queue(
        &self,
        thread_id: &str,
        idempotency_key: &str,
    ) -> Result<ThreadSnapshot> {
        let response: api::ThreadResponse = self
            .send(self.mutation::<()>(&["threads", thread_id, "resume"], idempotency_key, None)?)
            .await?;
        version(response.version)?;
        self.check_instance(&response.thread.server_instance_id)?;
        observation::check_thread(&response.thread.thread_id, thread_id)?;
        Ok(response.thread)
    }

    /// Read one durable page. Callers retain its cutoff and next cursor explicitly.
    pub async fn history(
        &self,
        thread_id: &str,
        query: &api::HistoryQuery,
    ) -> Result<ThreadHistoryPage> {
        let response: api::HistoryResponse = self
            .send(self.get(&["threads", thread_id, "history"])?.query(query))
            .await?;
        version(response.version)?;
        self.check_instance(&response.history.server_instance_id)?;
        observation::check_thread(&response.history.thread_id, thread_id)?;
        for event in &response.history.events {
            // Committed events retain their original execution epoch after a restart.
            observation::check_thread(&event.thread_id, thread_id)?;
        }
        Ok(response.history)
    }

    /// Attach to a continuing observation stream. Clean EOF means detached,
    /// including after a completed Turn; it says nothing about current execution.
    /// No reconnect, cursor advancement, cancellation or approval is automatic.
    pub async fn observe(
        &self,
        thread_id: &str,
        query: &api::ObserveQuery,
    ) -> Result<EventStream<ThreadObservation>> {
        let response = self
            .get(&["threads", thread_id, "observe"])?
            .query(query)
            .header(header::ACCEPT, "text/event-stream")
            .send()
            .await
            .map_err(Error::transport)?;
        let response =
            transport::successful_as(response, self.options.max_response_bytes, Error::Harness)
                .await?;
        if !transport::has_content_type(&response, "text/event-stream") {
            return Err(Error::Response("expected text/event-stream"));
        }
        Ok(observation::decode(
            response,
            self.options.max_event_bytes,
            thread_id.to_owned(),
            self.instance_id().to_owned(),
        ))
    }

    async fn receipt(
        &self,
        request: RequestBuilder,
        thread_id: &str,
        turn_id: Option<&str>,
    ) -> Result<TurnReceipt> {
        let response: api::TurnReceiptResponse = self.send(request).await?;
        version(response.version)?;
        observation::check_thread(&response.receipt.thread_id, thread_id)?;
        if turn_id.is_some_and(|id| response.receipt.turn_id != id) {
            return Err(Error::Response("harness response belongs to another Turn"));
        }
        Ok(response.receipt)
    }

    fn instance_id(&self) -> &str {
        &self.capabilities.runtime.server_instance_id
    }

    fn check_instance(&self, actual: &str) -> Result<()> {
        observation::check_instance(actual, self.instance_id())
    }

    fn get(&self, segments: &[&str]) -> Result<RequestBuilder> {
        Ok(self
            .http
            .get(self.url(segments)?)
            .header("x-bro-server-instance", self.instance.clone()))
    }

    fn mutation<T: Serialize>(
        &self,
        segments: &[&str],
        key: &str,
        input: Option<&T>,
    ) -> Result<RequestBuilder> {
        if key.is_empty() || key.len() > 128 || !key.is_ascii() {
            return Err(Error::Request(
                "Idempotency-Key must contain 1 to 128 ASCII bytes",
            ));
        }
        // HTTP field parsing removes surrounding optional whitespace. Reject
        // it rather than submitting a different acceptance identity.
        // https://www.rfc-editor.org/rfc/rfc9112.html#section-5.1
        if key.starts_with([' ', '\t']) || key.ends_with([' ', '\t']) {
            return Err(Error::Request(
                "Idempotency-Key must not start or end with HTTP whitespace",
            ));
        }
        let key = header::HeaderValue::from_str(key)
            .map_err(|_| Error::Request("invalid Idempotency-Key header"))?;
        let request = self
            .http
            .post(self.url(segments)?)
            .header("x-bro-server-instance", self.instance.clone())
            .header("idempotency-key", key);
        match input {
            Some(input) => Ok(request
                .header(header::CONTENT_TYPE, "application/json")
                .body(request_body(input)?)),
            None => Ok(request),
        }
    }

    async fn send<T: DeserializeOwned>(&self, request: RequestBuilder) -> Result<T> {
        json(
            request.send().await.map_err(Error::transport)?,
            self.options.max_response_bytes,
        )
        .await
    }

    fn url(&self, segments: &[&str]) -> Result<Url> {
        if segments
            .iter()
            .any(|segment| segment.is_empty() || matches!(*segment, "." | ".."))
        {
            return Err(Error::Request("invalid harness resource path segment"));
        }
        let mut url = self.endpoint.clone();
        url.path_segments_mut()
            .map_err(|_| Error::Options("invalid execution endpoint"))?
            .pop_if_empty()
            .extend(segments);
        Ok(url)
    }
}

async fn json<T: DeserializeOwned>(response: reqwest::Response, limit: usize) -> Result<T> {
    let response = transport::successful_as(response, limit, Error::Harness).await?;
    if !transport::has_content_type(&response, "application/json") {
        return Err(Error::Response("expected application/json"));
    }
    serde_json::from_slice(&transport::body(response, limit).await?)
        .map_err(|_| Error::Response("body does not match the harness JSON contract"))
}

fn version(actual: u32) -> Result<()> {
    if actual != PROTOCOL_VERSION {
        return Err(Error::Response("unsupported harness protocol version"));
    }
    Ok(())
}

fn endpoint_url(endpoint: &str) -> Result<Url> {
    let mut url =
        Url::parse(endpoint).map_err(|_| Error::Options("invalid execution endpoint URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return Err(Error::Options(
            "execution endpoint must be an absolute HTTP(S) URL",
        ));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::Options(
            "execution endpoint must not contain credentials, a query or a fragment",
        ));
    }
    let path = url.path().trim_end_matches('/');
    let path = path.strip_suffix("/agent/v2").unwrap_or(path);
    url.set_path(&format!("{path}/agent/v2/"));
    Ok(url)
}

fn request_body(input: &impl Serialize) -> Result<Vec<u8>> {
    struct BoundedBody(Vec<u8>);
    impl std::io::Write for BoundedBody {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.0.len().saturating_add(bytes.len()) > MAX_REQUEST_BYTES {
                return Err(std::io::Error::other("harness request exceeds 64 KiB"));
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut body = BoundedBody(Vec::new());
    serde_json::to_writer(&mut body, input)
        .map_err(|_| Error::Request("harness request is invalid or exceeds 64 KiB"))?;
    Ok(body.0)
}
