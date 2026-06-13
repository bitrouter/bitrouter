//! The two offline binding checks an attestation must pass (spec §5.1 step 4),
//! ported from the reference verifier (`private-ai-gateway`
//! `scripts/confidential_verifier/verifiers/dstack.py::verify_report_data` and
//! NEAR's `model-attestation.js`).
//!
//! 1. **report_data binds the signing key + nonce.** The 64-byte `report_data`
//!    field inside the TDX quote is `signing_address (20B) ‖ 12 zero bytes`
//!    followed by the 32-byte client `nonce`. This is what makes the attested
//!    key the *same* key the client pinned, for *this* request.
//! 2. **compose hash matches the reported config.** `sha256(app_compose)` must
//!    equal the report's `compose_hash` (NEAR's `mr_config`), proving the
//!    running config is the one whose hash the quote measured.

use sha2::{Digest, Sha256};

/// True iff `report_data` (the 64 bytes from the verified TDX quote) binds
/// `signing_address` in its first 32 bytes (20-byte address, zero-padded) and
/// the 32-byte `nonce` in its last 32 bytes — the "standard" dstack mode.
///
/// Fails closed on any malformed input (wrong length, bad hex) rather than
/// erroring, so the caller folds it straight into a verdict.
pub fn report_data_binds(report_data: &[u8], signing_address: &str, nonce: &str) -> bool {
    if report_data.len() != 64 {
        return false;
    }

    let Some(addr) = decode_hex(
        signing_address
            .strip_prefix("0x")
            .unwrap_or(signing_address),
    ) else {
        return false;
    };
    if addr.len() > 32 {
        return false;
    }
    let mut expected_addr = [0u8; 32];
    expected_addr[..addr.len()].copy_from_slice(&addr);
    if report_data[..32] != expected_addr {
        return false;
    }

    match decode_hex(nonce) {
        Some(nonce_bytes) => report_data[32..] == nonce_bytes[..],
        None => false,
    }
}

/// True iff `sha256(app_compose)` (hex) equals `mr_config` (case-insensitive),
/// proving the running config is the measured one.
pub fn compose_matches_mr_config(app_compose: &str, mr_config: &str) -> bool {
    let digest = Sha256::digest(app_compose.as_bytes());
    hex::encode(digest).eq_ignore_ascii_case(mr_config)
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    hex::decode(s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::near::report::AttestationReport;

    const FIXTURE: &str = include_str!("../../tests/fixtures/near_report.json");

    /// The verified TDX quote carries `report_data` at bytes `[568..632]`
    /// (48-byte header + report-body offset 520..584). Slice it from the real
    /// fixture quote so the binding test runs against genuine attested bytes.
    fn fixture_report_data(quote_hex: &str) -> Vec<u8> {
        let q = hex::decode(quote_hex).expect("quote is hex");
        q[48 + 520..48 + 584].to_vec()
    }

    fn fixture() -> AttestationReport {
        serde_json::from_str(FIXTURE).expect("fixture parses")
    }

    #[test]
    fn report_data_binds_the_attested_key_and_nonce() {
        let r = fixture();
        let m = &r.model_attestations[0];
        let rd = fixture_report_data(&m.intel_quote);
        assert!(report_data_binds(&rd, &m.signing_address, &m.request_nonce));
    }

    #[test]
    fn report_data_rejects_a_swapped_nonce() {
        let r = fixture();
        let m = &r.model_attestations[0];
        let rd = fixture_report_data(&m.intel_quote);
        let other_nonce = "00".repeat(32);
        assert!(!report_data_binds(&rd, &m.signing_address, &other_nonce));
    }

    #[test]
    fn report_data_rejects_a_swapped_address() {
        let r = fixture();
        let m = &r.model_attestations[0];
        let rd = fixture_report_data(&m.intel_quote);
        let attacker = "0xdead000000000000000000000000000000000000";
        assert!(!report_data_binds(&rd, attacker, &m.request_nonce));
    }

    #[test]
    fn report_data_rejects_wrong_length_or_bad_hex() {
        assert!(!report_data_binds(&[0u8; 10], "0xbb", "00"));
        assert!(!report_data_binds(&[0u8; 64], "nothex", &"00".repeat(32)));
    }

    #[test]
    fn compose_hash_matches_the_reported_config() {
        let r = fixture();
        let info = &r.model_attestations[0].info;
        // app_compose lives in tcb_info; pulled directly from the raw fixture
        // since AttestationInfo only models compose_hash at this stage.
        let raw: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        let app_compose = raw["model_attestations"][0]["info"]["tcb_info"]["app_compose"]
            .as_str()
            .unwrap();
        assert!(compose_matches_mr_config(app_compose, &info.compose_hash));
        assert!(!compose_matches_mr_config(
            "tampered compose",
            &info.compose_hash
        ));
    }
}
