//! Host-authenticated Responses continuation for managed canonical history.

use serde::{Deserialize, Serialize};

use super::native_context::{ORIGIN_NAMESPACE, metadata, metadata_mut};
use crate::error::BitrouterError;
use bitrouter_ai::types::{Content, Message, Prompt};

/// Successful source authenticated by the built-in executor, not by a caller's
/// metadata or a count operation. The host seals only the supplied output.
pub struct NativeContinuationSource<'a> {
    /// Full prepared input, including any locally retained prefix.
    pub prompt: &'a Prompt,
    /// Authenticated session owner.
    pub caller: &'a crate::caller::CallerContext,
    /// Actual successful serving target.
    pub target: &'a super::types::RoutingTarget,
    /// Principal bound to the final request's actual credentials and scopes.
    pub authority: &'a bitrouter_ai::auth::ContinuationAuthority,
    /// Private provider response identifier.
    pub response_id: &'a str,
    /// Every retained provider item can be replayed from canonical history.
    pub replayable: bool,
}

/// Encrypted host artifact retained with the assistant message, never on wire.
pub const CONTINUATION_FIELD: &str = "nativeContinuation";

/// Source-proven marker for output whose full provider state is not represented
/// locally. Removing the encrypted handle must not make such output replayable.
pub const REQUIRED_STATE_FIELD: &str = "nativeContextRequired";

/// Stable diagnostics without provider IDs, keys or content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinuationFailure {
    /// No installation policy is configured.
    PolicyUnavailable,
    /// The installation key could not be loaded.
    KeyUnavailable,
    /// The artifact, owner or message binding is invalid.
    ArtifactInvalid,
    /// The authenticated owner differs from the artifact owner.
    OwnerMismatch,
    /// Provider state is required but the visible prefix has changed.
    PrefixMismatch,
    /// Provider state is required but the candidate target differs.
    TargetMismatch,
    /// Provider state is required but the requested effort differs.
    EffortMismatch,
    /// Actual authentication has no stable principal proof.
    AuthorityUnavailable,
    /// Actual authentication differs from the successful source principal.
    AuthorityMismatch,
    /// No authenticated successful HTTP attempt supplied this result.
    AttemptUnverified,
    /// The output cannot carry an authenticated artifact.
    ContentUnavailable,
    /// A request attempted to inject or replace a native provider handle.
    BindingChanged,
    /// Locally retained output needs stored state that no selected handle covers.
    Required,
}

impl ContinuationFailure {
    /// Stable feasibility/error reason, suitable for a redacted receipt.
    pub fn reason(self) -> &'static str {
        match self {
            Self::PolicyUnavailable => "native_continuation_policy_unavailable",
            Self::KeyUnavailable => "native_continuation_key_unavailable",
            Self::ArtifactInvalid => "native_continuation_artifact_invalid",
            Self::OwnerMismatch => "native_continuation_owner_mismatch",
            Self::PrefixMismatch => "native_continuation_prefix_mismatch",
            Self::TargetMismatch => "native_continuation_target_mismatch",
            Self::EffortMismatch => "native_continuation_effort_mismatch",
            Self::AuthorityUnavailable => "native_continuation_authority_unavailable",
            Self::AuthorityMismatch => "native_continuation_authority_mismatch",
            Self::AttemptUnverified => "native_continuation_attempt_unverified",
            Self::ContentUnavailable => "native_continuation_content_unavailable",
            Self::BindingChanged => "native_continuation_binding_changed",
            Self::Required => "native_continuation_required",
        }
    }

    pub(crate) fn error(self) -> BitrouterError {
        BitrouterError::bad_request(self.reason())
    }
}

/// Why complete canonical history is sent instead of a stored provider handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FullHistoryReason {
    /// No authenticated handle is present in the input.
    NoHandle,
    /// The candidate has a different provider/model/endpoint/account/protocol.
    TargetChanged,
    /// The requested reasoning effort changed.
    EffortChanged,
    /// The artifact no longer describes the exact ordered history prefix.
    PrefixChanged,
}

/// Credential-free planned or observed provider context use.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum NativeContinuationInput {
    /// No observation, including an attempt stopped before dispatch.
    #[default]
    Unknown,
    /// Complete history is sent through the serving adapter.
    FullHistory {
        /// Local reason for sending complete history.
        reason: FullHistoryReason,
    },
    /// The provider receives only the suffix after this many canonical messages.
    Resumed {
        /// Number of messages represented by the provider's stored state.
        prefix_messages: u64,
    },
    /// Local candidate preparation failed before dispatch.
    Rejected {
        /// Stable local rejection, without private values.
        reason: ContinuationFailure,
    },
}

/// Evidence for the handle attached to this complete output.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum NativeContinuationOutput {
    /// No output observation exists.
    #[default]
    Unknown,
    /// The protocol has no Responses handle.
    NotSupported,
    /// The actual response did not confirm stored state.
    NotStored,
    /// An encrypted owner/source/prefix-bound artifact was issued.
    Issued,
    /// Output and usage survive, but no reusable artifact could be issued.
    Unverified {
        /// Why provenance or stored state could not be established.
        reason: ContinuationFailure,
    },
}

/// Actual generation evidence; counting alone never populates this observation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeContinuationObservation {
    /// Actual generation dispatch, separately from planned/count work.
    pub input: NativeContinuationInput,
    /// Artifact attached to the complete output before its receipt is emitted.
    pub output: NativeContinuationOutput,
}

/// An ephemeral host resolution. Never serialize the raw provider ID into plans.
pub struct NativeContinuationBinding {
    token: String,
    response_id: String,
    prefix_messages: usize,
    replayable: bool,
}

impl std::fmt::Debug for NativeContinuationBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeContinuationBinding")
            .field("prefix_messages", &self.prefix_messages)
            .finish_non_exhaustive()
    }
}

impl NativeContinuationBinding {
    /// Hosts construct this only after checking installation, owner and prefix.
    pub fn new(
        token: String,
        response_id: String,
        prefix_messages: usize,
        replayable: bool,
    ) -> Self {
        Self {
            token,
            response_id,
            prefix_messages,
            replayable,
        }
    }

    /// Opaque artifact used by the host to recheck the final authenticated request.
    pub fn token(&self) -> &str {
        &self.token
    }

    pub(crate) fn response_id(&self) -> &str {
        &self.response_id
    }
}

/// Pure local selection; the full canonical prompt remains in the durable plan.
#[derive(Debug)]
pub enum NativeContinuationPlan {
    /// Render and send the complete canonical history.
    FullHistory(FullHistoryReason),
    /// Render only the suffix using this authenticated provider-state binding.
    Resume(NativeContinuationBinding),
}

impl NativeContinuationPlan {
    pub(crate) fn observation(&self) -> NativeContinuationInput {
        match self {
            Self::FullHistory(reason) => NativeContinuationInput::FullHistory { reason: *reason },
            Self::Resume(binding) => NativeContinuationInput::Resumed {
                prefix_messages: binding.prefix_messages as u64,
            },
        }
    }

    pub(crate) fn execution_prompt(&self, prompt: &Prompt) -> Result<Prompt, ContinuationFailure> {
        let mut effective = prompt.clone();
        if let Self::Resume(binding) = self {
            effective.messages = prompt
                .messages
                .get(binding.prefix_messages..)
                .ok_or(ContinuationFailure::PrefixMismatch)?
                .to_vec();
        }
        if effective
            .messages
            .iter()
            .any(|message| message.content.iter().any(requires_stored_state))
        {
            return Err(ContinuationFailure::Required);
        }
        Ok(effective)
    }

    pub(crate) fn replayable(&self) -> bool {
        match self {
            Self::FullHistory(_) => true,
            Self::Resume(binding) => binding.replayable,
        }
    }
}

/// Whether a part requires remote state in addition to its canonical projection.
/// Presence, including malformed values, requires the source proof and a handle.
pub fn requires_stored_state(part: &Content) -> bool {
    metadata(part)
        .get(ORIGIN_NAMESPACE)
        .is_some_and(|fields| fields.get(REQUIRED_STATE_FIELD).is_some())
}

pub(crate) fn mark_required_state(content: &mut Vec<Content>) {
    if content.is_empty() {
        content.push(Content::Text {
            text: String::new(),
            provider_metadata: Default::default(),
        });
    }
    let fields = metadata_mut(&mut content[0])
        .entry(ORIGIN_NAMESPACE.into())
        .or_insert_with(|| serde_json::json!({}));
    if !fields.is_object() {
        *fields = serde_json::json!({});
    }
    // Bind each successful remote state independently, even when distinct raw
    // provider outputs collapse to the same visible canonical content.
    fields[REQUIRED_STATE_FIELD] = uuid::Uuid::new_v4().to_string().into();
}

/// Whether any message contains a native handle that requires host validation.
pub fn has_continuation(prompt: &Prompt) -> bool {
    prompt.messages.iter().any(|message| {
        message.content.iter().any(|part| {
            metadata(part)
                .get(ORIGIN_NAMESPACE)
                .is_some_and(|fields| fields.get(CONTINUATION_FIELD).is_some())
        })
    })
}

/// Remove only native handle artifacts, preserving private-part origin proofs.
pub fn clear_continuations(content: &mut [Content]) {
    for part in content {
        let meta = metadata_mut(part);
        let empty = match meta.get_mut(ORIGIN_NAMESPACE) {
            Some(serde_json::Value::Object(fields)) => {
                fields.remove(CONTINUATION_FIELD);
                fields.is_empty()
            }
            Some(_) => true,
            None => false,
        };
        if empty {
            meta.remove(ORIGIN_NAMESPACE);
        }
    }
}

/// Commit canonical history without self-referential SDK markers. Source proofs
/// and native artifacts are validated independently before using this digest.
pub fn history_commitment(messages: &[Message]) -> Result<Vec<u8>, ContinuationFailure> {
    use sha2::{Digest, Sha256};
    let mut messages = messages.to_vec();
    for message in &mut messages {
        super::native_context::clear_origins(&mut message.content);
        clear_continuations(&mut message.content);
    }
    serde_json::to_vec(&messages)
        .map(|bytes| Sha256::digest(bytes).to_vec())
        .map_err(|_| ContinuationFailure::ContentUnavailable)
}
