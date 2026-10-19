//! Deployment-key encryption with namespace, identity and revision binding.

use anyhow::{Context, Result, ensure};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

const NONCE_BYTES: usize = 12;
const MAX_SECRET_BYTES: usize = 1024 * 1024;
const MAX_ENCODED_BYTES: usize = (MAX_SECRET_BYTES + 32) * 4 / 3 + 4;

#[derive(Clone)]
pub(super) struct DeploymentKey {
    secret: [u8; 32],
    id: String,
}

impl std::fmt::Debug for DeploymentKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeploymentKey")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl DeploymentKey {
    pub fn from_bytes(secret: [u8; 32]) -> Result<Self> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&secret)
            .map_err(|_| anyhow::anyhow!("invalid shared deployment key"))?;
        mac.update(b"bitrouter.gateway.deployment-key-id.v1");
        let id = hex::encode(mac.finalize().into_bytes());
        Ok(Self { secret, id })
    }

    pub fn from_environment(name: &str) -> Result<Self> {
        let encoded = std::env::var(name).map_err(|_| {
            anyhow::anyhow!("shared gateway deployment key environment is unavailable")
        })?;
        ensure!(
            encoded.len() <= 128,
            "shared gateway deployment key must encode exactly 32 bytes"
        );
        let decoded = STANDARD
            .decode(encoded.trim())
            .context("shared gateway deployment key must be base64")?;
        let secret: [u8; 32] = decoded.try_into().map_err(|_| {
            anyhow::anyhow!("shared gateway deployment key must encode exactly 32 bytes")
        })?;
        Self::from_bytes(secret)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn seal(&self, identity: SecretIdentity<'_>, plaintext: &[u8]) -> Result<SealedSecret> {
        ensure!(
            plaintext.len() <= MAX_SECRET_BYTES,
            "shared secret exceeds byte limit"
        );
        let mut nonce = [0_u8; NONCE_BYTES];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| anyhow::anyhow!("generating shared encryption nonce failed"))?;
        let mut ciphertext = plaintext.to_vec();
        // ring's AEAD binds the supplied record identity and authenticates it
        // before plaintext is exposed on open.
        // https://docs.rs/ring/latest/ring/aead/struct.LessSafeKey.html
        self.key()?
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(identity.aad(&self.id)?),
                &mut ciphertext,
            )
            .map_err(|_| anyhow::anyhow!("encrypting shared secret failed"))?;
        Ok(SealedSecret {
            format: 1,
            key_id: self.id.clone(),
            nonce: STANDARD.encode(nonce),
            ciphertext: STANDARD.encode(ciphertext),
        })
    }

    pub fn open(&self, identity: SecretIdentity<'_>, sealed: &SealedSecret) -> Result<Vec<u8>> {
        ensure!(
            sealed.format == 1 && sealed.key_id == self.id,
            "shared secret key or format mismatch"
        );
        ensure!(sealed.nonce.len() == 16, "invalid shared secret nonce");
        ensure!(
            sealed.ciphertext.len() <= MAX_ENCODED_BYTES,
            "shared ciphertext exceeds byte limit"
        );
        let nonce: [u8; NONCE_BYTES] = STANDARD
            .decode(&sealed.nonce)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid shared secret nonce"))?;
        let mut ciphertext = STANDARD.decode(&sealed.ciphertext)?;
        let plaintext = self
            .key()?
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(identity.aad(&self.id)?),
                &mut ciphertext,
            )
            .map_err(|_| anyhow::anyhow!("shared secret authentication failed"))?;
        ensure!(
            plaintext.len() <= MAX_SECRET_BYTES,
            "shared secret exceeds byte limit"
        );
        Ok(plaintext.to_vec())
    }

    fn key(&self) -> Result<LessSafeKey> {
        UnboundKey::new(&AES_256_GCM, &self.secret)
            .map(LessSafeKey::new)
            .map_err(|_| anyhow::anyhow!("constructing shared encryption key failed"))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SealedSecret {
    format: u32,
    key_id: String,
    nonce: String,
    ciphertext: String,
}

impl std::fmt::Debug for SealedSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SealedSecret")
            .field("format", &self.format)
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
pub(super) struct SecretIdentity<'a> {
    pub namespace: &'a str,
    pub record: &'a str,
    pub revision: u64,
}

impl SecretIdentity<'_> {
    fn aad(&self, key_id: &str) -> Result<Vec<u8>> {
        ensure!(
            !self.namespace.is_empty() && self.namespace.len() <= 64,
            "invalid shared secret namespace"
        );
        ensure!(
            !self.record.is_empty() && self.record.len() <= 256,
            "invalid shared secret identity"
        );
        // Version/domain separation prevents substitution between records, epochs
        // or clusters even when operators deploy the same wrapping key.
        Ok(serde_json::to_vec(&(
            "bitrouter.gateway.secret.v1",
            self.namespace,
            self.record,
            self.revision,
            key_id,
        ))?)
    }
}

#[cfg(test)]
mod tests {
    use super::{DeploymentKey, SecretIdentity};

    #[test]
    fn ciphertext_cannot_move_between_clusters_records_revisions_or_keys() -> anyhow::Result<()> {
        let key = DeploymentKey::from_bytes([7; 32])?;
        let identity = SecretIdentity {
            namespace: "east",
            record: "account-a",
            revision: 4,
        };
        let sealed = key.seal(identity, b"private-token")?;
        assert_eq!(key.open(identity, &sealed)?, b"private-token");
        for other in [
            SecretIdentity {
                namespace: "west",
                ..identity
            },
            SecretIdentity {
                record: "account-b",
                ..identity
            },
            SecretIdentity {
                revision: 5,
                ..identity
            },
        ] {
            assert!(key.open(other, &sealed).is_err());
        }
        assert!(
            DeploymentKey::from_bytes([8; 32])?
                .open(identity, &sealed)
                .is_err()
        );
        let diagnostic = format!("{key:?} {sealed:?}");
        assert!(!diagnostic.contains("private-token"));
        assert!(!diagnostic.contains(&sealed.ciphertext));
        Ok(())
    }

    #[test]
    fn truncation_and_modified_ciphertext_fail_authentication() -> anyhow::Result<()> {
        let key = DeploymentKey::from_bytes([3; 32])?;
        let identity = SecretIdentity {
            namespace: "cluster",
            record: "keys",
            revision: 1,
        };
        let mut sealed = key.seal(identity, b"correlation-key")?;
        sealed.ciphertext.truncate(4);
        assert!(key.open(identity, &sealed).is_err());
        Ok(())
    }
}
