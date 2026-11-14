//! Whole native responses remain independent of restricted canonical conversion.
//! <https://ai.google.dev/api/generate-content#GenerateContentResponse>

use crate::gateway::{
    model::error::{ModelError, Result},
    pipeline::types::generate_content::{Input, Response},
};
use serde_json::Value;

pub(crate) mod framing;
mod json;
mod lifecycle;
pub(crate) mod stream;
#[cfg(test)]
mod tests;

pub(crate) fn qualify_json(input: &Input, body: Value) -> Result<Response> {
    let usage = body.get("usageMetadata").and_then(super::parse_usage);
    let mut lifecycle =
        lifecycle::Lifecycle::new(bitrouter_ai::tenancy::generate_content::MAX_REQUEST_BYTES);
    lifecycle
        .observe(&body)
        .and_then(|_| lifecycle.complete().map(|_| ()))
        .map_err(|_| ModelError::InvalidResponse {
            message: "invalid native response lifecycle".into(),
            usage: usage.clone().map(Box::new),
        })?;
    Ok(Response::new(input, body, usage))
}

pub(crate) fn value(text: &str) -> Result<Value> {
    if text.len() > bitrouter_ai::tenancy::generate_content::MAX_REQUEST_BYTES {
        return Err(ModelError::InvalidResponse {
            message: "native response limit".into(),
            usage: None,
        });
    }
    json::decode(text).map_err(|_| ModelError::InvalidResponse {
        message: "invalid native response JSON".into(),
        usage: None,
    })
}
