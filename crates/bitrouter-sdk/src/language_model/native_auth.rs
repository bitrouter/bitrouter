//! Account selector policy at the routing boundary.

use super::types::RoutingTarget;
use crate::error::Result;
use bitrouter_ai::auth::{AuthAppliers, ContinuationAuthority};

pub(crate) fn is_continuation_scope_header(name: &str) -> bool {
    bitrouter_ai::auth::is_continuation_scope_header(name)
}

/// Resolve configured account selectors using exactly the same precedence as
/// the outbound request. Missing inbound context is unknown for passthrough.
pub(crate) fn request_scope_headers(
    target: &RoutingTarget,
    inbound: Option<&http::HeaderMap>,
) -> Option<http::HeaderMap> {
    let mut headers = http::HeaderMap::new();
    for rule in &target.headers {
        if !is_continuation_scope_header(rule.name().as_str()) {
            continue;
        }
        headers.remove(rule.name());
        if rule.passthrough() {
            let values = inbound?.get_all(rule.name());
            if values.iter().next().is_some() {
                for value in values {
                    headers.append(rule.name().clone(), value.clone());
                }
                continue;
            }
        }
        if let Some(value) = rule.default() {
            headers.insert(rule.name().clone(), value.clone());
        }
    }
    Some(headers)
}

/// Resolve a continuation proof using the exact configured account selectors.
/// Passthrough values require an explicit inbound request context.
pub async fn continuation_authority_for_request(
    auth: &AuthAppliers,
    target: &RoutingTarget,
    inbound: Option<&http::HeaderMap>,
) -> Result<Option<ContinuationAuthority>> {
    if target.provider_name == "anthropic"
        && target
            .headers
            .iter()
            .any(|rule| rule.name().as_str() == "anthropic-workspace-id")
    {
        return Ok(None);
    }
    let Some(scope) = request_scope_headers(target, inbound) else {
        return Ok(None);
    };
    Ok(auth
        .continuation_authority_proof(&target.model_target())
        .await?
        .map(|authority| authority.with_request_scope(&scope)))
}
