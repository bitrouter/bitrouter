//! Host-authenticated Responses continuation for managed canonical history.

use bitrouter_ai::core::evidence::{
    ContinuationFailure, FullHistoryReason, NativeContinuationInput,
};

use super::native_context::{ORIGIN_NAMESPACE, metadata, metadata_mut};
use crate::gateway::error::BitrouterError;
use bitrouter_ai::types::{Content, Message, Prompt};

/// Successful source authenticated by the built-in executor, not by a caller's
/// metadata or a count operation. The host seals only the supplied output.
pub struct NativeContinuationSource<'a> {
    /// Full prepared input, including any locally retained prefix.
    pub prompt: &'a Prompt,
    /// Authenticated session owner.
    pub caller: &'a crate::gateway::caller::CallerContext,
    /// Actual successful serving target.
    pub target: &'a super::types::RoutingTarget,
    /// Principal bound to the final request's actual credentials and scopes.
    pub authority: &'a crate::gateway::providers::auth::ContinuationAuthority,
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

pub(crate) fn continuation_error(reason: ContinuationFailure) -> BitrouterError {
    BitrouterError::bad_request(reason.reason())
}
