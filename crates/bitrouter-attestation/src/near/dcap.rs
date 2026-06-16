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
    /// Intel security advisory IDs (e.g. `INTEL-SA-00615`) the operator
    /// explicitly accepts despite a non-current TCB. Empty (the default) means
    /// the floor requires `UpToDate`. Normalized to upper-case for comparison.
    allowed_tcb_advisory_ids: BTreeSet<String>,
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
            // Default floor: require an `UpToDate` TCB. Operators opt into
            // accepting specific advisories via `with_allowed_tcb_advisory_ids`.
            allowed_tcb_advisory_ids: BTreeSet::new(),
        })
    }

    /// Allow non-current TCB levels whose advisories are **all** in this set
    /// (e.g. `INTEL-SA-00615`). Empty (the default) keeps the floor at
    /// `UpToDate`. IDs are normalized to upper-case. Builder, so the load-
    /// bearing [`Self::new`] pins stay mandatory and this stays opt-in.
    #[must_use]
    pub fn with_allowed_tcb_advisory_ids(mut self, ids: impl IntoIterator<Item = String>) -> Self {
        self.allowed_tcb_advisory_ids = ids
            .into_iter()
            .map(|s| s.trim().to_uppercase())
            .filter(|s| !s.is_empty())
            .collect();
        self
    }

    /// The TCB floor decision → [`crate::AttestationChecks::tcb_level_acceptable`].
    /// `UpToDate` always passes. Any other (non-`Revoked`; `dcap-qvl` already
    /// rejects `Revoked`) status passes **only** if it carries at least one
    /// advisory ID and **every** advisory is allow-listed — so an empty
    /// allow-list accepts `UpToDate` only, and a non-current status with no
    /// nameable advisory is never silently accepted. `None` (no verified
    /// status) fails closed.
    pub fn tcb_acceptable(&self, status: Option<&str>, advisory_ids: &[String]) -> bool {
        match status {
            Some("UpToDate") => true,
            Some(_) => {
                !advisory_ids.is_empty()
                    && advisory_ids.iter().all(|id| {
                        self.allowed_tcb_advisory_ids
                            .contains(&id.trim().to_uppercase())
                    })
            }
            None => false,
        }
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

/// Return the SEC1 point for either a raw SEC1 encoding or a DER
/// SubjectPublicKeyInfo. For DER the structure is **parsed and validated**
/// (SEQUENCE → AlgorithmIdentifier with the `ecPublicKey` OID → BIT STRING) so
/// the point is read from the actual `subjectPublicKey` field, not sliced by
/// position — a crafted blob whose tail happens to equal a pinned key is
/// rejected. `None` if it is neither a valid SEC1 point nor a valid EC SPKI.
fn sec1_point(bytes: &[u8]) -> Option<Vec<u8>> {
    if is_sec1_point(bytes) {
        return Some(bytes.to_vec());
    }
    let point = spki_ec_point(bytes)?;
    is_sec1_point(&point).then_some(point)
}

/// True iff `b` is a well-formed SEC1 point: uncompressed `0x04‖X‖Y` (65 bytes)
/// or compressed `0x02|0x03‖X` (33 bytes).
fn is_sec1_point(b: &[u8]) -> bool {
    (b.len() == 65 && b[0] == 0x04) || (b.len() == 33 && matches!(b[0], 0x02 | 0x03))
}

/// ASN.1/DER `1.2.840.10045.2.1` — `ecPublicKey`.
const OID_EC_PUBLIC_KEY: &[u8] = &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01];

/// Parse a DER `SubjectPublicKeyInfo` for an EC key and return its SEC1 point
/// (the `subjectPublicKey` BIT STRING content, minus the unused-bits octet).
/// Validates the OID and structure rather than slicing by offset.
fn spki_ec_point(der: &[u8]) -> Option<Vec<u8>> {
    let (tag, spki, _) = der_tlv(der)?;
    if tag != 0x30 {
        return None; // SubjectPublicKeyInfo ::= SEQUENCE
    }
    let (alg_tag, alg, after_alg) = der_tlv(spki)?;
    if alg_tag != 0x30 {
        return None; // AlgorithmIdentifier ::= SEQUENCE
    }
    let (oid_tag, oid, _) = der_tlv(alg)?;
    if oid_tag != 0x06 || oid != OID_EC_PUBLIC_KEY {
        return None; // algorithm must be ecPublicKey
    }
    let (bit_tag, bit_string, _) = der_tlv(after_alg)?;
    if bit_tag != 0x03 {
        return None; // subjectPublicKey ::= BIT STRING
    }
    let (&unused_bits, point) = bit_string.split_first()?;
    if unused_bits != 0 {
        return None;
    }
    Some(point.to_vec())
}

/// Read one DER TLV: returns `(tag, content, remaining)`. Supports short and
/// long definite-length forms; `None` on any malformed length.
fn der_tlv(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&len0, rest) = rest.split_first()?;
    let (len, rest) = if len0 < 0x80 {
        (len0 as usize, rest)
    } else {
        let n = (len0 & 0x7f) as usize;
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let mut len = 0usize;
        for &b in &rest[..n] {
            len = (len << 8) | b as usize;
        }
        (len, &rest[n..])
    };
    if rest.len() < len {
        return None;
    }
    Some((tag, &rest[..len], &rest[len..]))
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
    fn rejects_a_crafted_der_blob_whose_tail_spoofs_a_pinned_point() {
        // A SEQUENCE wrapping an OCTET STRING (0x04) of the legitimate 65-byte
        // point — its final 65 bytes equal the pinned key, but it is not a valid
        // EC SubjectPublicKeyInfo. Byte-slicing would accept it; structure
        // validation must reject it.
        let raw_point = &KMS_ROOT_DER_SPKI[KMS_ROOT_DER_SPKI.len() - 130..];
        let crafted = format!("30430441{raw_point}");
        let policy =
            AciDcapVerifierPolicy::new([APP_ID.to_string()], [], [KMS_ROOT_DER_SPKI.to_string()])
                .unwrap();
        assert!(!policy.accepts_kms_root(&crafted));
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

    fn tcb_policy(allowed: &[&str]) -> AciDcapVerifierPolicy {
        AciDcapVerifierPolicy::new([APP_ID.to_string()], [], [KMS_ROOT_DER_SPKI.to_string()])
            .unwrap()
            .with_allowed_tcb_advisory_ids(allowed.iter().map(|s| s.to_string()))
    }

    #[test]
    fn tcb_floor_accepts_up_to_date_only_by_default() {
        let p = tcb_policy(&[]);
        assert!(p.tcb_acceptable(Some("UpToDate"), &[]));
        assert!(!p.tcb_acceptable(Some("OutOfDate"), &["INTEL-SA-00615".to_string()]));
        assert!(!p.tcb_acceptable(Some("ConfigurationNeeded"), &[]));
        assert!(!p.tcb_acceptable(Some("SWHardeningNeeded"), &[]));
        // A missing status (no collateral verdict) fails closed.
        assert!(!p.tcb_acceptable(None, &[]));
    }

    #[test]
    fn tcb_floor_allows_a_fully_allowlisted_non_current_status() {
        let p = tcb_policy(&["INTEL-SA-00615"]);
        assert!(p.tcb_acceptable(Some("OutOfDate"), &["INTEL-SA-00615".to_string()]));
        // Advisory IDs compare case-insensitively.
        assert!(p.tcb_acceptable(Some("OutOfDate"), &["intel-sa-00615".to_string()]));
    }

    #[test]
    fn tcb_floor_rejects_when_any_advisory_is_unlisted() {
        let p = tcb_policy(&["INTEL-SA-00615"]);
        assert!(!p.tcb_acceptable(
            Some("OutOfDate"),
            &["INTEL-SA-00615".to_string(), "INTEL-SA-00999".to_string()]
        ));
    }

    #[test]
    fn tcb_floor_never_accepts_a_non_current_status_with_no_named_advisory() {
        // Even with a non-empty allowlist, a non-current status that names no
        // advisory cannot be matched — fail closed, never vacuously true.
        let p = tcb_policy(&["INTEL-SA-00615"]);
        assert!(!p.tcb_acceptable(Some("ConfigurationNeeded"), &[]));
    }

    #[test]
    fn tcb_floor_trims_advisory_ids_on_both_sides() {
        // A whitespace-only allowlist entry must NOT become an empty-string
        // entry that could match a malformed empty advisory id.
        let p = tcb_policy(&["   "]);
        assert!(!p.tcb_acceptable(Some("OutOfDate"), &["".to_string()]));
        // A whitespace-padded advisory from the quote side still matches.
        let p2 = tcb_policy(&["INTEL-SA-00615"]);
        assert!(p2.tcb_acceptable(Some("OutOfDate"), &[" INTEL-SA-00615 ".to_string()]));
    }

    #[test]
    fn tcb_floor_treats_revoked_as_any_non_current_status() {
        // The hard guarantee against `Revoked` is upstream: `dcap-qvl`'s
        // `verify` errors on it before we ever see a status, so it reaches
        // `tcb_acceptable` only in theory. If it ever did, it is NOT special-
        // cased here — it lands in the generic non-`UpToDate` arm: denied with
        // no advisories, and (like any other status) gated by the allow-list
        // otherwise. This test documents that contract rather than asserting a
        // special-case the code does not make.
        let p = tcb_policy(&["INTEL-SA-00615"]);
        assert!(!p.tcb_acceptable(Some("Revoked"), &[]));
    }
}
