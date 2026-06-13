//! The LOAD-BEARING legitimacy pin (spec §1.5 cond. 1).
//!
//! Ported from private-ai-gateway's `AciDcapVerifierPolicy`
//! (`src/aci/verifier/dcap.rs`, Apache-2.0). The policy **refuses to construct
//! without a pin** and decides whether an attested TEE is running the
//! *legitimate* model, not merely *a* genuine TEE — the gap NEAR's own
//! reference verifier leaves open. A model is accepted iff its workload id is
//! allowlisted OR one of its image digests is, under a pinned dstack KMS root.
//!
//! Adaptation from the gateway: the gateway pinned raw **secp256k1** KMS root
//! points and canonicalized them with `compressed_k256_public_key_hex`. NEAR
//! publishes its dstack KMS root as a **P-256 DER SubjectPublicKeyInfo**
//! (`info.key_provider_info.id`), a different curve, so we canonicalize to the
//! SEC1 point instead (accepting both a raw point and a DER SPKI) — same intent,
//! correct for NEAR's key form. Workload id / image digests come from NEAR's
//! model `info` block (Decision 8) rather than the gateway's report shape.

use std::collections::BTreeSet;

use crate::VerifyError;
use crate::near::report::AttestationInfo;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PolicyError {
    #[error("DCAP policy requires at least one accepted workload id or image digest")]
    EmptyPolicy,
    #[error("DCAP policy requires at least one accepted dstack KMS root public key")]
    EmptyKmsRootPolicy,
    #[error("invalid dstack KMS root public key: {0}")]
    InvalidKmsRootPublicKey(String),
}

/// The pinned acceptance policy. Constructed once at boot from operator config;
/// every field is normalized so config and report compare equal regardless of
/// hex casing or EC encoding framing.
#[derive(Debug, Clone)]
pub struct AciDcapVerifierPolicy {
    accepted_workload_ids: BTreeSet<String>,
    accepted_image_digests: BTreeSet<String>,
    accepted_kms_root_public_keys: BTreeSet<String>,
}

impl AciDcapVerifierPolicy {
    /// Build a policy. Errors (matching the gateway) if no workload/image pin is
    /// given ([`PolicyError::EmptyPolicy`]), if no KMS root is given
    /// ([`PolicyError::EmptyKmsRootPolicy`]), or if a KMS root key is unparseable
    /// ([`PolicyError::InvalidKmsRootPublicKey`]). There is **no** unpinned
    /// constructor — that is the whole point.
    pub fn new(
        accepted_workload_ids: impl IntoIterator<Item = String>,
        accepted_image_digests: impl IntoIterator<Item = String>,
        accepted_kms_root_public_keys: impl IntoIterator<Item = String>,
    ) -> Result<Self, PolicyError> {
        let accepted_workload_ids = accepted_workload_ids
            .into_iter()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_lowercase())
            .collect::<BTreeSet<_>>();
        let accepted_image_digests = accepted_image_digests
            .into_iter()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_lowercase())
            .collect::<BTreeSet<_>>();
        let accepted_kms_root_public_keys = accepted_kms_root_public_keys
            .into_iter()
            .filter(|s| !s.is_empty())
            .map(|key| canonical_ec_public_key(&key))
            .collect::<Result<BTreeSet<_>, _>>()?;
        if accepted_workload_ids.is_empty() && accepted_image_digests.is_empty() {
            return Err(PolicyError::EmptyPolicy);
        }
        if accepted_kms_root_public_keys.is_empty() {
            return Err(PolicyError::EmptyKmsRootPolicy);
        }
        Ok(Self {
            accepted_workload_ids,
            accepted_image_digests,
            accepted_kms_root_public_keys,
        })
    }

    /// The legitimacy decision: `workload_id ∈ allowlist` OR any
    /// `image_digest ∈ allowlist`. → [`crate::AttestationChecks::policy_accepts`].
    pub fn accepts(&self, workload_id: &str, image_digests: &[String]) -> bool {
        self.accepted_workload_ids
            .contains(&workload_id.to_lowercase())
            || image_digests
                .iter()
                .any(|d| self.accepted_image_digests.contains(&d.to_lowercase()))
    }

    /// True iff the report's dstack KMS root is one we pinned. A model can only
    /// be trusted if endorsed by an accepted KMS root.
    pub fn accepts_kms_root(&self, kms_root_public_key: &str) -> bool {
        match canonical_ec_public_key(kms_root_public_key) {
            Ok(k) => self.accepted_kms_root_public_keys.contains(&k),
            Err(_) => false,
        }
    }
}

/// The identity fields a [`ModelAttestation`](crate::ModelAttestation) presents
/// to the policy, extracted from its `info` block (Decision 8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelIdentity {
    pub workload_id: String,
    pub image_digests: Vec<String>,
    pub kms_root_public_key: String,
}

#[derive(serde::Deserialize)]
struct KeyProviderInfo {
    id: String,
}

/// Extract the policy-relevant identity from a model's `info` block:
/// `workload_id ← app_id`, `image_digests ← {os_image_hash, compose_hash}`,
/// `kms_root ← key_provider_info.id` (spec §1.5 Decision 8). The compose
/// container `@sha256` digests are additional image pins that can be folded in
/// later without changing this shape.
pub fn model_identity(info: &AttestationInfo) -> Result<ModelIdentity, VerifyError> {
    let kpi: KeyProviderInfo =
        serde_json::from_str(&info.key_provider_info).map_err(|e| VerifyError::Malformed {
            what: "key_provider_info",
            detail: e.to_string(),
        })?;
    Ok(ModelIdentity {
        workload_id: info.app_id.clone(),
        image_digests: vec![info.os_image_hash.clone(), info.compose_hash.clone()],
        kms_root_public_key: kpi.id,
    })
}

/// Canonicalize an EC public key to its hex-encoded SEC1 point, accepting
/// either a raw SEC1 point or a DER SubjectPublicKeyInfo (NEAR's KMS root form).
/// Both config and report normalize to the same point so they compare equal.
fn canonical_ec_public_key(public_key_hex: &str) -> Result<String, PolicyError> {
    let bytes = hex::decode(public_key_hex.trim())
        .map_err(|e| PolicyError::InvalidKmsRootPublicKey(format!("not hex: {e}")))?;
    let point = sec1_point(&bytes).ok_or_else(|| {
        PolicyError::InvalidKmsRootPublicKey(
            "expected a SEC1 EC point or a DER SubjectPublicKeyInfo".to_string(),
        )
    })?;
    Ok(hex::encode(point))
}

/// Pull the uncompressed/compressed SEC1 point out of either a raw SEC1
/// encoding or the tail of a DER SubjectPublicKeyInfo. The EC point is the
/// meaningful key material; this ignores DER framing so two encodings of the
/// same key match.
fn sec1_point(bytes: &[u8]) -> Option<Vec<u8>> {
    match bytes.first() {
        Some(0x04) if bytes.len() == 65 => return Some(bytes.to_vec()),
        Some(0x02 | 0x03) if bytes.len() == 33 => return Some(bytes.to_vec()),
        _ => {}
    }
    // DER SPKI: the subjectPublicKey BIT STRING is the tail; an uncompressed
    // point is its final 65 bytes (leading 0x04), a compressed one its final 33.
    if bytes.first() != Some(&0x30) {
        return None;
    }
    if bytes.len() >= 65 && bytes[bytes.len() - 65] == 0x04 {
        return Some(bytes[bytes.len() - 65..].to_vec());
    }
    if bytes.len() >= 33 && matches!(bytes[bytes.len() - 33], 0x02 | 0x03) {
        return Some(bytes[bytes.len() - 33..].to_vec());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::near::report::AttestationReport;

    const FIXTURE: &str = include_str!("../../tests/fixtures/near_report.json");
    const APP_ID: &str = "2c0a0c96cb6dbd659bf1446e2f3fce58172ff91b";
    const COMPOSE_HASH: &str = "c445f29994165e94e85bdfc4824f4bcba89b0a883f45e7912f1bfd7c2634a698";
    const OS_IMAGE_HASH: &str = "9b69bb1698bacbb6985409a2c272bcb892e09cdcea63d5399c6768b67d3ff677";
    const KMS_ROOT_DER_SPKI: &str = "3059301306072a8648ce3d020106082a8648ce3d03010703420004228f800590a10442cba9d0e6adb2fa9f195eea9e75e23dd35990d52b59dda2415a63674c38adebde4ffd4d4b265bf818985933820c8053cee3ce29b5fb0fbcbc";

    fn fixture_info() -> AttestationInfo {
        let r: AttestationReport = serde_json::from_str(FIXTURE).unwrap();
        r.model_attestations[0].info.clone()
    }

    #[test]
    fn constructor_refuses_without_a_workload_or_image_pin() {
        let err = AciDcapVerifierPolicy::new([], [], [KMS_ROOT_DER_SPKI.to_string()]).unwrap_err();
        assert_eq!(err, PolicyError::EmptyPolicy);
    }

    #[test]
    fn constructor_refuses_without_a_kms_root_pin() {
        let err = AciDcapVerifierPolicy::new([APP_ID.to_string()], [], []).unwrap_err();
        assert_eq!(err, PolicyError::EmptyKmsRootPolicy);
    }

    #[test]
    fn constructor_rejects_an_unparseable_kms_root() {
        let err = AciDcapVerifierPolicy::new([APP_ID.to_string()], [], ["nothex!!".to_string()])
            .unwrap_err();
        assert!(matches!(err, PolicyError::InvalidKmsRootPublicKey(_)));
    }

    #[test]
    fn model_identity_maps_the_info_block() {
        let id = model_identity(&fixture_info()).expect("identity");
        assert_eq!(id.workload_id, APP_ID);
        assert!(id.image_digests.contains(&OS_IMAGE_HASH.to_string()));
        assert!(id.image_digests.contains(&COMPOSE_HASH.to_string()));
        assert_eq!(id.kms_root_public_key, KMS_ROOT_DER_SPKI);
    }

    #[test]
    fn policy_accepts_the_legitimate_model_by_workload_id() {
        let policy =
            AciDcapVerifierPolicy::new([APP_ID.to_string()], [], [KMS_ROOT_DER_SPKI.to_string()])
                .unwrap();
        let id = model_identity(&fixture_info()).unwrap();
        assert!(policy.accepts(&id.workload_id, &id.image_digests));
        assert!(policy.accepts_kms_root(&id.kms_root_public_key));
    }

    #[test]
    fn policy_accepts_by_image_digest_alone() {
        let policy = AciDcapVerifierPolicy::new(
            [],
            [COMPOSE_HASH.to_string()],
            [KMS_ROOT_DER_SPKI.to_string()],
        )
        .unwrap();
        let id = model_identity(&fixture_info()).unwrap();
        assert!(policy.accepts(&id.workload_id, &id.image_digests));
    }

    #[test]
    fn policy_rejects_a_genuine_tee_running_a_different_model() {
        // THE load-bearing case: a real TEE, but not the model we pinned.
        let policy = AciDcapVerifierPolicy::new(
            ["some-other-workload".to_string()],
            ["deadbeef".to_string()],
            [KMS_ROOT_DER_SPKI.to_string()],
        )
        .unwrap();
        let id = model_identity(&fixture_info()).unwrap();
        assert!(!policy.accepts(&id.workload_id, &id.image_digests));
    }

    #[test]
    fn kms_root_matches_whether_pinned_as_der_spki_or_raw_point() {
        // The raw SEC1 point is the trailing 65 bytes of the DER SPKI.
        let raw_point = &KMS_ROOT_DER_SPKI[KMS_ROOT_DER_SPKI.len() - 130..];
        let policy =
            AciDcapVerifierPolicy::new([APP_ID.to_string()], [], [raw_point.to_string()]).unwrap();
        // Report presents the full DER SPKI; it still matches the pinned point.
        assert!(policy.accepts_kms_root(KMS_ROOT_DER_SPKI));
    }

    #[test]
    fn policy_rejects_an_unpinned_kms_root() {
        let policy =
            AciDcapVerifierPolicy::new([APP_ID.to_string()], [], [KMS_ROOT_DER_SPKI.to_string()])
                .unwrap();
        // A different P-256 SPKI (last point byte flipped) must not match.
        let mut other = KMS_ROOT_DER_SPKI.to_string();
        other.replace_range(other.len() - 2.., "ff");
        assert!(!policy.accepts_kms_root(&other));
    }
}
