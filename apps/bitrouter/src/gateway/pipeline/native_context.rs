//! Host-authenticated provenance for private parts in managed agent history.

use bitrouter_ai::core::evidence::{
    NativePrivateContextObservation, PrivateContextEvidence, PrivateContextFailure,
};
use std::sync::{Arc, Mutex};

use sha2::{Digest, Sha256};

use super::context::PipelineContext;
use super::native_continuation::{
    NativeContinuationBinding, NativeContinuationPlan, NativeContinuationSource,
    clear_continuations, has_continuation, requires_stored_state,
};
use super::types::RoutingTarget;
use crate::gateway::caller::CallerContext;
use crate::gateway::error::{BitrouterError, Result};
use crate::gateway::providers::auth::ContinuationAuthority;
use bitrouter_ai::core::evidence::{
    ContinuationFailure, FullHistoryReason, NativeContinuationObservation, NativeContinuationOutput,
};
use bitrouter_ai::types::{Content, GenerateResult, Prompt, ProviderMetadata};

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

fn private_count(content: &[Content]) -> u64 {
    content.iter().filter(|part| is_private(part)).count() as u64
}

struct SuccessfulSource {
    prompt: Prompt,
    target: RoutingTarget,
    authority: Option<ContinuationAuthority>,
    result: Vec<u8>,
    stored_response: bool,
    replayable: bool,
    effort_bound: bool,
}

#[derive(Default)]
struct AttemptState {
    observation: NativePrivateContextObservation,
    continuation: NativeContinuationObservation,
    response_terminal_valid: bool,
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

    pub(crate) fn continuation_plan(
        &self,
        prompt: &Prompt,
        caller: &CallerContext,
        target: &RoutingTarget,
    ) -> std::result::Result<NativeContinuationPlan, ContinuationFailure> {
        let plan = match self.policy.as_deref() {
            Some(policy) => policy.continuation_plan(prompt, caller, target),
            None if has_continuation(prompt) => Err(ContinuationFailure::PolicyUnavailable),
            None => Ok(NativeContinuationPlan::FullHistory(
                FullHistoryReason::NoHandle,
            )),
        }?;
        // Include state requirements outside an older anchor or imported from
        // another branch; the latest artifact alone cannot prove replayability.
        plan.execution_prompt(prompt)?;
        Ok(plan)
    }

    pub(crate) fn validate_continuation_authority(
        &self,
        ctx: &PipelineContext,
        target: &RoutingTarget,
        actual: Option<&ContinuationAuthority>,
    ) -> std::result::Result<(), ContinuationFailure> {
        if let NativeContinuationPlan::Resume(binding) = self.continuation_plan(
            ctx.generation_prompt()
                .ok_or(ContinuationFailure::BindingChanged)?,
            ctx.caller(),
            target,
        )? {
            self.policy
                .as_deref()
                .ok_or(ContinuationFailure::PolicyUnavailable)?
                .validate_continuation_authority(
                    &binding,
                    ctx.caller(),
                    target,
                    actual.ok_or(ContinuationFailure::AuthorityUnavailable)?,
                )?;
        }
        Ok(())
    }

    pub(crate) fn continuation_dispatched(&self, plan: &NativeContinuationPlan) {
        self.state().continuation.input = plan.observation();
    }

    pub(crate) fn continuation_observation(&self) -> NativeContinuationObservation {
        self.state().continuation.clone()
    }

    pub(crate) fn response_terminal_valid(&self) -> bool {
        self.state().response_terminal_valid
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
        match self.policy_for(
            ctx.generation_prompt()
                .ok_or(PrivateContextFailure::PolicyUnavailable)?,
        )? {
            Some(policy) => policy.validate_history(
                ctx.generation_prompt()
                    .ok_or(PrivateContextFailure::PolicyUnavailable)?,
                ctx.caller(),
            ),
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
        match self.policy_for(
            ctx.generation_prompt()
                .ok_or(PrivateContextFailure::PolicyUnavailable)?,
        )? {
            Some(policy) => policy.validate_authority(
                ctx.generation_prompt()
                    .ok_or(PrivateContextFailure::PolicyUnavailable)?,
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
        prompt: &Prompt,
        target: &RoutingTarget,
        authority: Option<ContinuationAuthority>,
        result: &GenerateResult,
    ) {
        self.state().source = serde_json::to_vec(result)
            .ok()
            .map(|bytes| SuccessfulSource {
                prompt: prompt.clone(),
                target: target.clone(),
                authority,
                result: Sha256::digest(bytes).to_vec(),
                stored_response: false,
                replayable: false,
                effort_bound: false,
            });
    }

    pub(crate) fn stored_response(&self, stored: bool, replayable: bool, effort_bound: bool) {
        if let Some(source) = self.state().source.as_mut() {
            source.stored_response = stored;
            source.replayable = replayable;
            source.effort_bound = effort_bound;
        }
    }

    pub(crate) fn seal_output(
        &self,
        ctx: &PipelineContext,
        target: &RoutingTarget,
        result: &mut GenerateResult,
    ) {
        let source = self.state().source.take();
        let authentic = source.as_ref().is_some_and(|source| {
            serde_json::to_vec(result)
                .is_ok_and(|bytes| Sha256::digest(bytes).as_slice() == source.result)
        });
        clear_origins(&mut result.content);
        clear_continuations(&mut result.content);
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
                let source = source
                    .as_ref()
                    .ok_or(PrivateContextFailure::AttemptUnverified)?;
                let authority = source
                    .authority
                    .as_ref()
                    .ok_or(PrivateContextFailure::AuthorityUnavailable)?;
                self.policy
                    .as_deref()
                    .ok_or(PrivateContextFailure::PolicyUnavailable)?
                    .seal(&mut result.content, ctx.caller(), &source.target, authority)
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
        self.state().continuation.output = match source.as_ref() {
            Some(source)
                if source.target.api_protocol != bitrouter_ai::types::ApiProtocol::Responses =>
            {
                NativeContinuationOutput::NotSupported
            }
            Some(source) if !source.stored_response => NativeContinuationOutput::NotStored,
            Some(source) => {
                let sealed = (|| {
                    if !authentic {
                        return Err(ContinuationFailure::AttemptUnverified);
                    }
                    if !source.effort_bound
                        || source.prompt
                            != *ctx
                                .generation_prompt()
                                .ok_or(ContinuationFailure::BindingChanged)?
                    {
                        return Err(ContinuationFailure::BindingChanged);
                    }
                    let authority = source
                        .authority
                        .as_ref()
                        .ok_or(ContinuationFailure::AuthorityUnavailable)?;
                    let policy = self
                        .policy
                        .as_deref()
                        .ok_or(ContinuationFailure::PolicyUnavailable)?;
                    let plan = self.continuation_plan(
                        ctx.generation_prompt()
                            .ok_or(ContinuationFailure::BindingChanged)?,
                        ctx.caller(),
                        &source.target,
                    )?;
                    policy.seal_continuation(
                        NativeContinuationSource {
                            prompt: ctx
                                .generation_prompt()
                                .ok_or(ContinuationFailure::BindingChanged)?,
                            caller: ctx.caller(),
                            target: &source.target,
                            authority,
                            response_id: result
                                .response_id
                                .as_deref()
                                .filter(|id| !id.is_empty())
                                .ok_or(ContinuationFailure::ContentUnavailable)?,
                            replayable: source.replayable && plan.replayable(),
                        },
                        &mut result.content,
                    )
                })();
                match sealed {
                    Ok(()) => NativeContinuationOutput::Issued,
                    Err(reason) => {
                        clear_continuations(&mut result.content);
                        NativeContinuationOutput::Unverified { reason }
                    }
                }
            }
            None => NativeContinuationOutput::Unverified {
                reason: ContinuationFailure::AttemptUnverified,
            },
        };
        // Managed callers receive only the encrypted artifact. Even unverified
        // custom or stream-bridge output must not expose raw handles in receipts.
        if target.api_protocol == bitrouter_ai::types::ApiProtocol::Responses {
            self.state().response_terminal_valid = result
                .response_id
                .as_deref()
                .is_some_and(|id| !id.is_empty())
                && matches!(
                    result.finish_reason,
                    Some(
                        bitrouter_ai::types::FinishReason::Stop
                            | bitrouter_ai::types::FinishReason::Length
                    )
                );
            result.response_id = None;
        }
    }

    pub(crate) fn observation(&self) -> NativePrivateContextObservation {
        self.state().observation.clone()
    }
}

/// Hosts call this immediately after authentication, before any egress-capable
/// preparation hook. The pipeline also calls it before every external checker.
pub fn validate_managed_history(ctx: &PipelineContext) -> Result<()> {
    match ctx.extension::<NativePrivateContextRuntime>() {
        Some(runtime) => {
            runtime
                .validate_history(ctx)
                .map_err(crate::gateway::pipeline::native_context::private_context_error)?;
            match runtime.policy.as_deref() {
                Some(policy) => policy
                    .validate_continuation_history(ctx.require_generation_prompt()?, ctx.caller())
                    .map_err(crate::gateway::pipeline::native_continuation::continuation_error),
                None if has_continuation(ctx.require_generation_prompt()?) => Err(
                    crate::gateway::pipeline::native_continuation::continuation_error(
                        ContinuationFailure::PolicyUnavailable,
                    ),
                ),
                None => Ok(()),
            }
        }
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests;

pub(crate) fn private_context_error(reason: PrivateContextFailure) -> BitrouterError {
    BitrouterError::bad_request(reason.reason())
}
