//! One bounded token POST, owned by the existing execution future and receipt.
use super::{adapter::model_error, exchange::Exchange, profile};
use crate::gateway::{
    model::error::Result,
    providers::auth::{credentials::OAuthToken, store::OAuthRefresher},
};
use async_trait::async_trait;
use bitrouter_ai::tenancy::operation::ErrorCode;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{sync::Arc, time::Duration};

const RESPONSE_BYTES: usize = 256 * 1024;
const MAX_EXPIRY_SECONDS: u64 = 31 * 24 * 60 * 60;

pub(super) struct Refresher {
    exchange: Arc<Exchange>,
}
impl Refresher {
    pub fn new(exchange: Arc<Exchange>) -> Self {
        Self { exchange }
    }
}
#[async_trait]
impl OAuthRefresher for Refresher {
    async fn refresh(&self, current: &OAuthToken) -> Result<OAuthToken> {
        let exchange = &self.exchange;
        let budget = exchange
            .execution
            .remaining()
            .map_or(Duration::from_secs(30), |remaining| {
                remaining.min(Duration::from_secs(30))
            });
        if budget.is_zero() {
            return Err(model_error(exchange, ErrorCode::CapacityExceeded));
        }
        let operation = async {
            let metadata = &exchange.original.metadata;
            let known = exchange
                .original
                .token(
                    &exchange
                        .execution
                        .oauth_crypto()
                        .map_err(|code| model_error(exchange, code))?,
                )
                .map_err(|_| model_error(exchange, ErrorCode::AuthorityUnavailable))?;
            if &known != current {
                return Err(model_error(exchange, ErrorCode::Forbidden));
            }
            let refresh = current
                .refresh_token
                .as_ref()
                .ok_or_else(|| model_error(exchange, ErrorCode::CapabilityMismatch))?;
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .no_proxy()
                .timeout(budget)
                .build()
                .map_err(|_| model_error(exchange, ErrorCode::AuthorityUnavailable))?;
            let request = client
                .post(&metadata.profile.token_endpoint)
                .form(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh.as_str()),
                    ("client_id", metadata.profile.client_id.as_str()),
                ])
                .build()
                .map_err(|_| model_error(exchange, ErrorCode::InvalidRequest))?;
            exchange
                .execution
                .arm_oauth(exchange, &request)
                .map_err(|code| model_error(exchange, code))?;
            let response = client
                .execute(request)
                .await
                .map_err(|_| model_error(exchange, ErrorCode::StorageUnavailable))?;
            let status = response.status();
            exchange
                .status(status.as_u16())
                .map_err(|code| model_error(exchange, code))?;
            let bytes = bounded_body(response)
                .await
                .map_err(|_| model_error(exchange, ErrorCode::StorageUnavailable))?;
            if !status.is_success() {
                return Err(model_error(exchange, ErrorCode::Forbidden));
            }
            decode(&bytes).map_err(|_| model_error(exchange, ErrorCode::StorageUnavailable))
        };
        tokio::time::timeout(budget, operation)
            .await
            .map_err(|_| model_error(exchange, ErrorCode::StorageUnavailable))?
    }
}
async fn bounded_body(mut response: reqwest::Response) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        response
            .content_length()
            .is_none_or(|length| length <= RESPONSE_BYTES as u64),
        "OAuth response capacity"
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        anyhow::ensure!(
            bytes
                .len()
                .checked_add(chunk.len())
                .is_some_and(|size| size <= RESPONSE_BYTES),
            "OAuth response capacity"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
#[derive(Deserialize)]
struct Reply {
    access_token: String,
    refresh_token: String,
    expires_in: u64,
}
fn decode(bytes: &[u8]) -> anyhow::Result<OAuthToken> {
    anyhow::ensure!(bytes.len() <= RESPONSE_BYTES, "OAuth response capacity");
    let reply: Reply = serde_json::from_slice(bytes)?;
    anyhow::ensure!(
        reply.expires_in > 0 && reply.expires_in <= MAX_EXPIRY_SECONDS,
        "OAuth expiry capacity"
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let token = OAuthToken {
        access_token: reply.access_token,
        refresh_token: Some(reply.refresh_token),
        expires_at: now
            .checked_add(reply.expires_in)
            .ok_or_else(|| anyhow::anyhow!("OAuth expiry overflow"))?,
    };
    profile::validate_token(&token)?;
    Ok(token)
}
pub(in crate::tenancy) fn request_digest(
    request: &reqwest::Request,
) -> std::result::Result<[u8; 32], ErrorCode> {
    let mut hash = Sha256::new();
    hash.update(b"bitrouter.owned-oauth-request.v1");
    let body = request
        .body()
        .and_then(reqwest::Body::as_bytes)
        .ok_or(ErrorCode::InvalidRequest)?;
    for bytes in [
        request.method().as_str().as_bytes(),
        request.url().as_str().as_bytes(),
        body,
    ] {
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
    }
    for (name, value) in request.headers() {
        for bytes in [name.as_str().as_bytes(), value.as_bytes()] {
            hash.update((bytes.len() as u64).to_be_bytes());
            hash.update(bytes);
        }
    }
    Ok(hash.finalize().into())
}

#[cfg(test)]
mod tests;
