//! Tenant-only v1 payload in a distinct opaque token; Legacy bytes never change.

use super::*;
use crate::gateway::pipeline::{
    native_context::clear_origins,
    native_continuation::{CONTINUATION_FIELD, clear_continuations},
};
use bitrouter_ai::{
    core::evidence::{ContinuationFailure as Failure, FullHistoryReason},
    tenancy::{
        configuration::ResourceRef,
        identity::{AttemptId, ExecutionId, RevisionId},
    },
    types::{Message, ReasoningEffort},
};
use sha2::{Digest, Sha256};
use std::io::Write;

const LIMIT: usize = 16384;
const DOMAIN: &str = "continuation.native.v1";
const PREFIX: &str = "btct2.";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    version: u8,
    route: ResourceRef,
    credential: ResourceRef,
    execution: ExecutionId,
    revision: RevisionId,
    attempt: AttemptId,
    candidate: String,
    authority: String,
    message: Vec<u8>,
    prefix: Vec<u8>,
    prefix_messages: usize,
    effort: Option<ReasoningEffort>,
    response_id: String,
    replayable: bool,
}

fn token(content: &Content) -> Option<&Value> {
    metadata(content)
        .get(ORIGIN_NAMESPACE)?
        .get(CONTINUATION_FIELD)
}

fn decode(policy: &Policy, token: &str) -> ContinuationResult<Artifact> {
    let artifact: Artifact = policy
        .decode(DOMAIN, PREFIX, token, LIMIT)
        .map_err(|_| Failure::ArtifactInvalid)?;
    if artifact.version != 1
        || artifact.route != *policy.route.reference()
        || artifact.response_id.is_empty()
        || artifact.response_id.len() > 512
        || artifact.prefix_messages == 0
        || artifact.message.len() != 32
        || artifact.prefix.len() != 32
        || artifact.candidate.len() != 64
        || artifact.authority.len() != 64
    {
        return Err(Failure::ArtifactInvalid);
    }
    Ok(artifact)
}

pub(super) fn validate_history(
    policy: &Policy,
    prompt: &Prompt,
    caller: &CallerContext,
) -> ContinuationResult<()> {
    policy.caller(caller).map_err(|_| Failure::OwnerMismatch)?;
    for message in &prompt.messages {
        for (index, part) in message.content.iter().enumerate() {
            let Some(token) = token(part) else {
                continue;
            };
            if index != 0 || message.role != Role::Assistant {
                return Err(Failure::ArtifactInvalid);
            }
            let artifact = decode(policy, token.as_str().ok_or(Failure::ArtifactInvalid)?)?;
            if artifact.message
                != message_commitment(&message.content).map_err(|_| Failure::ContentUnavailable)?
            {
                return Err(Failure::ArtifactInvalid);
            }
        }
    }
    Ok(())
}

pub(super) fn plan(
    policy: &Policy,
    prompt: &Prompt,
    caller: &CallerContext,
    target: &RoutingTarget,
) -> ContinuationResult<NativeContinuationPlan> {
    validate_history(policy, prompt, caller)?;
    let Some((index, token)) =
        prompt
            .messages
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, message)| {
                message
                    .content
                    .first()
                    .and_then(token)
                    .and_then(Value::as_str)
                    .map(|token| (index, token))
            })
    else {
        return Ok(NativeContinuationPlan::FullHistory(
            FullHistoryReason::NoHandle,
        ));
    };
    let artifact = decode(policy, token)?;
    let credential = policy
        .route
        .match_candidate(target)
        .map_err(|_| Failure::TargetMismatch)?;
    let candidate = policy
        .candidate(target)
        .map_err(|_| Failure::TargetMismatch)?;
    // A different credential ref never inherits a handle, even for equal keys.
    if artifact.credential != credential {
        return Err(Failure::AuthorityMismatch);
    }
    let mismatch = if artifact.candidate != candidate {
        Some((FullHistoryReason::TargetChanged, Failure::TargetMismatch))
    } else if artifact.effort != prompt.params.reasoning_effort {
        Some((FullHistoryReason::EffortChanged, Failure::EffortMismatch))
    } else if artifact.prefix_messages != index + 1
        || artifact.prefix != history(prompt.messages[..=index].iter())?
    {
        Some((FullHistoryReason::PrefixChanged, Failure::PrefixMismatch))
    } else {
        None
    };
    if let Some((reason, failure)) = mismatch {
        return if artifact.replayable {
            Ok(NativeContinuationPlan::FullHistory(reason))
        } else {
            Err(failure)
        };
    }
    Ok(NativeContinuationPlan::Resume(
        NativeContinuationBinding::new(
            token.to_owned(),
            artifact.response_id,
            index + 1,
            artifact.replayable,
        ),
    ))
}

pub(super) fn validate_authority(
    policy: &Policy,
    binding: &NativeContinuationBinding,
    caller: &CallerContext,
    target: &RoutingTarget,
    authority: &ContinuationAuthority,
) -> ContinuationResult<()> {
    policy.caller(caller).map_err(|_| Failure::OwnerMismatch)?;
    let artifact = decode(policy, binding.token())?;
    if artifact.candidate
        != policy
            .candidate(target)
            .map_err(|_| Failure::TargetMismatch)?
    {
        return Err(Failure::TargetMismatch);
    }
    if artifact.credential
        != policy
            .route
            .match_candidate(target)
            .map_err(|_| Failure::TargetMismatch)?
        || artifact.authority
            != policy
                .authority(authority)
                .map_err(|_| Failure::AuthorityUnavailable)?
    {
        return Err(Failure::AuthorityMismatch);
    }
    Ok(())
}

pub(super) fn seal(
    policy: &Policy,
    source: NativeContinuationSource<'_>,
    content: &mut [Content],
) -> ContinuationResult<()> {
    policy
        .caller(source.caller)
        .map_err(|_| Failure::OwnerMismatch)?;
    if content.is_empty() || source.response_id.is_empty() || source.response_id.len() > 512 {
        return Err(Failure::ContentUnavailable);
    }
    let proof = policy
        .source
        .lock()
        .map_err(|_| Failure::AttemptUnverified)?;
    let proof = proof.as_ref().ok_or(Failure::AttemptUnverified)?;
    policy
        .check_source(proof, content, source.target, source.authority)
        .map_err(|_| Failure::AttemptUnverified)?;
    if !proof.continuation_matches(source.prompt, source.response_id, source.replayable) {
        return Err(Failure::AttemptUnverified);
    }
    policy
        .validate_output_origins(content, source.target, source.authority)
        .map_err(|_| Failure::AttemptUnverified)?;
    let assistant = Message {
        role: Role::Assistant,
        content: content.to_vec(),
    };
    let artifact = Artifact {
        version: 1,
        route: policy.route.reference().clone(),
        credential: proof.credential().clone(),
        execution: policy.execution.clone(),
        revision: policy.revision.clone(),
        attempt: proof.attempt().clone(),
        candidate: policy
            .candidate(source.target)
            .map_err(|_| Failure::TargetMismatch)?,
        authority: policy
            .authority(source.authority)
            .map_err(|_| Failure::AuthorityUnavailable)?,
        message: message_commitment(content).map_err(|_| Failure::ContentUnavailable)?,
        prefix: history(
            source
                .prompt
                .messages
                .iter()
                .chain(std::iter::once(&assistant)),
        )?,
        prefix_messages: source
            .prompt
            .messages
            .len()
            .checked_add(1)
            .ok_or(Failure::ContentUnavailable)?,
        effort: source.prompt.params.reasoning_effort,
        response_id: source.response_id.into(),
        replayable: source.replayable,
    };
    let token = policy
        .encode(DOMAIN, PREFIX, &artifact, LIMIT)
        .map_err(|_| Failure::ContentUnavailable)?;
    put(&mut content[0], CONTINUATION_FIELD, token);
    Ok(())
}

/// Same canonical array commitment as the existing native runtime, streamed
/// one message at a time. No complete-history clone or unbounded JSON buffer.
fn history<'a>(messages: impl Iterator<Item = &'a Message>) -> ContinuationResult<Vec<u8>> {
    struct Hash {
        digest: Sha256,
        remaining: usize,
    }
    impl Write for Hash {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.remaining = self
                .remaining
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("native history exceeds limit"))?;
            self.digest.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut hash = Hash {
        digest: Sha256::new(),
        remaining: 16 * 1024 * 1024,
    };
    let write = |_: std::io::Error| Failure::ContentUnavailable;
    hash.write_all(b"[").map_err(write)?;
    for (index, message) in messages.enumerate() {
        if index != 0 {
            hash.write_all(b",").map_err(write)?;
        }
        let mut message = message.clone();
        clear_origins(&mut message.content);
        clear_continuations(&mut message.content);
        serde_json::to_writer(&mut hash, &message).map_err(|_| Failure::ContentUnavailable)?;
    }
    hash.write_all(b"]").map_err(write)?;
    Ok(hash.digest.finalize().to_vec())
}

#[cfg(test)]
mod tests;
