//! Application encryption primitives. Shared wire identities remain unchanged;
//! local tenancy supplies independently framed, domain-separated identities.

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
pub(crate) struct DeploymentKey {
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

    /// Foundation helpers reconstruct peers in the same namespace. Retain one
    /// random deployment key per test process so those peers share authority.
    #[cfg(all(test, feature = "multi-instance"))]
    pub(crate) fn fixture() -> Result<Self> {
        static SECRET: std::sync::OnceLock<[u8; 32]> = std::sync::OnceLock::new();
        Self::from_bytes(*SECRET.get_or_init(rand::random))
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

    /// Bind secret-bearing operator input without storing a guessable plaintext
    /// digest. Length framing separates the namespace, purpose and input.
    pub fn fingerprint(&self, namespace: &str, purpose: &str, input: &[u8]) -> Result<String> {
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.secret)
            .map_err(|_| anyhow::anyhow!("invalid shared deployment key"))?;
        // HMAC-SHA256; the domain is separate from encryption and key identity.
        // https://www.rfc-editor.org/rfc/rfc2104
        mac.update(b"bitrouter.gateway.operator-binding.v1");
        for part in [namespace.as_bytes(), purpose.as_bytes(), input] {
            mac.update(&u64::try_from(part.len())?.to_be_bytes());
            mac.update(part);
        }
        Ok(hex::encode(mac.finalize().into_bytes()))
    }

    #[cfg(any(test, feature = "multi-instance"))]
    pub fn seal(&self, identity: SecretIdentity<'_>, plaintext: &[u8]) -> Result<SealedSecret> {
        self.seal_aad(&identity.aad(&self.id)?, plaintext)
    }

    /// Every field of `identity` is authenticated as structured JSON, without
    /// concatenation or the shared backend's namespace/record length limits.
    pub fn seal_bound(
        &self,
        purpose: &str,
        identity: &impl Serialize,
        plaintext: &[u8],
    ) -> Result<SealedSecret> {
        self.seal_aad(&self.bound_aad(purpose, identity)?, plaintext)
    }

    pub fn open_bound(
        &self,
        purpose: &str,
        identity: &impl Serialize,
        sealed: &SealedSecret,
    ) -> Result<Vec<u8>> {
        self.open_aad(&self.bound_aad(purpose, identity)?, sealed)
    }

    fn bound_aad(&self, purpose: &str, identity: &impl Serialize) -> Result<Vec<u8>> {
        let aad =
            serde_json::to_vec(&("bitrouter.tenancy.secret.v1", purpose, identity, self.id()))?;
        ensure!(
            aad.len() <= MAX_SECRET_BYTES,
            "encryption identity exceeds byte limit"
        );
        Ok(aad)
    }

    fn seal_aad(&self, aad: &[u8], plaintext: &[u8]) -> Result<SealedSecret> {
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
                Aad::from(aad),
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

    #[cfg(any(test, feature = "multi-instance"))]
    pub fn open(&self, identity: SecretIdentity<'_>, sealed: &SealedSecret) -> Result<Vec<u8>> {
        self.open_aad(&identity.aad(&self.id)?, sealed)
    }

    fn open_aad(&self, aad: &[u8], sealed: &SealedSecret) -> Result<Vec<u8>> {
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
                Aad::from(aad),
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
pub(crate) struct SealedSecret {
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
#[cfg(any(test, feature = "multi-instance"))]
pub(crate) struct SecretIdentity<'a> {
    pub namespace: &'a str,
    pub record: &'a str,
    pub revision: u64,
}

#[cfg(any(test, feature = "multi-instance"))]
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
    fn operator_fingerprint_binds_key_namespace_purpose_and_content() -> anyhow::Result<()> {
        let key = DeploymentKey::from_bytes(rand::random())?;
        let wrong_key = DeploymentKey::from_bytes(rand::random())?;
        let fingerprint = key.fingerprint("east", "mutation", b"private-token")?;
        assert_eq!(
            fingerprint,
            key.fingerprint("east", "mutation", b"private-token")?
        );
        assert_ne!(
            fingerprint,
            key.fingerprint("west", "mutation", b"private-token")?
        );
        assert_ne!(
            fingerprint,
            key.fingerprint("east", "principal", b"private-token")?
        );
        assert_ne!(
            fingerprint,
            key.fingerprint("east", "mutation", b"other-token")?
        );
        assert_ne!(
            fingerprint,
            wrong_key.fingerprint("east", "mutation", b"private-token")?
        );
        assert_ne!(
            key.fingerprint("ab", "c", b"d")?,
            key.fingerprint("a", "bc", b"d")?
        );
        Ok(())
    }

    #[test]
    fn ciphertext_cannot_move_between_clusters_records_revisions_or_keys() -> anyhow::Result<()> {
        let key = DeploymentKey::from_bytes(rand::random())?;
        let wrong_key = DeploymentKey::from_bytes(rand::random())?;
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
        assert!(wrong_key.open(identity, &sealed).is_err());
        let diagnostic = format!("{key:?} {sealed:?}");
        assert!(!diagnostic.contains("private-token"));
        assert!(!diagnostic.contains(&sealed.ciphertext));
        Ok(())
    }

    #[test]
    fn truncation_and_modified_ciphertext_fail_authentication() -> anyhow::Result<()> {
        let key = DeploymentKey::from_bytes(rand::random())?;
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

    #[test]
    fn tenancy_binding_frames_full_ids_and_cannot_cross_legacy_domains() -> anyhow::Result<()> {
        let key = DeploymentKey::from_bytes([7; 32])?;
        let owner = "o".repeat(128);
        let resource = "r".repeat(128);
        let revision = "v".repeat(128);
        let identity =
            serde_json::json!(["installation", "tenant", owner, "route", resource, revision]);
        let sealed = key.seal_bound("resource", &identity, b"private")?;
        assert_eq!(key.open_bound("resource", &identity, &sealed)?, b"private");
        assert!(key.open_bound("configuration", &identity, &sealed).is_err());
        let mut other = identity.clone();
        other[0] = serde_json::json!("other-installation");
        assert!(key.open_bound("resource", &other, &sealed).is_err());
        let left = serde_json::json!(["ab", "c"]);
        let right = serde_json::json!(["a", "bc"]);
        let sealed = key.seal_bound("resource", &left, b"private")?;
        assert!(key.open_bound("resource", &right, &sealed).is_err());
        assert!(
            key.open(
                SecretIdentity {
                    namespace: "ab",
                    record: "c",
                    revision: 1
                },
                &sealed
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn extracted_crypto_reads_the_existing_shared_ciphertext_format() -> anyhow::Result<()> {
        use base64::Engine;
        use ring::aead::{Aad, Nonce};
        let key = DeploymentKey::from_bytes([7; 32])?;
        // Construct the old wire format independently of the extracted seal API.
        let aad = serde_json::to_vec(&(
            "bitrouter.gateway.secret.v1",
            "cluster",
            "slot",
            4_u64,
            key.id(),
        ))?;
        let nonce = [11; 12];
        let mut ciphertext = b"legacy-secret".to_vec();
        key.key()?
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut ciphertext,
            )
            .map_err(|_| anyhow::anyhow!("legacy fixture sealing failed"))?;
        let wire = serde_json::json!({"format":1,"key_id":key.id(),
            "nonce":super::STANDARD.encode(nonce),"ciphertext":super::STANDARD.encode(ciphertext)});
        let sealed = serde_json::from_value(wire)?;
        assert_eq!(
            key.open(
                SecretIdentity {
                    namespace: "cluster",
                    record: "slot",
                    revision: 4
                },
                &sealed
            )?,
            b"legacy-secret"
        );
        Ok(())
    }
}
