//! Selected static Google Chat credentials live only in their admitted invocation.

use std::{collections::HashMap, sync::Mutex};

use super::{Credential, ErrorCode, ResourceRef, RoutingTarget, TenantExecution};
use crate::gateway::model::target::{CredentialPriority, ModelTarget};

type Candidate = (ResourceRef, Vec<u8>);

#[derive(Default)]
pub(super) struct Contexts(Mutex<HashMap<Candidate, String>>);

impl TenantExecution {
    /// Resolve the exact retained slot before credential-bound replay preflight.
    pub(crate) async fn prepare_google_chat(
        &self,
        target: &RoutingTarget,
    ) -> Result<bool, ErrorCode> {
        if !target.chat_google_extensions
            || target.api_protocol != bitrouter_ai::types::ApiProtocol::ChatCompletions
        {
            return Ok(false);
        }
        self.revalidate()?;
        let key = (
            self.route.match_candidate(target)?,
            self.route.candidate_key(target)?,
        );
        if self
            .google_chat
            .0
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?
            .contains_key(&key)
        {
            return Ok(true);
        }
        let owned = self.credential(target).await?;
        let Credential::ApiKey { value } = owned.credential else {
            return Err(ErrorCode::CapabilityMismatch);
        };
        if owned.reference != key.0 || value.is_empty() {
            return Err(ErrorCode::CapabilityMismatch);
        }
        self.google_chat
            .0
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?
            .insert(key, value);
        Ok(true)
    }

    /// Borrow the same selected secret for replay validation and output proofs.
    /// The retained target and shared catalog remain credential-free. Actual-send
    /// authentication still reads and revalidates the original resource slot.
    pub(crate) fn google_chat_target(
        &self,
        target: &RoutingTarget,
    ) -> Result<ModelTarget, ErrorCode> {
        let mut model = target.model_target();
        if !model.compatibility.chat_completions.google_extensions
            || model.api_protocol != bitrouter_ai::types::ApiProtocol::ChatCompletions
        {
            return Ok(model);
        }
        let key = (
            self.route.match_candidate(target)?,
            self.route.candidate_key(target)?,
        );
        model.api_key = self
            .google_chat
            .0
            .lock()
            .map_err(|_| ErrorCode::AuthorityUnavailable)?
            .get(&key)
            .cloned()
            .ok_or(ErrorCode::AuthorityUnavailable)?;
        model.credential_priority = CredentialPriority::Explicit;
        Ok(model)
    }
}
