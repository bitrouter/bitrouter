//! Host-authenticated provenance for private parts in managed agent history.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::auth::ContinuationAuthority;
use super::context::PipelineContext;
use super::types::{Content, GenerateResult, Prompt, ProviderMetadata, RoutingTarget};
use crate::caller::CallerContext;
use crate::error::{BitrouterError, Result};

/// The only reserved SDK-owned field used for private-history provenance.
pub const ORIGIN_NAMESPACE: &str = "bitrouter";
/// Opaque host-authenticated proof; never forwarded to a provider.
pub const ORIGIN_FIELD: &str = "nativePrivateOrigin";

/// Controlled rejection/uncertainty reasons, without private identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivateContextFailure {
    /// No host policy is installed for private history.
    PolicyUnavailable,
    /// The installation key cannot be loaded.
    KeyUnavailable,
    /// A private part has no origin proof.
    ProofMissing,
    /// The proof, message content or part position is invalid.
    ProofInvalid,
    /// The authenticated owner differs from the origin owner.
    OwnerMismatch,
    /// The selected provider/model/endpoint/account differs from the origin.
    TargetMismatch,
    /// Final authentication cannot prove its credential principal.
    AuthorityUnavailable,
    /// Final authentication differs from the origin credential principal.
    AuthorityMismatch,
    /// Private parts must belong to an assistant message.
    InvalidRole,
    /// No successful HTTP result supplies provenance for this output.
    AttemptUnverified,
    /// Content could not be committed or sealed.
    ContentUnavailable,
}

impl PrivateContextFailure {
    /// Stable protocol feasibility diagnostic.
    pub fn reason(self) -> &'static str {
        match self {
            Self::PolicyUnavailable => "private_context_policy_unavailable",
            Self::KeyUnavailable => "private_context_key_unavailable",
            Self::ProofMissing => "private_context_proof_missing",
            Self::ProofInvalid => "private_context_proof_invalid",
            Self::OwnerMismatch => "private_context_owner_mismatch",
            Self::TargetMismatch => "private_context_target_mismatch",
            Self::AuthorityUnavailable => "private_context_authority_unavailable",
            Self::AuthorityMismatch => "private_context_authority_mismatch",
            Self::InvalidRole => "private_context_invalid_role",
            Self::AttemptUnverified => "private_context_attempt_unverified",
            Self::ContentUnavailable => "private_context_content_unavailable",
        }
    }

    pub(crate) fn error(self) -> BitrouterError {
        BitrouterError::bad_request(self.reason())
    }
}

/// Explicit evidence; old receipts without this field remain unknown.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PrivateContextEvidence {
    /// No observation is available, including an attempt stopped before send.
    #[default]
    Unknown,
    /// The observed input/output contains no private parts.
    NotPresent,
    /// The observed private parts have verified source bindings.
    Verified {
        /// Number of authenticated private parts.
        parts: u64,
    },
    /// The result is retained, but its private context cannot be safely reused.
    Unverified {
        /// Controlled reason for missing provenance.
        reason: PrivateContextFailure,
    },
}

/// Private context actually sent and returned by one managed attempt.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativePrivateContextObservation {
    /// Set only immediately before a generation request is dispatched.
    pub input: PrivateContextEvidence,
    /// Set before the attempt report and core history receive the result.
    pub output: PrivateContextEvidence,
}

/// Host-owned installation-key policy. Validation is local and read-only;
/// it must not resolve credentials, perform provider I/O or mutate the prompt.
pub trait NativePrivateContextPolicy: Send + Sync {
    /// Validate owner, role, proof and complete message integrity before any
    /// external checker or route preparation can observe private history.
    fn validate_history(
        &self,
        prompt: &Prompt,
        caller: &CallerContext,
    ) -> std::result::Result<(), PrivateContextFailure>;

    /// Additionally validate the immutable candidate's source identity.
    fn validate_target(
        &self,
        prompt: &Prompt,
        caller: &CallerContext,
        target: &RoutingTarget,
    ) -> std::result::Result<(), PrivateContextFailure>;

    /// Verify the final authenticated authority on every actual request/retry.
    fn validate_authority(
        &self,
        prompt: &Prompt,
        caller: &CallerContext,
        target: &RoutingTarget,
        authority: &ContinuationAuthority,
    ) -> std::result::Result<(), PrivateContextFailure>;

    /// Seal the complete assistant message using the actual successful attempt.
    /// Failure must preserve billed output and usage; the SDK removes any
    /// partially written markers and records unverified provenance instead.
    fn seal(
        &self,
        content: &mut [Content],
        caller: &CallerContext,
        target: &RoutingTarget,
        authority: &ContinuationAuthority,
    ) -> std::result::Result<(), PrivateContextFailure>;
}

/// Read a part's canonical metadata without converting it to provider wire JSON.
pub fn metadata(content: &Content) -> &ProviderMetadata {
    match content {
        Content::Text {
            provider_metadata, ..
        }
        | Content::Reasoning {
            provider_metadata, ..
        }
        | Content::File {
            provider_metadata, ..
        }
        | Content::ToolCall {
            provider_metadata, ..
        }
        | Content::ToolResult {
            provider_metadata, ..
        }
        | Content::Source {
            provider_metadata, ..
        }
        | Content::ToolApprovalRequest {
            provider_metadata, ..
        }
        | Content::ToolApprovalResponse {
            provider_metadata, ..
        } => provider_metadata,
    }
}

/// Mutable counterpart used only while constructing a host-sealed output.
pub fn metadata_mut(content: &mut Content) -> &mut ProviderMetadata {
    match content {
        Content::Text {
            provider_metadata, ..
        }
        | Content::Reasoning {
            provider_metadata, ..
        }
        | Content::File {
            provider_metadata, ..
        }
        | Content::ToolCall {
            provider_metadata, ..
        }
        | Content::ToolResult {
            provider_metadata, ..
        }
        | Content::Source {
            provider_metadata, ..
        }
        | Content::ToolApprovalRequest {
            provider_metadata, ..
        }
        | Content::ToolApprovalResponse {
            provider_metadata, ..
        } => provider_metadata,
    }
}

/// Provider-private data that cannot acquire provenance merely by being renderable.
/// https://platform.claude.com/docs/en/build-with-claude/extended-thinking
/// https://ai.google.dev/gemini-api/docs/thought-signatures
pub fn is_private(content: &Content) -> bool {
    let meta = metadata(content);
    meta.get("anthropic").is_some_and(|value| {
        value.get("signature").is_some()
            || value.get("redactedThinking").is_some()
            || value.get("redactedData").is_some()
    }) || meta
        .get("google")
        .is_some_and(|value| value.get("thoughtSignature").is_some())
}

/// Proof-bearing parts still require integrity validation when a caller removes
/// their provider-private metadata. Otherwise that removal could bypass checks.
pub fn requires_origin_validation(content: &Content) -> bool {
    is_private(content)
        || metadata(content)
            .get(ORIGIN_NAMESPACE)
            .is_some_and(|fields| fields.get(ORIGIN_FIELD).is_some())
}

/// Opaque encrypted thinking must never be projected as checker-readable text.
pub fn is_opaque_reasoning(content: &Content) -> bool {
    matches!(content, Content::Reasoning { .. })
        && metadata(content)
            .get("anthropic")
            .is_some_and(|value| value.get("redactedThinking").is_some())
}

/// Remove every origin marker before computing the whole-message commitment,
/// or when output provenance cannot be verified.
pub fn clear_origins(content: &mut [Content]) {
    for part in content {
        let meta = metadata_mut(part);
        let remove_namespace = match meta.get_mut(ORIGIN_NAMESPACE) {
            Some(serde_json::Value::Object(fields)) => {
                fields.remove(ORIGIN_FIELD);
                fields.is_empty()
            }
            Some(_) => true,
            None => false,
        };
        if remove_namespace {
            meta.remove(ORIGIN_NAMESPACE);
        }
    }
}

/// Commit all parts in their original order, excluding all SDK origin markers.
pub fn message_commitment(
    content: &[Content],
) -> std::result::Result<Vec<u8>, PrivateContextFailure> {
    let mut clean = content.to_vec();
    clear_origins(&mut clean);
    serde_json::to_vec(&clean)
        .map(|bytes| Sha256::digest(bytes).to_vec())
        .map_err(|_| PrivateContextFailure::ContentUnavailable)
}

fn private_count(content: &[Content]) -> u64 {
    content.iter().filter(|part| is_private(part)).count() as u64
}

struct SuccessfulSource {
    target: RoutingTarget,
    authority: Option<ContinuationAuthority>,
    result: Vec<u8>,
}

#[derive(Default)]
struct AttemptState {
    observation: NativePrivateContextObservation,
    source: Option<SuccessfulSource>,
}

/// Only the built-in executor can provide a successful HTTP source. Count
/// operations and external custom executors cannot populate this private slot.
pub(crate) struct NativePrivateContextRuntime {
    policy: Option<Arc<dyn NativePrivateContextPolicy>>,
    attempt: Mutex<AttemptState>,
}

impl NativePrivateContextRuntime {
    pub(crate) fn new(policy: Option<Arc<dyn NativePrivateContextPolicy>>) -> Self {
        Self {
            policy,
            attempt: Mutex::new(AttemptState::default()),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, AttemptState> {
        match self.attempt.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    pub(crate) fn begin_attempt(&self) {
        *self.state() = AttemptState::default();
    }

    fn has_private(prompt: &Prompt) -> bool {
        prompt
            .messages
            .iter()
            .any(|message| message.content.iter().any(requires_origin_validation))
    }

    fn policy_for(
        &self,
        prompt: &Prompt,
    ) -> std::result::Result<Option<&dyn NativePrivateContextPolicy>, PrivateContextFailure> {
        if !Self::has_private(prompt) {
            return Ok(None);
        }
        self.policy
            .as_deref()
            .map(Some)
            .ok_or(PrivateContextFailure::PolicyUnavailable)
    }

    pub(crate) fn validate_history(
        &self,
        ctx: &PipelineContext,
    ) -> std::result::Result<(), PrivateContextFailure> {
        match self.policy_for(ctx.prompt())? {
            Some(policy) => policy.validate_history(ctx.prompt(), ctx.caller()),
            None => Ok(()),
        }
    }

    pub(crate) fn validate_target(
        &self,
        prompt: &Prompt,
        ctx: &PipelineContext,
        target: &RoutingTarget,
    ) -> std::result::Result<(), PrivateContextFailure> {
        match self.policy_for(prompt)? {
            Some(policy) => policy.validate_target(prompt, ctx.caller(), target),
            None => Ok(()),
        }
    }

    pub(crate) fn validate_authority(
        &self,
        ctx: &PipelineContext,
        target: &RoutingTarget,
        authority: Option<&ContinuationAuthority>,
    ) -> std::result::Result<(), PrivateContextFailure> {
        match self.policy_for(ctx.prompt())? {
            Some(policy) => policy.validate_authority(
                ctx.prompt(),
                ctx.caller(),
                target,
                authority.ok_or(PrivateContextFailure::AuthorityUnavailable)?,
            ),
            None => Ok(()),
        }
    }

    pub(crate) fn dispatched(&self, prompt: &Prompt) {
        let parts = prompt
            .messages
            .iter()
            .map(|message| private_count(&message.content))
            .sum();
        self.state().observation.input = if parts == 0 {
            PrivateContextEvidence::NotPresent
        } else {
            PrivateContextEvidence::Verified { parts }
        };
    }

    pub(crate) fn succeeded(
        &self,
        target: &RoutingTarget,
        authority: Option<ContinuationAuthority>,
        result: &GenerateResult,
    ) {
        self.state().source = serde_json::to_vec(result)
            .ok()
            .map(|bytes| SuccessfulSource {
                target: target.clone(),
                authority,
                result: Sha256::digest(bytes).to_vec(),
            });
    }

    pub(crate) fn seal_output(&self, ctx: &PipelineContext, result: &mut GenerateResult) {
        let source = self.state().source.take();
        let authentic = source.as_ref().is_some_and(|source| {
            serde_json::to_vec(result)
                .is_ok_and(|bytes| Sha256::digest(bytes).as_slice() == source.result)
        });
        clear_origins(&mut result.content);
        // Canonical correlation IDs are generated once, before both the durable
        // receipt and history observe them. They are not provider-assigned IDs.
        for part in &mut result.content {
            if let Content::ToolCall {
                id,
                provider_executed: false,
                ..
            } = part
                && id.is_empty()
            {
                *id = format!("provider_call_{}", uuid::Uuid::new_v4());
            }
        }
        let parts = private_count(&result.content);
        let outcome = if parts == 0 {
            Ok(())
        } else {
            (|| {
                if !authentic {
                    return Err(PrivateContextFailure::AttemptUnverified);
                }
                let source = source.ok_or(PrivateContextFailure::AttemptUnverified)?;
                let authority = source
                    .authority
                    .ok_or(PrivateContextFailure::AuthorityUnavailable)?;
                self.policy
                    .as_deref()
                    .ok_or(PrivateContextFailure::PolicyUnavailable)?
                    .seal(
                        &mut result.content,
                        ctx.caller(),
                        &source.target,
                        &authority,
                    )
            })()
        };
        self.state().observation.output = match outcome {
            Ok(()) if parts == 0 => PrivateContextEvidence::NotPresent,
            Ok(()) => PrivateContextEvidence::Verified { parts },
            Err(reason) => {
                clear_origins(&mut result.content);
                PrivateContextEvidence::Unverified { reason }
            }
        };
    }

    pub(crate) fn observation(&self) -> NativePrivateContextObservation {
        self.state().observation.clone()
    }
}

/// Hosts call this immediately after authentication, before any egress-capable
/// preparation hook. The pipeline also calls it before every external checker.
pub fn validate_managed_history(ctx: &PipelineContext) -> Result<()> {
    match ctx.extension::<NativePrivateContextRuntime>() {
        Some(runtime) => runtime
            .validate_history(ctx)
            .map_err(PrivateContextFailure::error),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests;
