//! Single-attempt stream I/O and the host's durable replay boundary.
use std::{future::Future, time::Duration};

use futures_util::{future, pin_mut};
use serde_json::Value;

use crate::error::{Error, Result};

/// A durable event belongs to one original execution. The store is scoped
/// to that execution by its host. Sequence is also the replay cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub sequence: u64,
    pub source: String,
    pub frame: Value,
}

/// Host-supplied persistence. An append acknowledges only committed data.
/// Reads are ordered and exclusive of the supplied cursor.
pub trait EventStore: Send + Sync {
    type Error;

    fn append(
        &self,
        event: Event,
    ) -> impl Future<Output = std::result::Result<(), Self::Error>> + Send;

    fn read_after(
        &self,
        after: Option<u64>,
    ) -> impl Future<Output = std::result::Result<Vec<Event>, Self::Error>> + Send;
}

/// Drives I/O for an original attempt without acquiring dispatch or retry
/// permission. A foreground host supplies its local stop signal. A background
/// host owns the task independently of subscribers.
pub struct Driver {
    idle: Duration,
}

impl Driver {
    pub fn new(idle: Duration) -> Self {
        Self { idle }
    }

    pub async fn contact<T>(
        &self,
        stop: impl Future<Output = ()>,
        io: impl Future<Output = T>,
    ) -> Result<Option<T>> {
        let contact = tokio::time::timeout(self.idle, io);
        pin_mut!(contact, stop);
        match future::select(stop, contact).await {
            future::Either::Left(_) => Ok(None),
            future::Either::Right((result, _)) => result
                .map(Some)
                .map_err(|_| Error::Response("native attempt contact timed out")),
        }
    }
}

/// Direct Anthropic transport. The host can retain the original wire bytes
/// before doing protocol-specific validation.
#[derive(Clone)]
pub struct MessagesTransport {
    http: reqwest::Client,
    endpoint: reqwest::Url,
}

impl MessagesTransport {
    /// Use a provisioned native origin or endpoint and a dedicated API key.
    ///
    /// # Errors
    /// Returns an error for an invalid endpoint, key, or HTTP configuration.
    pub fn new(endpoint: &str, key: &str) -> Result<Self> {
        let mut endpoint = reqwest::Url::parse(endpoint)
            .map_err(|_| Error::Options("invalid native Messages endpoint"))?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || key.trim().is_empty()
        {
            return Err(Error::Options("invalid native Messages endpoint or key"));
        }
        let mut key = reqwest::header::HeaderValue::from_str(key)
            .map_err(|_| Error::Options("invalid Messages API key"))?;
        key.set_sensitive(true);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-api-key", key);
        headers.insert(
            "anthropic-version",
            reqwest::header::HeaderValue::from_static("2023-06-01"),
        );
        let path = endpoint.path().trim_end_matches('/');
        let path = if path.ends_with("/v1/messages") {
            path.to_owned()
        } else {
            format!("{}/v1/messages", path.strip_suffix("/v1").unwrap_or(path))
        };
        endpoint.set_path(&path);
        let http = crate::transport::build_http(
            headers,
            &crate::gateway::TransportOptions::default(),
            false,
        )?;
        Ok(Self { http, endpoint })
    }

    /// Issue one original Messages POST with no retry or redirection.
    ///
    /// # Errors
    /// A transport error may follow accepted execution.
    pub async fn open(&self, body: &Value) -> Result<reqwest::Response> {
        self.http
            .post(self.endpoint.clone())
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(body)
            .send()
            .await
            .map_err(Error::transport)
    }
}

/// Read the top-level background control shared by the three inference APIs.
/// The host decides whether the field is forwarded to the selected seller.
///
/// # Errors
/// Returns an error if the supplied control is not a Boolean.
pub fn background(request: &Value) -> Result<bool> {
    match request.get("background") {
        None => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or(Error::Request("background must be a Boolean")),
    }
}
