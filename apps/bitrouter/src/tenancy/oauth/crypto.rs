//! Narrow slot crypto; the wrapping key never escapes Resources into callbacks.
use super::{profile, storage::Metadata};
use crate::{
    gateway::providers::auth::credentials::OAuthToken,
    runtime_crypto::{DeploymentKey, SealedSecret},
};
use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::{configuration::ResourceRef, identity::InstallationId};

pub(in crate::tenancy) struct Crypto {
    installation: InstallationId,
    key: DeploymentKey,
}
impl Crypto {
    pub(in crate::tenancy) fn new(installation: InstallationId, key: DeploymentKey) -> Self {
        Self { installation, key }
    }
    pub fn seed(
        &self,
        reference: &ResourceRef,
        profile: &profile::Profile,
        token: &OAuthToken,
    ) -> Result<String> {
        // The reference is part of the fingerprint; it is never searched globally.
        self.key.fingerprint(
            "tenancy-oauth-v1",
            "same-slot-import-lineage",
            &serde_json::to_vec(&(&self.installation, reference, profile, token))?,
        )
    }
    pub fn seal(&self, metadata: &Metadata, token: &OAuthToken) -> Result<String> {
        ensure!(
            metadata.installation == self.installation,
            "OAuth installation mismatch"
        );
        metadata.validate()?;
        profile::validate_token(token)?;
        let payload =
            self.key
                .seal_bound("owned-oauth-live-v1", metadata, &serde_json::to_vec(token)?)?;
        let payload = serde_json::to_string(&payload)?;
        ensure!(
            payload.len() <= profile::PAYLOAD_BYTES,
            "OAuth ciphertext capacity"
        );
        Ok(payload)
    }
    pub fn open(&self, metadata: &Metadata, payload: &str) -> Result<OAuthToken> {
        ensure!(
            metadata.installation == self.installation && payload.len() <= profile::PAYLOAD_BYTES,
            "OAuth ciphertext scope or capacity"
        );
        metadata.validate()?;
        let sealed: SealedSecret = serde_json::from_str(payload)?;
        let bytes = self
            .key
            .open_bound("owned-oauth-live-v1", metadata, &sealed)?;
        ensure!(
            bytes.len() <= profile::TOKEN_JSON_BYTES,
            "OAuth plaintext capacity"
        );
        let token = serde_json::from_slice(&bytes)?;
        profile::validate_token(&token)?;
        Ok(token)
    }
}
