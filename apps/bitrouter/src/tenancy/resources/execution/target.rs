//! Exact, credential-free target codec. Every RoutingTarget field is reviewed.

use anyhow::{Result, ensure};
use bitrouter_ai::{
    core::evidence::NativeRouteConstraints,
    types::{ApiProtocol, ChatTokenLimitField, ReasoningEffortConfig},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::super::{
    ExecutionCapacityExceeded, MAX_ARTIFACT_BYTES,
    manifest::{ByteBudget, bounded_bytes},
};
use crate::gateway::{
    model::target::AuthScheme,
    pipeline::types::{OutboundHeaderRule, RoutingTarget},
};

const MAX_HEADERS: usize = 4096;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    provider: String,
    model: String,
    api_base: String,
    protocol: ApiProtocol,
    account: Option<String>,
    auth_scheme: AuthScheme,
    chat_token_limit_field: Option<ChatTokenLimitField>,
    chat_supports_store: Option<bool>,
    chat_supports_stream_options: Option<bool>,
    chat_google_extensions: bool,
    reasoning_effort: Option<ReasoningEffortConfig>,
    model_constraints: NativeRouteConstraints,
    headers: Vec<Header>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    name: String,
    default: Option<String>,
    passthrough: bool,
}

#[derive(Serialize)]
struct Projection<'a> {
    provider: &'a str,
    model: &'a str,
    api_base: &'a str,
    protocol: &'a ApiProtocol,
    account: &'a Option<String>,
    auth_scheme: &'a AuthScheme,
    chat_token_limit_field: &'a Option<ChatTokenLimitField>,
    chat_supports_store: &'a Option<bool>,
    chat_supports_stream_options: &'a Option<bool>,
    chat_google_extensions: &'a bool,
    reasoning_effort: &'a Option<ReasoningEffortConfig>,
    model_constraints: &'a NativeRouteConstraints,
    headers: Vec<HeaderProjection<'a>>,
}

#[derive(Serialize)]
struct HeaderProjection<'a> {
    name: &'a str,
    default: Option<&'a str>,
    passthrough: bool,
}

/// Bounded exact bytes, not a hash or a normalized endpoint/account identity.
pub(super) fn project(target: &RoutingTarget) -> Result<Vec<u8>> {
    // Deliberately exhaustive: adding any behavioral field must break this
    // projection at compile time rather than silently broadening membership.
    let RoutingTarget {
        provider_name,
        service_id,
        api_base,
        api_key,
        api_protocol,
        chat_token_limit_field,
        chat_supports_store,
        chat_supports_stream_options,
        chat_google_extensions,
        reasoning_effort,
        model_constraints,
        account_label,
        api_key_override,
        api_base_override,
        auth_scheme,
        headers,
    } = target;
    ensure!(
        api_key.is_empty() && api_key_override.is_none() && api_base_override.is_none(),
        "retained target carries mutable credentials or endpoint overrides"
    );
    ensure!(headers.len() <= MAX_HEADERS, ExecutionCapacityExceeded);
    let headers = headers
        .iter()
        .map(|header| {
            Ok(HeaderProjection {
                name: header.name().as_str(),
                default: header
                    .default()
                    .map(|value| value.to_str())
                    .transpose()
                    .map_err(|_| anyhow::anyhow!("invalid retained header"))?,
                passthrough: header.passthrough(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    bounded_bytes(
        &Projection {
            provider: provider_name,
            model: service_id,
            api_base,
            protocol: api_protocol,
            account: account_label,
            auth_scheme,
            chat_token_limit_field,
            chat_supports_store,
            chat_supports_stream_options,
            chat_google_extensions,
            reasoning_effort,
            model_constraints,
            headers,
        },
        MAX_ARTIFACT_BYTES,
    )
    .map_err(|_| ExecutionCapacityExceeded.into())
}

pub(super) fn decode(value: Value) -> Result<(RoutingTarget, Vec<u8>)> {
    ByteBudget::new(MAX_ARTIFACT_BYTES)
        .charge(&value)
        .map_err(|_| ExecutionCapacityExceeded)?;
    let decoded: Target = serde_json::from_value(value.clone())
        .map_err(|_| anyhow::anyhow!("invalid retained target fields"))?;
    // This also rejects missing optional fields and ignored nested fields, rather
    // than turning corrupt retained input into a different normalized target.
    ensure!(
        serde_json::to_value(&decoded)? == value,
        "non-exact retained target encoding"
    );
    ensure!(
        !decoded.provider.is_empty()
            && !decoded.model.is_empty()
            && decoded
                .account
                .as_ref()
                .is_none_or(|account| !account.is_empty())
            && !matches!(decoded.protocol, ApiProtocol::Custom(_)),
        "invalid retained target identity"
    );
    crate::config::url_validator::validate_upstream_url(&decoded.api_base)
        .map_err(|_| anyhow::anyhow!("invalid retained target URL"))?;
    if let Some(effort) = &decoded.reasoning_effort {
        effort
            .validate()
            .map_err(|_| anyhow::anyhow!("invalid retained reasoning support"))?;
    }
    ensure!(
        decoded.headers.len() <= MAX_HEADERS,
        ExecutionCapacityExceeded
    );
    let mut names = std::collections::HashSet::new();
    let headers = decoded
        .headers
        .into_iter()
        .map(|header| {
            ensure!(
                !header.passthrough && names.insert(header.name.clone()),
                "unqualified retained header"
            );
            OutboundHeaderRule::new(&header.name, header.default.as_deref(), false)
                .map_err(|_| anyhow::anyhow!("invalid retained header"))
        })
        .collect::<Result<Vec<_>>>()?;
    let target = RoutingTarget {
        provider_name: decoded.provider,
        service_id: decoded.model,
        api_base: decoded.api_base,
        api_key: String::new(),
        api_protocol: decoded.protocol,
        chat_token_limit_field: decoded.chat_token_limit_field,
        chat_supports_store: decoded.chat_supports_store,
        chat_supports_stream_options: decoded.chat_supports_stream_options,
        chat_google_extensions: decoded.chat_google_extensions,
        reasoning_effort: decoded.reasoning_effort,
        model_constraints: decoded.model_constraints,
        account_label: decoded.account,
        api_key_override: None,
        api_base_override: None,
        auth_scheme: decoded.auth_scheme,
        headers,
    };
    let projection = project(&target)?;
    ensure!(
        serde_json::from_slice::<Value>(&projection)? == value,
        "retained target changed during materialization"
    );
    Ok((target, projection))
}
