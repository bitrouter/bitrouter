//! Encrypted native handles ride in harness-owned history, not a second journal.

use bitrouter_ai::types::{Message, ReasoningEffort};
use bitrouter_sdk::language_model::native_continuation::{
    CONTINUATION_FIELD, ContinuationFailure, FullHistoryReason, NativeContinuationBinding,
    NativeContinuationPlan, NativeContinuationSource, has_continuation, history_commitment,
};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};

use super::*;

pub(super) type PolicyResult<T> = std::result::Result<T, ContinuationFailure>;
const DOMAIN: &[u8] = b"bitrouter.native-continuation.aead.v1";
const MAX_TOKEN_BYTES: usize = 16384;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Artifact {
    version: u8,
    owner: [u8; 32],
    target: [u8; 32],
    authority: [u8; 32],
    message: Vec<u8>,
    prefix: Vec<u8>,
    prefix_messages: usize,
    effort: Option<ReasoningEffort>,
    response_id: String,
    replayable: bool,
}

fn key(policy: &PrivateContextPolicy) -> PolicyResult<ContinuationKey> {
    policy
        .key()
        .map_err(|_| ContinuationFailure::KeyUnavailable)
}

fn owner(key: &ContinuationKey, caller: &CallerContext) -> PolicyResult<[u8; 32]> {
    owner_tag(key, caller).map_err(|_| ContinuationFailure::OwnerMismatch)
}

fn target(key: &ContinuationKey, target: &RoutingTarget) -> PolicyResult<[u8; 32]> {
    target_tag(key, target).map_err(|_| ContinuationFailure::ContentUnavailable)
}

fn authority(key: &ContinuationKey, authority: &ContinuationAuthority) -> PolicyResult<[u8; 32]> {
    authority_tag(key, authority).map_err(|_| ContinuationFailure::AuthorityUnavailable)
}

fn decode(key: &ContinuationKey, token: &str) -> PolicyResult<Artifact> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(ContinuationFailure::ArtifactInvalid);
    }
    let (nonce, ciphertext) = token
        .split_once('.')
        .ok_or(ContinuationFailure::ArtifactInvalid)?;
    let nonce = URL_SAFE_NO_PAD
        .decode(nonce)
        .map_err(|_| ContinuationFailure::ArtifactInvalid)?;
    let nonce: [u8; 12] = nonce
        .try_into()
        .map_err(|_| ContinuationFailure::ArtifactInvalid)?;
    let mut ciphertext = URL_SAFE_NO_PAD
        .decode(ciphertext)
        .map_err(|_| ContinuationFailure::ArtifactInvalid)?;
    // Same installation cipher as gateway continuation, with a distinct AAD.
    // https://docs.rs/ring/latest/ring/aead/struct.LessSafeKey.html
    let key = LessSafeKey::new(
        UnboundKey::new(&AES_256_GCM, &key.secret)
            .map_err(|_| ContinuationFailure::KeyUnavailable)?,
    );
    let bytes = key
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(DOMAIN),
            &mut ciphertext,
        )
        .map_err(|_| ContinuationFailure::ArtifactInvalid)?;
    let artifact: Artifact =
        serde_json::from_slice(bytes).map_err(|_| ContinuationFailure::ArtifactInvalid)?;
    if artifact.version != 1
        || artifact.prefix_messages == 0
        || artifact.response_id.is_empty()
        || artifact.response_id.len() > 512
    {
        return Err(ContinuationFailure::ArtifactInvalid);
    }
    Ok(artifact)
}

fn token(content: &Content) -> Option<&Value> {
    metadata(content)
        .get(ORIGIN_NAMESPACE)?
        .get(CONTINUATION_FIELD)
}

pub(super) fn validate_history(
    policy: &PrivateContextPolicy,
    prompt: &Prompt,
    caller: &CallerContext,
) -> PolicyResult<()> {
    if !has_continuation(prompt) {
        return Ok(());
    }
    let key = key(policy)?;
    let owner = owner(&key, caller)?;
    for message in &prompt.messages {
        for (index, part) in message.content.iter().enumerate() {
            let Some(token) = token(part) else {
                continue;
            };
            if index != 0 || message.role != Role::Assistant {
                return Err(ContinuationFailure::ArtifactInvalid);
            }
            let artifact = decode(
                &key,
                token.as_str().ok_or(ContinuationFailure::ArtifactInvalid)?,
            )?;
            if artifact.owner != owner {
                return Err(ContinuationFailure::OwnerMismatch);
            }
            if artifact.message
                != message_commitment(&message.content)
                    .map_err(|_| ContinuationFailure::ContentUnavailable)?
            {
                return Err(ContinuationFailure::ArtifactInvalid);
            }
        }
    }
    Ok(())
}

pub(super) fn plan(
    policy: &PrivateContextPolicy,
    prompt: &Prompt,
    caller: &CallerContext,
    selected: &RoutingTarget,
) -> PolicyResult<NativeContinuationPlan> {
    validate_history(policy, prompt, caller)?;
    let anchor = prompt
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
        });
    let Some((index, token)) = anchor else {
        return Ok(NativeContinuationPlan::FullHistory(
            FullHistoryReason::NoHandle,
        ));
    };
    let key = key(policy)?;
    let artifact = decode(&key, token)?;
    let mismatch = if artifact.target != target(&key, selected)? {
        Some((
            FullHistoryReason::TargetChanged,
            ContinuationFailure::TargetMismatch,
        ))
    } else if artifact.effort != prompt.params.reasoning_effort {
        Some((
            FullHistoryReason::EffortChanged,
            ContinuationFailure::EffortMismatch,
        ))
    } else if artifact.prefix_messages != index + 1
        || artifact.prefix != history_commitment(&prompt.messages[..=index])?
    {
        Some((
            FullHistoryReason::PrefixChanged,
            ContinuationFailure::PrefixMismatch,
        ))
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
    policy: &PrivateContextPolicy,
    binding: &NativeContinuationBinding,
    caller: &CallerContext,
    selected: &RoutingTarget,
    actual: &ContinuationAuthority,
) -> PolicyResult<()> {
    let key = key(policy)?;
    let artifact = decode(&key, binding.token())?;
    if artifact.owner != owner(&key, caller)? {
        return Err(ContinuationFailure::OwnerMismatch);
    }
    if artifact.target != target(&key, selected)? {
        return Err(ContinuationFailure::TargetMismatch);
    }
    if artifact.authority != authority(&key, actual)? {
        return Err(ContinuationFailure::AuthorityMismatch);
    }
    Ok(())
}

pub(super) fn seal(
    policy: &PrivateContextPolicy,
    source: NativeContinuationSource<'_>,
    content: &mut [Content],
) -> PolicyResult<()> {
    if content.is_empty() || source.response_id.len() > 512 {
        return Err(ContinuationFailure::ContentUnavailable);
    }
    let key = key(policy)?;
    let mut prefix = source.prompt.messages.clone();
    prefix.push(Message {
        role: Role::Assistant,
        content: content.to_vec(),
    });
    let artifact = Artifact {
        version: 1,
        owner: owner(&key, source.caller)?,
        target: target(&key, source.target)?,
        authority: authority(&key, source.authority)?,
        message: message_commitment(content)
            .map_err(|_| ContinuationFailure::ContentUnavailable)?,
        prefix: history_commitment(&prefix)?,
        prefix_messages: prefix.len(),
        effort: source.prompt.params.reasoning_effort,
        response_id: source.response_id.to_owned(),
        replayable: source.replayable,
    };
    let bytes =
        serde_json::to_vec(&artifact).map_err(|_| ContinuationFailure::ContentUnavailable)?;
    let (ciphertext, nonce) = super::super::encrypt(&key, &bytes, DOMAIN)
        .map_err(|_| ContinuationFailure::KeyUnavailable)?;
    let token = format!("{nonce}.{ciphertext}");
    if token.len() > MAX_TOKEN_BYTES {
        return Err(ContinuationFailure::ContentUnavailable);
    }
    let fields = metadata_mut(&mut content[0])
        .entry(ORIGIN_NAMESPACE.into())
        .or_insert_with(|| serde_json::json!({}));
    if !fields.is_object() {
        *fields = serde_json::json!({});
    }
    fields
        .as_object_mut()
        .ok_or(ContinuationFailure::ContentUnavailable)?
        .insert(CONTINUATION_FIELD.into(), Value::String(token));
    Ok(())
}
