//! Host-authenticated provenance for private parts in managed agent history.

use crate::managed_contract::evidence::PrivateContextFailure;

use sha2::{Digest, Sha256};

use super::native_continuation::{
    NativeContinuationBinding, NativeContinuationPlan, NativeContinuationSource,
    clear_continuations, has_continuation,
};
use super::types::RoutingTarget;
use crate::gateway::caller::CallerContext;
use crate::gateway::providers::auth::ContinuationAuthority;
use crate::managed_contract::evidence::{ContinuationFailure, FullHistoryReason};
use bitrouter_ai::types::{Content, Prompt, ProviderMetadata};

/// The only reserved SDK-owned field used for private-history provenance.
pub const ORIGIN_NAMESPACE: &str = "bitrouter";
/// Opaque host-authenticated proof; never forwarded to a provider.
pub const ORIGIN_FIELD: &str = "nativePrivateOrigin";

/// Host-owned installation-key policy. Validation is local and read-only;
/// it must not resolve credentials, perform provider I/O or mutate the prompt.
pub trait NativePrivateContextPolicy: Send + Sync {
    /// Authenticate stored Responses artifacts before preparation/checker egress.
    fn validate_continuation_history(
        &self,
        prompt: &Prompt,
        _caller: &CallerContext,
    ) -> std::result::Result<(), ContinuationFailure> {
        if has_continuation(prompt) {
            Err(ContinuationFailure::PolicyUnavailable)
        } else {
            Ok(())
        }
    }

    /// Select a provider execution view locally, without changing canonical history.
    fn continuation_plan(
        &self,
        prompt: &Prompt,
        caller: &CallerContext,
        _target: &RoutingTarget,
    ) -> std::result::Result<NativeContinuationPlan, ContinuationFailure> {
        self.validate_continuation_history(prompt, caller)?;
        Ok(NativeContinuationPlan::FullHistory(
            FullHistoryReason::NoHandle,
        ))
    }

    /// Recheck actual final authentication before generation or count dispatch.
    fn validate_continuation_authority(
        &self,
        _binding: &NativeContinuationBinding,
        _caller: &CallerContext,
        _target: &RoutingTarget,
        _authority: &ContinuationAuthority,
    ) -> std::result::Result<(), ContinuationFailure> {
        Err(ContinuationFailure::PolicyUnavailable)
    }

    /// Attach an encrypted handle to the output, preserving output/usage on error.
    fn seal_continuation(
        &self,
        _source: NativeContinuationSource<'_>,
        _content: &mut [Content],
    ) -> std::result::Result<(), ContinuationFailure> {
        Err(ContinuationFailure::PolicyUnavailable)
    }

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
/// <https://platform.claude.com/docs/en/build-with-claude/extended-thinking>
/// <https://ai.google.dev/gemini-api/docs/thought-signatures>
/// <https://developers.openai.com/api/docs/guides/reasoning>
pub fn is_private(content: &Content) -> bool {
    let meta = metadata(content);
    matches!(
        content,
        Content::Reasoning {
            native: Some(_),
            ..
        }
    ) || meta.get("anthropic").is_some_and(|value| {
        value.get("signature").is_some()
            || value.get("redactedThinking").is_some()
            || value.get("redactedData").is_some()
    }) || meta
        .get("google")
        .is_some_and(|value| value.get("thoughtSignature").is_some())
        || meta
            .get("openai")
            .is_some_and(|value| value.get("reasoningItem").is_some())
        || requires_stored_state(content)
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

/// Opaque payloads excluded from checker text, including metadata attached to
/// readable summaries. A readable summary still needs to be checked.
pub(crate) fn has_opaque_payload(content: &Content) -> bool {
    matches!(content, Content::Reasoning { native: Some(bitrouter_ai::types::NativeReasoning::Responses(item)), .. } if item.get("encrypted_content").is_some_and(|value| !value.is_null()))
        || is_opaque_reasoning(content)
        || metadata(content)
            .get("openai")
            .and_then(|fields| fields.get("reasoningItem"))
            .and_then(|item| item.get("encrypted_content"))
            .is_some_and(|value| !value.is_null())
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
    clear_continuations(&mut clean);
    serde_json::to_vec(&clean)
        .map(|bytes| Sha256::digest(bytes).to_vec())
        .map_err(|_| PrivateContextFailure::ContentUnavailable)
}

#[cfg(test)]
mod tests;
