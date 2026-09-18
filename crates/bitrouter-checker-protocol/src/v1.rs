//! Request-check HTTP contract version 1.

use std::fmt;
use std::io::Write;

use serde::{Deserialize, Serialize};

/// Wire-contract version implemented by this module.
pub const CONTRACT_VERSION: u16 = 1;
/// Absolute bound for an encoded request envelope.
pub const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
/// Absolute bound for an encoded response envelope.
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024;
/// Maximum projected content fragments accepted by version 1.
pub const MAX_CONTENT_FRAGMENTS: usize = 4096;
/// Maximum bytes accepted for one request or binding identifier.
pub const MAX_IDENTIFIER_BYTES: usize = 256;
/// Maximum projected text bytes a version 1 binding may admit.
pub const MAX_INPUT_BYTES: u64 = 4 * 1024 * 1024;
/// Maximum total invocation deadline declared by a version 1 binding.
pub const MAX_TIMEOUT_MS: u64 = 30_000;

const MAX_IMPLEMENTATION_VERSION_BYTES: usize = 128;
const MAX_REASON_CODE_BYTES: usize = 64;

/// A version 1 request-check invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Wire-contract version. Version 1 decoders require this to be `1`.
    pub contract_version: u16,
    /// Unique invocation identity that the response must echo.
    pub invocation_id: String,
    /// Gateway request identity visible to the caller.
    pub request_id: String,
    /// Canonical named-router id.
    pub router_id: String,
    /// Redaction-safe digest of the effective router binding.
    pub router_binding_digest: String,
    /// Checker binding frozen with the router.
    pub checker: CheckerBinding,
    /// Ordered effective entry-request text fragments.
    pub content: Vec<ContentFragment>,
    /// Evidence describing the projected coverage.
    pub coverage: Coverage,
}

/// Non-secret limits and identity for the selected checker binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckerBinding {
    /// Checker name resolved by the host runtime.
    pub checker_id: String,
    /// Redaction-safe digest of the effective checker binding.
    pub binding_digest: String,
    /// Maximum serialized text bytes admitted by this binding.
    pub max_input_bytes: u64,
    /// Total invocation deadline enforced by the host runtime.
    pub timeout_ms: u64,
}

/// Role attached to a canonical input fragment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentRole {
    /// Out-of-band system instructions.
    System,
    /// End-user content.
    User,
    /// Prior assistant content.
    Assistant,
    /// Tool results or approval responses.
    Tool,
}

/// Canonical kind of one input fragment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentFragmentKind {
    /// Plain message or system text.
    Text,
    /// Prior assistant reasoning text.
    Reasoning,
    /// Tool-call arguments.
    ToolCall,
    /// A tool result.
    ToolResult,
    /// A tool-approval decision.
    ToolApproval,
}

/// One ordered fragment sent to the request-check service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContentFragment {
    /// Conversation role of this fragment.
    pub role: ContentRole,
    /// Canonical content kind.
    pub kind: ContentFragmentKind,
    /// Textual content, when this kind has a text representation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Coverage scope for contract version 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageScope {
    /// Textual fragments on the entry request only.
    EntryRequestText,
}

/// Whether the projected input was complete within its declared scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageStatus {
    /// Every entry-request text fragment fit within the host bounds.
    CompleteWithinScope,
    /// Entry-request text exceeded a host byte or fragment bound.
    InputTooLarge,
}

/// Typed evidence for what an invocation covered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    /// Fixed version 1 coverage scope.
    pub scope: CoverageScope,
    /// Total text bytes projected for the checker.
    pub text_bytes: u64,
    /// Number of projected text fragments.
    pub text_fragments: u64,
    /// Entry-request media fragments intentionally excluded from the payload.
    pub excluded_media_fragments: u64,
    /// Completeness within the fixed scope.
    pub status: CoverageStatus,
}

/// Decision carried by a validated response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The request may proceed.
    Allow,
    /// The request must stop before model selection and provider dispatch.
    Deny,
}

/// A version 1 response envelope.
///
/// Construct responses with [`Response::allow`] or [`Response::deny`] so the
/// same bounds enforced by host decoding are also enforced by services.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Response(ResponseEnvelope);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
enum ResponseEnvelope {
    /// The request may proceed.
    Allow {
        contract_version: u16,
        invocation_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        implementation_version: Option<String>,
    },
    /// The request must stop before model selection and provider dispatch.
    Deny {
        contract_version: u16,
        invocation_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason_code: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        implementation_version: Option<String>,
    },
}

impl Response {
    /// Construct a validated allow response correlated to `invocation_id`.
    pub fn allow(
        invocation_id: String,
        implementation_version: Option<String>,
    ) -> Result<Self, ProtocolError> {
        validate_implementation_version(implementation_version.as_deref())?;
        Ok(Self(ResponseEnvelope::Allow {
            contract_version: CONTRACT_VERSION,
            invocation_id,
            implementation_version,
        }))
    }

    /// Construct a validated deny response correlated to `invocation_id`.
    pub fn deny(
        invocation_id: String,
        reason_code: Option<String>,
        implementation_version: Option<String>,
    ) -> Result<Self, ProtocolError> {
        validate_reason_code(reason_code.as_deref())?;
        validate_implementation_version(implementation_version.as_deref())?;
        Ok(Self(ResponseEnvelope::Deny {
            contract_version: CONTRACT_VERSION,
            invocation_id,
            reason_code,
            implementation_version,
        }))
    }

    /// Return the validated decision.
    pub fn decision(&self) -> Decision {
        match &self.0 {
            ResponseEnvelope::Allow { .. } => Decision::Allow,
            ResponseEnvelope::Deny { .. } => Decision::Deny,
        }
    }

    /// Return the invocation identity echoed by this response.
    pub fn invocation_id(&self) -> &str {
        match &self.0 {
            ResponseEnvelope::Allow { invocation_id, .. }
            | ResponseEnvelope::Deny { invocation_id, .. } => invocation_id,
        }
    }

    /// Return the optional machine-readable denial code.
    pub fn reason_code(&self) -> Option<&str> {
        match &self.0 {
            ResponseEnvelope::Allow { .. } => None,
            ResponseEnvelope::Deny { reason_code, .. } => reason_code.as_deref(),
        }
    }

    /// Return the optional implementation version reported by the service.
    pub fn implementation_version(&self) -> Option<&str> {
        match &self.0 {
            ResponseEnvelope::Allow {
                implementation_version,
                ..
            }
            | ResponseEnvelope::Deny {
                implementation_version,
                ..
            } => implementation_version.as_deref(),
        }
    }

    fn contract_version(&self) -> u16 {
        match &self.0 {
            ResponseEnvelope::Allow {
                contract_version, ..
            }
            | ResponseEnvelope::Deny {
                contract_version, ..
            } => *contract_version,
        }
    }
}

/// A sanitized request-check protocol failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    /// The request envelope exceeded [`MAX_REQUEST_BYTES`].
    RequestTooLarge,
    /// The response envelope exceeded [`MAX_RESPONSE_BYTES`].
    ResponseTooLarge,
    /// The request was not valid strict version 1 JSON.
    MalformedRequest,
    /// The response was not valid strict version 1 JSON.
    MalformedResponse,
    /// The envelope declared a different contract version.
    VersionMismatch,
    /// The response did not echo the expected invocation identity.
    InvocationMismatch,
    /// The implementation version was empty, too long, or contained invalid bytes.
    InvalidImplementationVersion,
    /// The denial code was empty, too long, or contained invalid bytes.
    InvalidReasonCode,
    /// A request identifier, binding limit, content fragment, or coverage total was invalid.
    InvalidRequest,
    /// Projected content exceeded the binding or version 1 content limits.
    InputTooLarge,
    /// A validated envelope could not be serialized.
    EncodeFailed,
}

impl ProtocolError {
    /// Return the stable machine-readable diagnostic used by the host.
    pub const fn code(self) -> &'static str {
        match self {
            Self::RequestTooLarge => "request_too_large",
            Self::ResponseTooLarge => "response_too_large",
            Self::MalformedRequest => "malformed_request",
            Self::MalformedResponse => "malformed_response",
            Self::VersionMismatch => "version_mismatch",
            Self::InvocationMismatch => "invocation_mismatch",
            Self::InvalidImplementationVersion => "invalid_implementation_version",
            Self::InvalidReasonCode => "invalid_reason_code",
            Self::InvalidRequest => "invalid_request",
            Self::InputTooLarge => "input_too_large",
            Self::EncodeFailed => "encode_failed",
        }
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for ProtocolError {}

/// Decode and validate a strict version 1 request envelope.
pub fn decode_request(body: &[u8]) -> Result<Request, ProtocolError> {
    if body.len() > MAX_REQUEST_BYTES {
        return Err(ProtocolError::RequestTooLarge);
    }
    let request =
        serde_json::from_slice::<Request>(body).map_err(|_| ProtocolError::MalformedRequest)?;
    if request.contract_version != CONTRACT_VERSION {
        return Err(ProtocolError::VersionMismatch);
    }
    validate_request(&request)?;
    Ok(request)
}

/// Encode a version 1 request without exceeding [`MAX_REQUEST_BYTES`].
pub fn encode_request(request: &Request) -> Result<Vec<u8>, ProtocolError> {
    if request.contract_version != CONTRACT_VERSION {
        return Err(ProtocolError::VersionMismatch);
    }
    validate_request(request)?;
    encode_bounded(request, MAX_REQUEST_BYTES, ProtocolError::RequestTooLarge)
}

/// Decode and validate a strict, correlated version 1 response envelope.
pub fn decode_response(
    expected_invocation_id: &str,
    body: &[u8],
) -> Result<Response, ProtocolError> {
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(ProtocolError::ResponseTooLarge);
    }
    let response = serde_json::from_slice::<ResponseEnvelope>(body)
        .map_err(|_| ProtocolError::MalformedResponse)?;
    let response = Response(response);
    if response.contract_version() != CONTRACT_VERSION {
        return Err(ProtocolError::VersionMismatch);
    }
    if response.invocation_id() != expected_invocation_id {
        return Err(ProtocolError::InvocationMismatch);
    }
    validate_implementation_version(response.implementation_version())?;
    validate_reason_code(response.reason_code())?;
    Ok(response)
}

/// Validate the semantic invariants of a version 1 request.
///
/// Services can call this when they obtain a [`Request`] through a framework
/// extractor instead of [`decode_request`].
pub fn validate_request(request: &Request) -> Result<(), ProtocolError> {
    if request.contract_version != CONTRACT_VERSION {
        return Err(ProtocolError::VersionMismatch);
    }
    if [
        request.invocation_id.as_str(),
        request.request_id.as_str(),
        request.router_id.as_str(),
        request.router_binding_digest.as_str(),
        request.checker.checker_id.as_str(),
        request.checker.binding_digest.as_str(),
    ]
    .into_iter()
    .any(|value| value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES)
    {
        return Err(ProtocolError::InvalidRequest);
    }
    if request.checker.max_input_bytes == 0
        || request.checker.max_input_bytes > MAX_INPUT_BYTES
        || request.checker.timeout_ms == 0
        || request.checker.timeout_ms > MAX_TIMEOUT_MS
    {
        return Err(ProtocolError::InvalidRequest);
    }
    if request.content.len() > MAX_CONTENT_FRAGMENTS {
        return Err(ProtocolError::InputTooLarge);
    }
    if request.coverage.status != CoverageStatus::CompleteWithinScope {
        return Err(ProtocolError::InvalidRequest);
    }
    let fragment_count = u64::try_from(request.content.len()).unwrap_or(u64::MAX);
    if request.coverage.text_fragments != fragment_count {
        return Err(ProtocolError::InvalidRequest);
    }
    let text_bytes = request.content.iter().try_fold(0_u64, |total, fragment| {
        let text = fragment
            .text
            .as_deref()
            .ok_or(ProtocolError::InvalidRequest)?;
        total
            .checked_add(text.len() as u64)
            .ok_or(ProtocolError::InputTooLarge)
    })?;
    if text_bytes != request.coverage.text_bytes {
        return Err(ProtocolError::InvalidRequest);
    }
    if text_bytes > request.checker.max_input_bytes {
        return Err(ProtocolError::InputTooLarge);
    }
    Ok(())
}

/// Encode a validated version 1 response without exceeding [`MAX_RESPONSE_BYTES`].
pub fn encode_response(response: &Response) -> Result<Vec<u8>, ProtocolError> {
    if response.contract_version() != CONTRACT_VERSION {
        return Err(ProtocolError::VersionMismatch);
    }
    validate_implementation_version(response.implementation_version())?;
    validate_reason_code(response.reason_code())?;
    encode_bounded(
        response,
        MAX_RESPONSE_BYTES,
        ProtocolError::ResponseTooLarge,
    )
}

/// Validate an optional implementation identity using the v1 response grammar.
/// Native registrations and configuration must use this same validation before
/// activation because their revision is returned as the implementation version.
pub fn validate_implementation_version(value: Option<&str>) -> Result<(), ProtocolError> {
    if value.is_some_and(|value| {
        value.is_empty()
            || value.len() > MAX_IMPLEMENTATION_VERSION_BYTES
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-' | b'/')
            })
    }) {
        return Err(ProtocolError::InvalidImplementationVersion);
    }
    Ok(())
}

fn validate_reason_code(value: Option<&str>) -> Result<(), ProtocolError> {
    if value.is_some_and(|value| {
        value.is_empty()
            || value.len() > MAX_REASON_CODE_BYTES
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':')
            })
    }) {
        return Err(ProtocolError::InvalidReasonCode);
    }
    Ok(())
}

fn encode_bounded<T: Serialize>(
    value: &T,
    limit: usize,
    too_large: ProtocolError,
) -> Result<Vec<u8>, ProtocolError> {
    let mut output = BoundedBuffer::new(limit);
    match serde_json::to_writer(&mut output, value) {
        Ok(()) => Ok(output.bytes),
        Err(_) if output.exceeded => Err(too_large),
        Err(_) => Err(ProtocolError::EncodeFailed),
    }
}

struct BoundedBuffer {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}

impl BoundedBuffer {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(MAX_RESPONSE_BYTES)),
            limit,
            exceeded: false,
        }
    }
}

impl Write for BoundedBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > self.limit {
            self.exceeded = true;
            return Err(std::io::Error::other(
                "request-check protocol payload limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn request() -> Request {
        Request {
            contract_version: CONTRACT_VERSION,
            invocation_id: "invocation-1".to_owned(),
            request_id: "request-1".to_owned(),
            router_id: "coding".to_owned(),
            router_binding_digest: "router-digest".to_owned(),
            checker: CheckerBinding {
                checker_id: "organization-input".to_owned(),
                binding_digest: "checker-digest".to_owned(),
                max_input_bytes: 262_144,
                timeout_ms: 500,
            },
            content: vec![ContentFragment {
                role: ContentRole::User,
                kind: ContentFragmentKind::Text,
                text: Some("hello".to_owned()),
            }],
            coverage: Coverage {
                scope: CoverageScope::EntryRequestText,
                text_bytes: 5,
                text_fragments: 1,
                excluded_media_fragments: 0,
                status: CoverageStatus::CompleteWithinScope,
            },
        }
    }

    #[test]
    fn request_schema_matches_existing_version_one_envelope() -> Result<(), ProtocolError> {
        let encoded = encode_request(&request())?;
        let value = serde_json::from_slice::<serde_json::Value>(&encoded)
            .map_err(|_| ProtocolError::MalformedRequest)?;
        assert_eq!(
            value,
            json!({
                "contract_version": 1,
                "invocation_id": "invocation-1",
                "request_id": "request-1",
                "router_id": "coding",
                "router_binding_digest": "router-digest",
                "checker": {
                    "checker_id": "organization-input",
                    "binding_digest": "checker-digest",
                    "max_input_bytes": 262144,
                    "timeout_ms": 500
                },
                "content": [{"role": "user", "kind": "text", "text": "hello"}],
                "coverage": {
                    "scope": "entry_request_text",
                    "text_bytes": 5,
                    "text_fragments": 1,
                    "excluded_media_fragments": 0,
                    "status": "complete_within_scope"
                }
            })
        );
        assert_eq!(decode_request(&encoded)?, request());
        Ok(())
    }

    #[test]
    fn response_schema_and_optional_fields_remain_compatible() -> Result<(), ProtocolError> {
        let allow = Response::allow("invocation-1".to_owned(), Some("1.0.0".to_owned()))?;
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&encode_response(&allow)?)
                .map_err(|_| ProtocolError::MalformedResponse)?,
            json!({
                "decision": "allow",
                "contract_version": 1,
                "invocation_id": "invocation-1",
                "implementation_version": "1.0.0"
            })
        );
        let deny = decode_response(
            "invocation-1",
            br#"{"contract_version":1,"invocation_id":"invocation-1","decision":"deny","reason_code":"policy:block"}"#,
        )?;
        assert_eq!(deny.decision(), Decision::Deny);
        assert_eq!(deny.reason_code(), Some("policy:block"));
        assert_eq!(deny.implementation_version(), None);
        Ok(())
    }

    #[test]
    fn response_decode_is_strict_and_correlated() {
        let unknown = br#"{"contract_version":1,"invocation_id":"invocation-1","decision":"allow","extra":true}"#;
        assert_eq!(
            decode_response("invocation-1", unknown),
            Err(ProtocolError::MalformedResponse)
        );
        let wrong_version =
            br#"{"contract_version":2,"invocation_id":"invocation-1","decision":"allow"}"#;
        assert_eq!(
            decode_response("invocation-1", wrong_version),
            Err(ProtocolError::VersionMismatch)
        );
        let wrong_invocation =
            br#"{"contract_version":1,"invocation_id":"other","decision":"allow"}"#;
        assert_eq!(
            decode_response("invocation-1", wrong_invocation),
            Err(ProtocolError::InvocationMismatch)
        );
    }

    #[test]
    fn response_strings_and_envelope_sizes_are_bounded() {
        assert_eq!(
            Response::deny(
                "invocation-1".to_owned(),
                Some("not allowed".to_owned()),
                None
            ),
            Err(ProtocolError::InvalidReasonCode)
        );
        assert_eq!(
            Response::allow("invocation-1".to_owned(), Some("x".repeat(129))),
            Err(ProtocolError::InvalidImplementationVersion)
        );
        assert_eq!(
            decode_response("invocation-1", &vec![b' '; MAX_RESPONSE_BYTES + 1]),
            Err(ProtocolError::ResponseTooLarge)
        );

        let mut oversized = request();
        oversized.content[0].text = Some("x".repeat(MAX_INPUT_BYTES as usize + 1));
        oversized.coverage.text_bytes = MAX_INPUT_BYTES + 1;
        assert_eq!(
            encode_request(&oversized),
            Err(ProtocolError::InputTooLarge)
        );

        let escaped_text = "\0".repeat(MAX_REQUEST_BYTES / 6 + 1);
        let mut oversized_envelope = request();
        oversized_envelope.checker.max_input_bytes = escaped_text.len() as u64;
        oversized_envelope.coverage.text_bytes = escaped_text.len() as u64;
        oversized_envelope.content[0].text = Some(escaped_text);
        assert_eq!(
            encode_request(&oversized_envelope),
            Err(ProtocolError::RequestTooLarge)
        );
        assert_eq!(
            decode_request(&vec![b' '; MAX_REQUEST_BYTES + 1]),
            Err(ProtocolError::RequestTooLarge)
        );
    }

    #[test]
    fn request_decode_rejects_unknown_fields_and_versions() -> Result<(), serde_json::Error> {
        let mut value = serde_json::to_value(request())?;
        if let Some(object) = value.as_object_mut() {
            object.insert("extra".to_owned(), serde_json::Value::Bool(true));
        }
        let encoded = serde_json::to_vec(&value)?;
        assert_eq!(
            decode_request(&encoded),
            Err(ProtocolError::MalformedRequest)
        );

        let mut wrong_version = request();
        wrong_version.contract_version = 2;
        let encoded = serde_json::to_vec(&wrong_version)?;
        assert_eq!(
            decode_request(&encoded),
            Err(ProtocolError::VersionMismatch)
        );
        Ok(())
    }

    #[test]
    fn request_semantics_are_checked_before_service_code_runs() {
        let mut missing_text = request();
        missing_text.content[0].text = None;
        assert_eq!(
            validate_request(&missing_text),
            Err(ProtocolError::InvalidRequest)
        );

        let mut mismatched_coverage = request();
        mismatched_coverage.coverage.text_bytes = 4;
        assert_eq!(
            validate_request(&mismatched_coverage),
            Err(ProtocolError::InvalidRequest)
        );

        let mut incomplete = request();
        incomplete.coverage.status = CoverageStatus::InputTooLarge;
        assert_eq!(
            validate_request(&incomplete),
            Err(ProtocolError::InvalidRequest)
        );

        let mut invalid_identifier = request();
        invalid_identifier.invocation_id = "x".repeat(MAX_IDENTIFIER_BYTES + 1);
        assert_eq!(
            validate_request(&invalid_identifier),
            Err(ProtocolError::InvalidRequest)
        );
    }
}
