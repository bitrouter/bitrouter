//! Selected controlled counting belongs to the original execution, never its callbacks.
use super::{TenantExecution, authorize_route};
use crate::gateway::pipeline::types::RoutingTarget;
use crate::tenancy::receipts::auxiliary::count::{Identity, Permit};
use bitrouter_ai::{tenancy::operation::ErrorCode, types::Prompt};
use sha2::{Digest, Sha256};

impl TenantExecution {
    pub(super) fn prepare_count_profile(&self, prompt: &Prompt) -> Result<(), ErrorCode> {
        let Some(target) = self.route.count_candidate()? else {
            return Ok(());
        };
        self.revalidate()?;
        if self.uses_oauth(target)? || forbidden_prompt(prompt) {
            return Err(ErrorCode::CapabilityMismatch);
        }
        self.receipt
            .get()
            .ok_or(ErrorCode::AuthorityUnavailable)?
            .auxiliary
            .begin_counts(self.count_identity(target)?)
    }
    fn count_identity(&self, target: &RoutingTarget) -> Result<Identity, ErrorCode> {
        Ok(Identity {
            credential: self.route.match_candidate(target)?,
            candidate: hex::encode(Sha256::digest(self.route.candidate_key(target)?)),
        })
    }
    pub(crate) fn count_required(&self) -> Result<bool, ErrorCode> {
        Ok(self.route.count_candidate()?.is_some())
    }
    pub(crate) fn reserve_count(
        self: &std::sync::Arc<Self>,
        target: &RoutingTarget,
        prompt: &Prompt,
    ) -> Result<Permit, ErrorCode> {
        self.revalidate()?;
        let expected = self.count_identity(target)?;
        let journal = &self
            .receipt
            .get()
            .ok_or(ErrorCode::AuthorityUnavailable)?
            .auxiliary;
        if journal.count_identity()?.as_ref() != Some(&expected)
            || self.uses_oauth(target)?
            || forbidden_prompt(prompt)
        {
            return Err(ErrorCode::CapabilityMismatch);
        }
        journal.claim_count(prompt_digest(prompt)?)
    }
    pub(crate) fn allow_count_rebuild(&self, prompt: &Prompt) -> Result<(), ErrorCode> {
        self.revalidate()?;
        if forbidden_prompt(prompt) {
            return Err(ErrorCode::CapabilityMismatch);
        }
        self.receipt
            .get()
            .ok_or(ErrorCode::AuthorityUnavailable)?
            .auxiliary
            .allow_count_rebuild(prompt_digest(prompt)?)
    }
    pub(crate) fn arm_count(
        &self,
        target: &RoutingTarget,
        proof: &crate::gateway::pipeline::executor::input_count::owned::Binding,
        permit: &mut Permit,
        request: &reqwest::Request,
    ) -> Result<(), ErrorCode> {
        self.service
            .with_dispatch_authority(&self.submission, |state| {
                authorize_route(state, &self.submission, self.route.dependencies())?;
                if self
                    .rejection
                    .lock()
                    .map_err(|_| ErrorCode::AuthorityUnavailable)?
                    .is_some()
                {
                    return Err(ErrorCode::Forbidden);
                }
                let journal = &self
                    .receipt
                    .get()
                    .ok_or(ErrorCode::AuthorityUnavailable)?
                    .auxiliary;
                if !permit.belongs_to(journal) || self.count_identity(target)? != *permit.identity()
                {
                    return Err(ErrorCode::Forbidden);
                }
                let owner = self.receipt.get().ok_or(ErrorCode::AuthorityUnavailable)?;
                if owner.cancelled.is_cancelled() || tokio::time::Instant::now() >= owner.deadline {
                    return Err(ErrorCode::CapacityExceeded);
                }
                proof.arm(self, permit, request)
            })
    }
    pub(super) fn verify_count_send(
        &self,
        target: &RoutingTarget,
        digest: Option<&str>,
    ) -> Result<(), ErrorCode> {
        let journal = &self
            .receipt
            .get()
            .ok_or(ErrorCode::AuthorityUnavailable)?
            .auxiliary;
        if self.count_required()? || journal.count_identity()?.is_some() {
            journal.verify_count(
                &self.count_identity(target)?,
                digest.ok_or(ErrorCode::Forbidden)?,
            )?;
        }
        Ok(())
    }
}
fn forbidden_prompt(prompt: &Prompt) -> bool {
    ["previous_response_id", "conversation"].iter().any(|key| {
        prompt
            .params
            .extra
            .get(*key)
            .is_some_and(|value| !value.is_null())
            || prompt
                .params
                .supplemental_extra
                .get(*key)
                .is_some_and(|value| !value.is_null())
    })
}
fn prompt_digest(prompt: &Prompt) -> Result<[u8; 32], ErrorCode> {
    struct HashWriter {
        hash: Sha256,
        left: usize,
    }
    impl std::io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.left = self
                .left
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("count prompt bound"))?;
            self.hash.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter {
        hash: Sha256::new(),
        left: 16 * 1024 * 1024,
    };
    serde_json::to_writer(&mut writer, prompt).map_err(|_| ErrorCode::PayloadTooLarge)?;
    Ok(writer.hash.finalize().into())
}
