use crate::models::shared::types::JsonValue;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, BitrouterError>;

#[derive(Debug, Clone, Error)]
pub enum BitrouterError {
    #[error("{provider} does not support {feature}")]
    UnsupportedFeature {
        provider: String,
        feature: String,
        details: Option<String>,
    },
    #[error("request cancelled: {message}")]
    Cancelled {
        provider: Option<String>,
        message: String,
    },
    #[error("invalid request: {message}")]
    InvalidRequest {
        provider: Option<String>,
        message: String,
        body: Option<JsonValue>,
    },
    #[error("transport failure: {message}")]
    Transport {
        provider: Option<String>,
        message: String,
    },
    #[error("response decode failure: {message}")]
    ResponseDecode {
        provider: Option<String>,
        message: String,
        body: Option<JsonValue>,
    },
    #[error("invalid response: {message}")]
    InvalidResponse {
        provider: Option<String>,
        message: String,
        body: Option<JsonValue>,
    },
    #[error("provider error: {message}")]
    Provider {
        provider: String,
        status_code: Option<u16>,
        error_type: Option<String>,
        code: Option<String>,
        param: Option<String>,
        message: String,
        request_id: Option<String>,
        body: Option<JsonValue>,
    },
    #[error("stream protocol failure: {message}")]
    StreamProtocol {
        provider: Option<String>,
        message: String,
        chunk: Option<JsonValue>,
    },
}

impl BitrouterError {
    pub fn cancelled(provider: Option<&str>, message: impl Into<String>) -> Self {
        Self::Cancelled {
            provider: provider.map(str::to_owned),
            message: message.into(),
        }
    }

    pub fn unsupported(
        provider: impl Into<String>,
        feature: impl Into<String>,
        details: Option<String>,
    ) -> Self {
        Self::UnsupportedFeature {
            provider: provider.into(),
            feature: feature.into(),
            details,
        }
    }

    pub fn invalid_request(
        provider: Option<&str>,
        message: impl Into<String>,
        body: Option<JsonValue>,
    ) -> Self {
        Self::InvalidRequest {
            provider: provider.map(str::to_owned),
            message: message.into(),
            body,
        }
    }

    pub fn transport(provider: Option<&str>, message: impl Into<String>) -> Self {
        Self::Transport {
            provider: provider.map(str::to_owned),
            message: message.into(),
        }
    }

    pub fn response_decode(
        provider: Option<&str>,
        message: impl Into<String>,
        body: Option<JsonValue>,
    ) -> Self {
        Self::ResponseDecode {
            provider: provider.map(str::to_owned),
            message: message.into(),
            body,
        }
    }

    pub fn invalid_response(
        provider: Option<&str>,
        message: impl Into<String>,
        body: Option<JsonValue>,
    ) -> Self {
        Self::InvalidResponse {
            provider: provider.map(str::to_owned),
            message: message.into(),
            body,
        }
    }

    pub fn provider_error(
        provider: impl Into<String>,
        status_code: Option<u16>,
        error_type: Option<String>,
        code: Option<String>,
        param: Option<String>,
        message: impl Into<String>,
        request_id: Option<String>,
        body: Option<JsonValue>,
    ) -> Self {
        Self::Provider {
            provider: provider.into(),
            status_code,
            error_type,
            code,
            param,
            message: message.into(),
            request_id,
            body,
        }
    }

    pub fn stream_protocol(
        provider: Option<&str>,
        message: impl Into<String>,
        chunk: Option<JsonValue>,
    ) -> Self {
        Self::StreamProtocol {
            provider: provider.map(str::to_owned),
            message: message.into(),
            chunk,
        }
    }
}
