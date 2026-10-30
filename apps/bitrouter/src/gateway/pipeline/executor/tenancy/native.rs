//! Sealing custody is created beside the actual successful HTTP attempt.

use bitrouter_ai::{
    tenancy::{
        configuration::ResourceRef,
        identity::{AttemptId, ExecutionId},
    },
    types::{Content, GenerateResult, Prompt},
};
use sha2::{Digest, Sha256};

use super::BoundAuthentication;
use crate::gateway::{
    pipeline::native_context::message_commitment, providers::auth::ContinuationAuthority,
};

/// Bind actual retained fixed-header values, including account selectors outside
/// the built-in list. Retained imports continue to reject passthrough rules.
/// Automatic trace headers outside the retained target are not constraints.
pub(in crate::gateway::pipeline::executor) fn bind_headers(
    authority: Option<ContinuationAuthority>,
    request: &reqwest::Request,
    target: &crate::gateway::pipeline::types::RoutingTarget,
    ctx: &crate::gateway::pipeline::context::PipelineContext,
) -> Option<ContinuationAuthority> {
    let Some(execution) = super::execution(ctx) else {
        return authority;
    };
    if target.headers.is_empty()
        || ctx
            .extension::<crate::gateway::pipeline::native::NativeManagedRequest>()
            .is_none()
    {
        return authority;
    }
    authority.map(|authority| {
        bind_header_values(
            execution.installation_id().as_str(),
            authority,
            request.headers(),
            &target.headers,
        )
    })
}

fn bind_header_values(
    installation: &str,
    authority: ContinuationAuthority,
    headers: &http::HeaderMap,
    rules: &[crate::gateway::pipeline::types::OutboundHeaderRule],
) -> ContinuationAuthority {
    let mut digest = Sha256::new();
    digest.update(b"bitrouter.tenant.native.request-headers.v1");
    digest.update(authority.credential().proof_bytes());
    for rule in rules {
        let name = rule.name().as_str().as_bytes();
        digest.update((name.len() as u64).to_be_bytes());
        digest.update(name);
        let values = headers.get_all(rule.name());
        digest.update((values.iter().count() as u64).to_be_bytes());
        for value in values.iter() {
            digest.update((value.as_bytes().len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
    }
    ContinuationAuthority::new(
        crate::gateway::providers::auth::CredentialAuthority::derive_scoped(
            "tenant-native-header-scope-v1",
            installation,
            &hex::encode(digest.finalize()),
        ),
        authority.effective_scheme(),
    )
}

pub(in crate::gateway::pipeline::executor) struct Storage {
    pub stored: bool,
    pub replayable: bool,
    pub effort_bound: bool,
}

/// No public constructor or writable fields. A public continuation source is
/// merely a proposed projection, checked against this executor-owned evidence.
pub(crate) struct Proof {
    execution: ExecutionId,
    attempt: AttemptId,
    candidate: Vec<u8>,
    credential: ResourceRef,
    authority: ContinuationAuthority,
    prompt: [u8; 32],
    message: Vec<u8>,
    response_id: Option<String>,
    storage: Storage,
}

impl BoundAuthentication {
    pub(in crate::gateway::pipeline::executor) fn record_native_source(
        &self,
        attempt: AttemptId,
        authority: &ContinuationAuthority,
        prompt: &Prompt,
        result: &GenerateResult,
        storage: Storage,
    ) {
        // A malformed/oversized source cannot acquire sealing authority, but
        // its already billed result is still returned by the executor.
        let Some(prompt) = prompt_digest(prompt) else {
            return;
        };
        let Ok(message) = message_commitment(&result.content) else {
            return;
        };
        self.execution.record_native_source(Proof {
            execution: self.execution.id().clone(),
            attempt,
            candidate: self.candidate.clone(),
            credential: self.credential.clone(),
            authority: authority.clone(),
            prompt,
            message,
            response_id: result.response_id.clone(),
            storage,
        });
    }
}

impl Proof {
    pub(crate) fn execution(&self) -> &ExecutionId {
        &self.execution
    }
    pub(crate) fn attempt(&self) -> &AttemptId {
        &self.attempt
    }
    pub(crate) fn candidate(&self) -> &[u8] {
        &self.candidate
    }
    pub(crate) fn credential(&self) -> &ResourceRef {
        &self.credential
    }
    pub(crate) fn authority(&self) -> &ContinuationAuthority {
        &self.authority
    }
    pub(crate) fn message_matches(&self, content: &[Content]) -> bool {
        message_commitment(content).is_ok_and(|message| message == self.message)
    }
    pub(crate) fn continuation_matches(
        &self,
        prompt: &Prompt,
        response_id: &str,
        replayable: bool,
    ) -> bool {
        self.storage.stored
            && self.storage.effort_bound
            && (!replayable || self.storage.replayable)
            && self.response_id.as_deref() == Some(response_id)
            && prompt_digest(prompt).is_some_and(|digest| digest == self.prompt)
    }
}

pub(in crate::gateway::pipeline::executor) fn prompt_digest(prompt: &Prompt) -> Option<[u8; 32]> {
    struct Hash {
        digest: Sha256,
        remaining: usize,
    }
    impl std::io::Write for Hash {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.remaining = self
                .remaining
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("native source exceeds limit"))?;
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
    serde_json::to_writer(&mut hash, prompt).ok()?;
    Some(hash.digest.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{
        model::target::AuthScheme, pipeline::types::OutboundHeaderRule,
        providers::auth::CredentialAuthority,
    };
    use anyhow::{Result, ensure};

    #[test]
    fn fixed_header_authority_commits_actual_values_without_claiming_passthrough_support()
    -> Result<()> {
        let rules = [OutboundHeaderRule::new(
            "x-workspace",
            Some("fixed"),
            false,
        )?];
        let authority = ContinuationAuthority::new(
            CredentialAuthority::derive_scoped("test", "installation", "secret"),
            AuthScheme::Bearer,
        );
        let mut sent = http::HeaderMap::new();
        sent.insert("x-workspace", "fixed".parse()?);
        let expected = bind_header_values("installation", authority.clone(), &sent, &rules);
        let mut changed = sent.clone();
        changed.insert("x-workspace", "changed".parse()?);
        ensure!(
            bind_header_values("installation", authority.clone(), &changed, &rules) != expected
        );
        changed.remove("x-workspace");
        ensure!(
            bind_header_values("installation", authority.clone(), &changed, &rules) != expected
        );
        changed = sent.clone();
        changed.append("x-workspace", "fixed".parse()?);
        ensure!(
            bind_header_values("installation", authority.clone(), &changed, &rules) != expected
        );
        changed = sent.clone();
        changed.insert("x-request-id", "other-trace".parse()?);
        ensure!(
            bind_header_values("installation", authority.clone(), &changed, &rules) == expected
        );
        ensure!(bind_header_values("other-installation", authority, &sent, &rules) != expected);
        // This private projection test injects actual-header drift directly.
        // It does not make mutable headers admissible in a retained route.
        Ok(())
    }
}
