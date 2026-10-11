//! Installation-authenticated origins for provider-private managed history.

mod continuation;

use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bitrouter_ai::auth::ContinuationAuthority;
use bitrouter_ai::types::{AuthScheme, Content, Prompt, Role};
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::error::Result;
use bitrouter_sdk::language_model::context::PipelineContext;
use bitrouter_sdk::language_model::hooks::{HookDecision, PreRequestHook};
use bitrouter_sdk::language_model::native_context::{
    NativePrivateContextPolicy, ORIGIN_FIELD, ORIGIN_NAMESPACE, PrivateContextFailure, is_private,
    message_commitment, metadata, metadata_mut, requires_origin_validation,
    validate_managed_history,
};
use bitrouter_sdk::language_model::types::RoutingTarget;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;

use super::{ContinuationKey, ContinuationKeySource};

type PolicyResult<T> = std::result::Result<T, PrivateContextFailure>;
const MAX_PROOF_BYTES: usize = 2048;
const PROOF_DOMAIN: &[u8] = b"bitrouter.native-private.origin.v1";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Origin {
    version: u8,
    owner: [u8; 32],
    target: [u8; 32],
    authority: [u8; 32],
    message: Vec<u8>,
    index: usize,
}

/// Shares the persistent installation key with Responses continuation storage;
/// history remains owned and stored by the embedding harness.
#[derive(Clone)]
pub struct PrivateContextPolicy {
    keys: ContinuationKeySource,
}

impl PrivateContextPolicy {
    pub fn new(keys: ContinuationKeySource) -> Self {
        Self { keys }
    }

    fn key(&self) -> PolicyResult<ContinuationKey> {
        self.keys
            .load()
            .map_err(|_| PrivateContextFailure::KeyUnavailable)
    }

    fn validate(
        &self,
        prompt: &Prompt,
        caller: &CallerContext,
        target: Option<&RoutingTarget>,
        authority: Option<&ContinuationAuthority>,
    ) -> PolicyResult<()> {
        if !prompt
            .messages
            .iter()
            .any(|message| message.content.iter().any(requires_origin_validation))
        {
            return Ok(());
        }
        let key = self.key()?;
        let owner = owner_tag(&key, caller)?;
        let target = target.map(|target| target_tag(&key, target)).transpose()?;
        let authority = authority
            .map(|authority| authority_tag(&key, authority))
            .transpose()?;
        for message in &prompt.messages {
            if !message.content.iter().any(requires_origin_validation) {
                continue;
            }
            if message.role != Role::Assistant {
                return Err(PrivateContextFailure::InvalidRole);
            }
            let commitment = message_commitment(&message.content)?;
            for (index, part) in message
                .content
                .iter()
                .enumerate()
                .filter(|(_, part)| requires_origin_validation(part))
            {
                let token = metadata(part)
                    .get(ORIGIN_NAMESPACE)
                    .and_then(|fields| fields.get(ORIGIN_FIELD))
                    .and_then(Value::as_str)
                    .ok_or(PrivateContextFailure::ProofMissing)?;
                let origin = decode(&key, token)?;
                if origin.message != commitment || origin.index != index {
                    return Err(PrivateContextFailure::ProofInvalid);
                }
                if origin.owner != owner {
                    return Err(PrivateContextFailure::OwnerMismatch);
                }
                if target.is_some_and(|target| origin.target != target) {
                    return Err(PrivateContextFailure::TargetMismatch);
                }
                if authority.is_some_and(|authority| origin.authority != authority) {
                    return Err(PrivateContextFailure::AuthorityMismatch);
                }
            }
        }
        Ok(())
    }
}

fn mac(key: &ContinuationKey, domain: &[u8], value: &[u8]) -> PolicyResult<Hmac<Sha256>> {
    let mut mac = Hmac::<Sha256>::new_from_slice(&key.secret)
        .map_err(|_| PrivateContextFailure::KeyUnavailable)?;
    mac.update(&(domain.len() as u64).to_be_bytes());
    mac.update(domain);
    mac.update(&(value.len() as u64).to_be_bytes());
    mac.update(value);
    Ok(mac)
}

fn tag(key: &ContinuationKey, domain: &[u8], value: &impl Serialize) -> PolicyResult<[u8; 32]> {
    let bytes = serde_json::to_vec(value).map_err(|_| PrivateContextFailure::ContentUnavailable)?;
    Ok(mac(key, domain, &bytes)?.finalize().into_bytes().into())
}

fn owner_tag(key: &ContinuationKey, caller: &CallerContext) -> PolicyResult<[u8; 32]> {
    if caller.is_anonymous() {
        return Err(PrivateContextFailure::OwnerMismatch);
    }
    tag(key, b"bitrouter.native-private.owner.v1", &caller.user_id())
}

fn target_tag(key: &ContinuationKey, target: &RoutingTarget) -> PolicyResult<[u8; 32]> {
    tag(
        key,
        b"bitrouter.native-private.target.v1",
        &(
            &target.provider_name,
            &target.service_id,
            target.api_protocol.to_string(),
            target
                .api_base_override
                .as_deref()
                .unwrap_or(&target.api_base),
            &target.account_label,
        ),
    )
}

fn authority_tag(
    key: &ContinuationKey,
    authority: &ContinuationAuthority,
) -> PolicyResult<[u8; 32]> {
    let scheme = match authority.effective_scheme() {
        AuthScheme::XApiKey => "x-api-key",
        AuthScheme::Bearer => "bearer",
    };
    tag(
        key,
        b"bitrouter.native-private.authority.v1",
        &(scheme, authority.credential().proof_bytes()),
    )
}

fn encode(key: &ContinuationKey, origin: &Origin) -> PolicyResult<String> {
    let bytes =
        serde_json::to_vec(origin).map_err(|_| PrivateContextFailure::ContentUnavailable)?;
    let proof = mac(key, PROOF_DOMAIN, &bytes)?.finalize().into_bytes();
    Ok(format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(bytes),
        URL_SAFE_NO_PAD.encode(proof)
    ))
}

fn decode(key: &ContinuationKey, token: &str) -> PolicyResult<Origin> {
    if token.len() > MAX_PROOF_BYTES {
        return Err(PrivateContextFailure::ProofInvalid);
    }
    let (bytes, proof) = token
        .split_once('.')
        .ok_or(PrivateContextFailure::ProofInvalid)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(bytes)
        .map_err(|_| PrivateContextFailure::ProofInvalid)?;
    let proof = URL_SAFE_NO_PAD
        .decode(proof)
        .map_err(|_| PrivateContextFailure::ProofInvalid)?;
    mac(key, PROOF_DOMAIN, &bytes)?
        .verify_slice(&proof)
        .map_err(|_| PrivateContextFailure::ProofInvalid)?;
    let origin: Origin =
        serde_json::from_slice(&bytes).map_err(|_| PrivateContextFailure::ProofInvalid)?;
    if origin.version != 1 {
        return Err(PrivateContextFailure::ProofInvalid);
    }
    Ok(origin)
}

impl NativePrivateContextPolicy for PrivateContextPolicy {
    fn validate_continuation_history(
        &self,
        prompt: &Prompt,
        caller: &CallerContext,
    ) -> continuation::PolicyResult<()> {
        continuation::validate_history(self, prompt, caller)
    }

    fn continuation_plan(
        &self,
        prompt: &Prompt,
        caller: &CallerContext,
        target: &RoutingTarget,
    ) -> continuation::PolicyResult<
        bitrouter_sdk::language_model::native_continuation::NativeContinuationPlan,
    > {
        continuation::plan(self, prompt, caller, target)
    }

    fn validate_continuation_authority(
        &self,
        binding: &bitrouter_sdk::language_model::native_continuation::NativeContinuationBinding,
        caller: &CallerContext,
        target: &RoutingTarget,
        authority: &ContinuationAuthority,
    ) -> continuation::PolicyResult<()> {
        continuation::validate_authority(self, binding, caller, target, authority)
    }

    fn seal_continuation(
        &self,
        source: bitrouter_sdk::language_model::native_continuation::NativeContinuationSource<'_>,
        content: &mut [Content],
    ) -> continuation::PolicyResult<()> {
        continuation::seal(self, source, content)
    }

    fn validate_history(&self, prompt: &Prompt, caller: &CallerContext) -> PolicyResult<()> {
        self.validate(prompt, caller, None, None)
    }

    fn validate_target(
        &self,
        prompt: &Prompt,
        caller: &CallerContext,
        target: &RoutingTarget,
    ) -> PolicyResult<()> {
        self.validate(prompt, caller, Some(target), None)
    }

    fn validate_authority(
        &self,
        prompt: &Prompt,
        caller: &CallerContext,
        target: &RoutingTarget,
        authority: &ContinuationAuthority,
    ) -> PolicyResult<()> {
        self.validate(prompt, caller, Some(target), Some(authority))
    }

    fn seal(
        &self,
        content: &mut [Content],
        caller: &CallerContext,
        target: &RoutingTarget,
        authority: &ContinuationAuthority,
    ) -> PolicyResult<()> {
        if !content.iter().any(is_private) {
            return Ok(());
        }
        let key = self.key()?;
        let owner = owner_tag(&key, caller)?;
        let target = target_tag(&key, target)?;
        let authority = authority_tag(&key, authority)?;
        // Strip all part markers in one commitment before stamping any part.
        let message = message_commitment(content)?;
        let proofs = content
            .iter()
            .enumerate()
            .filter(|(_, part)| is_private(part))
            .map(|(index, _)| {
                encode(
                    &key,
                    &Origin {
                        version: 1,
                        owner,
                        target,
                        authority,
                        message: message.clone(),
                        index,
                    },
                )
                .map(|token| (index, token))
            })
            .collect::<PolicyResult<Vec<_>>>()?;
        for (index, token) in proofs {
            let namespace = metadata_mut(&mut content[index])
                .entry(ORIGIN_NAMESPACE.into())
                .or_insert_with(|| Value::Object(Default::default()));
            if !namespace.is_object() {
                *namespace = Value::Object(Default::default());
            }
            let fields = namespace
                .as_object_mut()
                .ok_or(PrivateContextFailure::ContentUnavailable)?;
            fields.insert(ORIGIN_FIELD.into(), Value::String(token));
        }
        Ok(())
    }
}

#[async_trait]
impl PreRequestHook for PrivateContextPolicy {
    async fn check(&self, ctx: &mut PipelineContext) -> Result<HookDecision> {
        self.revalidate_context(ctx).await
    }

    async fn revalidate_context(&self, ctx: &PipelineContext) -> Result<HookDecision> {
        validate_managed_history(ctx)?;
        Ok(HookDecision::Allow)
    }
}
