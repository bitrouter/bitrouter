//! Local credential discovery and CLI error translation for the HTTP SDK.
//!
//! Transport, capability caching and reload recovery belong to the SDK. This
//! adapter retains application credential selection and diagnostic categories.

use crate::gateway::error::BitrouterError;
use anyhow::Result;
use bitrouter_ai::control::action::Action;
use bitrouter_ai::control::administration::{
    AgentsReport, ObserveReport, PolicyInput, PolicyReport, ProvidersReport,
};
use bitrouter_ai::control::models::ModelsReport;
use bitrouter_ai::control::reload::{OperationReport, ReloadInput, ReloadState};
use bitrouter_ai::control::requests::{RequestFilters, RequestsReport};
use bitrouter_ai::control::route::{RouteInput, RouteReport};
use bitrouter_ai::control::status::StatusReport;
use bitrouter_ai::control::{CapabilitiesReport, MIN_TOKEN_BYTES, StateReport};
use bitrouter_sdk::control::{ControlClient, ControlOptions};
use bitrouter_sdk::error::Error as ClientError;

pub struct HttpControlClient {
    endpoint: String,
    client: ControlClient,
}

impl HttpControlClient {
    pub fn new(endpoint: &str, token_env: &str) -> Result<Self> {
        let token = std::env::var(token_env).map_err(|_| {
            BitrouterError::Unauthorized(format!(
                "remote control token environment variable {token_env} is not set"
            ))
        })?;
        Self::from_token(endpoint, token, token_env)
    }

    pub(crate) fn from_token(endpoint: &str, token: String, token_source: &str) -> Result<Self> {
        let endpoint = normalize_endpoint(endpoint).map_err(BitrouterError::bad_request)?;
        if token.len() < MIN_TOKEN_BYTES {
            return Err(BitrouterError::Unauthorized(format!(
                "remote control token from {token_source} must contain at least {MIN_TOKEN_BYTES} bytes"
            )).into());
        }
        // Preserve the product client's existing environment-proxy behavior;
        // standalone SDK clients opt into this explicitly instead.
        let options = ControlOptions {
            system_proxy: true,
            ..ControlOptions::default()
        };
        let client = ControlClient::with_options(&endpoint, &token, options)
            .map_err(|error| map_error(error, &endpoint))?;
        Ok(Self { endpoint, client })
    }

    pub async fn capabilities(&self) -> Result<CapabilitiesReport> {
        self.client
            .capabilities()
            .await
            .map_err(|error| map_error(error, &self.endpoint))
    }

    pub async fn state_report(&self) -> Result<StateReport> {
        self.client
            .state_report()
            .await
            .map_err(|error| map_error(error, &self.endpoint))
    }

    pub async fn state(&self) -> Result<ReloadState> {
        self.client
            .state()
            .await
            .map_err(|error| map_error(error, &self.endpoint))
    }

    pub async fn submit_reload(&self, input: &ReloadInput) -> Result<OperationReport> {
        self.client
            .submit_reload(input)
            .await
            .map_err(|error| map_error(error, &self.endpoint))
    }

    pub async fn operation(&self, request_id: &str, instance: &str) -> Result<OperationReport> {
        self.client
            .operation(request_id, instance)
            .await
            .map_err(|error| map_error(error, &self.endpoint))
    }

    pub async fn reload(&self) -> Result<OperationReport> {
        self.client
            .reload()
            .await
            .map_err(|error| map_error(error, &self.endpoint))
    }

    pub async fn providers(&self) -> Result<ProvidersReport> {
        self.client
            .providers()
            .await
            .map_err(|error| map_error(error, &self.endpoint))
    }

    pub async fn agents(&self) -> Result<AgentsReport> {
        self.client
            .agents()
            .await
            .map_err(|error| map_error(error, &self.endpoint))
    }

    pub async fn observe(&self) -> Result<ObserveReport> {
        self.client
            .observe()
            .await
            .map_err(|error| map_error(error, &self.endpoint))
    }

    pub async fn policy(&self, input: &PolicyInput) -> Result<PolicyReport> {
        self.client
            .policy(input)
            .await
            .map_err(|error| map_error(error, &self.endpoint))
    }

    pub async fn status(&self) -> Result<StatusReport> {
        self.client
            .status()
            .await
            .map_err(|error| map_error(error, &self.endpoint))
    }

    pub async fn models(&self, provider: Option<&str>) -> Result<ModelsReport> {
        self.client
            .models(provider)
            .await
            .map_err(|error| map_error(error, &self.endpoint))
    }

    pub async fn route(&self, input: &RouteInput) -> Result<RouteReport> {
        self.client
            .route(input)
            .await
            .map_err(|error| map_error(error, &self.endpoint))
    }

    pub async fn requests(&self, limit: Option<u64>) -> Result<RequestsReport> {
        self.client
            .requests(limit)
            .await
            .map_err(|error| map_error(error, &self.endpoint))
    }

    pub async fn requests_filtered(&self, filters: &RequestFilters) -> Result<RequestsReport> {
        self.client
            .requests_filtered(filters)
            .await
            .map_err(|error| map_error(error, &self.endpoint))
    }

    pub async fn can_action(&self, action: &str) -> Result<bool> {
        let action = Action::from_id(action)
            .ok_or_else(|| BitrouterError::bad_request("unknown control action"))?;
        self.client
            .can_action(action)
            .await
            .map_err(|error| map_error(error, &self.endpoint))
    }
}

pub fn normalize_endpoint(endpoint: &str) -> Result<String> {
    bitrouter_sdk::control::normalize_endpoint(endpoint).map_err(anyhow::Error::new)
}

fn invalid_response(message: impl Into<String>) -> anyhow::Error {
    BitrouterError::UpstreamInvalidResponse {
        message: message.into(),
        usage: None,
    }
    .into()
}

fn map_error(error: ClientError, endpoint: &str) -> anyhow::Error {
    let message = error.to_string();
    match error {
        ClientError::ReloadUnknown { source, .. } => map_error(*source, endpoint).context(message),
        ClientError::Transport(error) if error.is_body() || error.is_decode() => {
            invalid_response(format!("read remote control response: {error}"))
        }
        ClientError::Transport(error) => {
            let classified = if error.is_timeout() {
                BitrouterError::UpstreamTimeout
            } else {
                BitrouterError::UpstreamUnavailable
            };
            anyhow::Error::new(classified)
                .context(format!("remote control endpoint {endpoint} is unreachable"))
        }
        ClientError::Control(failure) => {
            if let Some(body) = failure.body {
                let error = body.error;
                match error.code.as_str() {
                    "unauthorized" => BitrouterError::Unauthorized(error.message).into(),
                    "bad_request" | "method_not_allowed" => {
                        BitrouterError::bad_request(error.message).into()
                    }
                    "not_found" => BitrouterError::NotFound(error.message).into(),
                    _ => BitrouterError::Upstream {
                        status: failure.status,
                        message: error.message,
                    }
                    .into(),
                }
            } else {
                match failure.body_error {
                    Some(ClientError::Limit) => {
                        invalid_response("remote control response exceeded the 8 MiB client limit")
                    }
                    Some(ClientError::Transport(error)) => {
                        invalid_response(format!("read remote control response: {error}"))
                    }
                    _ => BitrouterError::Upstream {
                        status: failure.status,
                        message: "remote control returned an invalid error response".into(),
                    }
                    .into(),
                }
            }
        }
        ClientError::Options(message) | ClientError::Request(message) => {
            BitrouterError::bad_request(message).into()
        }
        ClientError::Limit => {
            invalid_response("remote control response exceeded the 8 MiB client limit")
        }
        ClientError::ControlResourceUnavailable { .. } => anyhow::anyhow!(message),
        _ => invalid_response(message),
    }
}
