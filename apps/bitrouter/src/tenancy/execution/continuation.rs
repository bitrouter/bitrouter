//! One admitted native invocation's opaque history policy. No ambient registry.

mod artifact;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bitrouter_ai::{
    core::evidence::{ContinuationFailure, PrivateContextFailure},
    tenancy::identity::{ExecutionId, PrincipalId, RevisionId, TenantId},
    types::{Content, Prompt, Role},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};

use super::{RetainedRoute, Submission};
use crate::{
    gateway::{
        caller::CallerContext,
        model::target::AuthScheme,
        pipeline::{
            executor::tenancy::native::Proof,
            native_context::{
                NativePrivateContextPolicy, ORIGIN_FIELD, ORIGIN_NAMESPACE, is_private,
                message_commitment, metadata, metadata_mut, requires_origin_validation,
            },
            native_continuation::{
                NativeContinuationBinding, NativeContinuationPlan, NativeContinuationSource,
            },
            types::RoutingTarget,
        },
        providers::auth::ContinuationAuthority,
    },
    tenancy::continuation::PinnedCrypto,
};

type PrivateResult<T> = Result<T, PrivateContextFailure>;
type ContinuationResult<T> = Result<T, ContinuationFailure>;
const ORIGIN_LIMIT: usize = 2048;

#[derive(Serialize)]
struct Owner {
    tenant: TenantId,
    principal: PrincipalId,
}

pub(super) struct Policy {
    crypto: PinnedCrypto,
    owner: Owner,
    execution: ExecutionId,
    revision: RevisionId,
    route: Arc<RetainedRoute>,
    source: Mutex<Option<Proof>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Origin {
    version: u8,
    route: String,
    credential: String,
    candidate: String,
    authority: String,
    message: Vec<u8>,
    index: usize,
}

impl Policy {
    pub(super) fn new(
        crypto: PinnedCrypto,
        submission: &Submission,
        execution: &ExecutionId,
        route: Arc<RetainedRoute>,
    ) -> Self {
        Self {
            crypto,
            owner: Owner {
                tenant: submission.tenant.clone(),
                principal: submission.principal.clone(),
            },
            execution: execution.clone(),
            revision: submission.binding.execution_revision.clone(),
            route,
            source: Mutex::new(None),
        }
    }

    pub(super) fn record(&self, proof: Proof) {
        if let Ok(mut source) = self.source.lock() {
            *source = (proof.execution() == &self.execution).then_some(proof);
        }
    }

    fn caller(&self, caller: &CallerContext) -> PrivateResult<()> {
        if caller.is_anonymous() || caller.user_id() != self.owner.principal.as_str() {
            return Err(PrivateContextFailure::OwnerMismatch);
        }
        Ok(())
    }

    fn candidate(&self, target: &RoutingTarget) -> PrivateResult<String> {
        let bytes = self
            .route
            .candidate_key(target)
            .map_err(|_| PrivateContextFailure::TargetMismatch)?;
        self.crypto
            .tag_bytes("continuation.candidate.v1", &bytes)
            .map_err(|_| PrivateContextFailure::ContentUnavailable)
    }

    fn credential(&self, target: &RoutingTarget) -> PrivateResult<String> {
        let credential = self
            .route
            .match_candidate(target)
            .map_err(|_| PrivateContextFailure::TargetMismatch)?;
        self.crypto
            .tag("continuation.credential.v1", &credential)
            .map_err(|_| PrivateContextFailure::ContentUnavailable)
    }

    fn route_tag(&self) -> PrivateResult<String> {
        self.crypto
            .tag("continuation.route.v1", self.route.reference())
            .map_err(|_| PrivateContextFailure::ContentUnavailable)
    }

    fn authority(&self, authority: &ContinuationAuthority) -> PrivateResult<String> {
        let scheme = match authority.effective_scheme() {
            AuthScheme::Bearer => "bearer",
            AuthScheme::XApiKey => "x-api-key",
        };
        self.crypto
            .tag(
                "continuation.authority.v1",
                &(scheme, authority.credential().proof_bytes()),
            )
            .map_err(|_| PrivateContextFailure::ContentUnavailable)
    }

    fn encode(
        &self,
        purpose: &str,
        prefix: &str,
        value: &impl Serialize,
        limit: usize,
    ) -> anyhow::Result<String> {
        // Bound the inner envelope before allocating the outer base64 text.
        let inner_limit = limit.saturating_sub(prefix.len()).saturating_mul(3) / 4;
        let bytes = self.crypto.seal(purpose, &self.owner, value, inner_limit)?;
        let text = format!("{prefix}{}", URL_SAFE_NO_PAD.encode(bytes));
        anyhow::ensure!(text.len() <= limit, "native artifact exceeds limit");
        Ok(text)
    }

    fn decode<T: serde::de::DeserializeOwned>(
        &self,
        purpose: &str,
        prefix: &str,
        token: &str,
        limit: usize,
    ) -> anyhow::Result<T> {
        anyhow::ensure!(token.len() <= limit, "native artifact exceeds limit");
        let token = token
            .strip_prefix(prefix)
            .ok_or_else(|| anyhow::anyhow!("native artifact format mismatch"))?;
        let bytes = URL_SAFE_NO_PAD.decode(token)?;
        self.crypto.open(purpose, &self.owner, &bytes, limit)
    }

    fn validate_origins(
        &self,
        prompt: &Prompt,
        caller: &CallerContext,
        target: Option<&RoutingTarget>,
        authority: Option<&ContinuationAuthority>,
    ) -> PrivateResult<()> {
        self.caller(caller)?;
        let route = self.route_tag()?;
        let candidate = target.map(|target| self.candidate(target)).transpose()?;
        let credential = target.map(|target| self.credential(target)).transpose()?;
        let authority = authority
            .map(|authority| self.authority(authority))
            .transpose()?;
        for message in &prompt.messages {
            if !message.content.iter().any(requires_origin_validation) {
                continue;
            }
            if message.role != Role::Assistant {
                return Err(PrivateContextFailure::InvalidRole);
            }
            self.validate_parts(
                &message.content,
                &route,
                candidate.as_deref(),
                credential.as_deref(),
                authority.as_deref(),
            )?;
        }
        Ok(())
    }

    fn validate_parts(
        &self,
        content: &[Content],
        route: &str,
        candidate: Option<&str>,
        credential: Option<&str>,
        authority: Option<&str>,
    ) -> PrivateResult<()> {
        let commitment = message_commitment(content)?;
        for (index, part) in content
            .iter()
            .enumerate()
            .filter(|(_, part)| requires_origin_validation(part))
        {
            let token = metadata(part)
                .get(ORIGIN_NAMESPACE)
                .and_then(|fields| fields.get(ORIGIN_FIELD))
                .and_then(Value::as_str)
                .ok_or(PrivateContextFailure::ProofMissing)?;
            let origin: Origin = self
                .decode("continuation.origin.v1", "btpo2.", token, ORIGIN_LIMIT)
                .map_err(|_| PrivateContextFailure::ProofInvalid)?;
            if origin.version != 1
                || origin.message != commitment
                || origin.index != index
                || origin.route != route
            {
                return Err(PrivateContextFailure::ProofInvalid);
            }
            if candidate.is_some_and(|candidate| origin.candidate != candidate)
                || credential.is_some_and(|credential| origin.credential != credential)
            {
                return Err(PrivateContextFailure::TargetMismatch);
            }
            if authority.is_some_and(|authority| origin.authority != authority) {
                return Err(PrivateContextFailure::AuthorityMismatch);
            }
        }
        Ok(())
    }

    fn validate_output_origins(
        &self,
        content: &[Content],
        target: &RoutingTarget,
        authority: &ContinuationAuthority,
    ) -> PrivateResult<()> {
        if !content.iter().any(requires_origin_validation) {
            return Ok(());
        }
        self.validate_parts(
            content,
            &self.route_tag()?,
            Some(&self.candidate(target)?),
            Some(&self.credential(target)?),
            Some(&self.authority(authority)?),
        )
    }

    fn check_source(
        &self,
        proof: &Proof,
        content: &[Content],
        target: &RoutingTarget,
        authority: &ContinuationAuthority,
    ) -> PrivateResult<()> {
        if proof.execution() != &self.execution || !proof.message_matches(content) {
            return Err(PrivateContextFailure::AttemptUnverified);
        }
        if proof.candidate()
            != self
                .route
                .candidate_key(target)
                .map_err(|_| PrivateContextFailure::TargetMismatch)?
            || proof.credential()
                != &self
                    .route
                    .match_candidate(target)
                    .map_err(|_| PrivateContextFailure::TargetMismatch)?
        {
            return Err(PrivateContextFailure::TargetMismatch);
        }
        if proof.authority() != authority {
            return Err(PrivateContextFailure::AuthorityMismatch);
        }
        Ok(())
    }
}

impl NativePrivateContextPolicy for Policy {
    fn validate_history(&self, prompt: &Prompt, caller: &CallerContext) -> PrivateResult<()> {
        self.validate_origins(prompt, caller, None, None)
    }
    fn validate_target(
        &self,
        prompt: &Prompt,
        caller: &CallerContext,
        target: &RoutingTarget,
    ) -> PrivateResult<()> {
        self.validate_origins(prompt, caller, Some(target), None)
    }
    fn validate_authority(
        &self,
        prompt: &Prompt,
        caller: &CallerContext,
        target: &RoutingTarget,
        authority: &ContinuationAuthority,
    ) -> PrivateResult<()> {
        self.validate_origins(prompt, caller, Some(target), Some(authority))
    }
    fn seal(
        &self,
        content: &mut [Content],
        caller: &CallerContext,
        target: &RoutingTarget,
        authority: &ContinuationAuthority,
    ) -> PrivateResult<()> {
        self.caller(caller)?;
        if !content.iter().any(is_private) {
            return Ok(());
        }
        let source = self
            .source
            .lock()
            .map_err(|_| PrivateContextFailure::AttemptUnverified)?;
        let proof = source
            .as_ref()
            .ok_or(PrivateContextFailure::AttemptUnverified)?;
        self.check_source(proof, content, target, authority)?;
        let route = self.route_tag()?;
        let candidate = self.candidate(target)?;
        let credential = self.credential(target)?;
        let authority = self.authority(authority)?;
        let message = message_commitment(content)?;
        let mut proofs = Vec::new();
        let mut remaining = 64 * 1024_usize;
        for (index, _) in content
            .iter()
            .enumerate()
            .filter(|(_, part)| is_private(part))
        {
            if proofs.len() >= 128 {
                return Err(PrivateContextFailure::ContentUnavailable);
            }
            let origin = Origin {
                version: 1,
                route: route.clone(),
                credential: credential.clone(),
                candidate: candidate.clone(),
                authority: authority.clone(),
                message: message.clone(),
                index,
            };
            let token = self
                .encode("continuation.origin.v1", "btpo2.", &origin, ORIGIN_LIMIT)
                .map_err(|_| PrivateContextFailure::ContentUnavailable)?;
            remaining = remaining
                .checked_sub(token.len())
                .ok_or(PrivateContextFailure::ContentUnavailable)?;
            proofs.push((index, token));
        }
        for (index, token) in proofs {
            put(&mut content[index], ORIGIN_FIELD, token);
        }
        Ok(())
    }
    fn validate_continuation_history(
        &self,
        prompt: &Prompt,
        caller: &CallerContext,
    ) -> ContinuationResult<()> {
        artifact::validate_history(self, prompt, caller)
    }
    fn continuation_plan(
        &self,
        prompt: &Prompt,
        caller: &CallerContext,
        target: &RoutingTarget,
    ) -> ContinuationResult<NativeContinuationPlan> {
        artifact::plan(self, prompt, caller, target)
    }
    fn validate_continuation_authority(
        &self,
        binding: &NativeContinuationBinding,
        caller: &CallerContext,
        target: &RoutingTarget,
        authority: &ContinuationAuthority,
    ) -> ContinuationResult<()> {
        artifact::validate_authority(self, binding, caller, target, authority)
    }
    fn seal_continuation(
        &self,
        source: NativeContinuationSource<'_>,
        content: &mut [Content],
    ) -> ContinuationResult<()> {
        artifact::seal(self, source, content)
    }
}

fn put(content: &mut Content, field: &str, token: String) {
    let fields = metadata_mut(content)
        .entry(ORIGIN_NAMESPACE.into())
        .or_insert_with(|| serde_json::json!({}));
    if !fields.is_object() {
        *fields = serde_json::json!({});
    }
    fields[field] = token.into();
}
