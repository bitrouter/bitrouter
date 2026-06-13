//! Intel TDX quote handling via `dcap-qvl` (spec §5.1 step 3).
//!
//! Two entry points, matching the spec's offline/online split:
//! - [`parse_tdx_quote`] — **offline**, deterministic. Decodes the measurements
//!   (mr_td, rtmr0..3, report_data, td_attributes) straight from the quote
//!   bytes. Used to cross-check the report's self-reported `tcb_info`, to feed
//!   the report_data binding (Task 3) and the event-log/RTMR replay (Task 5b),
//!   and runs in CI without network or hardware.
//! - [`verify_tdx_quote`] — **online**. Fetches DCAP collateral from a PCCS and
//!   verifies Intel's signature + TCB at a given time, returning the
//!   measurements from the *verified* report. Network-dependent, so its test is
//!   `#[ignore]`d in CI.
//!
//! `parse_tdx_quote` alone proves nothing about authenticity — it only decodes.
//! Genuineness comes from `verify_tdx_quote` (Intel signature) plus the policy
//! pin (Task 5b). Keeping them separate lets every other check be unit-tested
//! offline against the golden fixture.

use dcap_qvl::quote::{Quote, Report};

use crate::VerifyError;

/// The measurement registers and report_data decoded from a TDX quote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TdxMeasurements {
    /// The 64-byte `report_data` — `signing_address ‖ nonce` for NEAR
    /// (see [`crate::report_data_binds`]).
    pub report_data: [u8; 64],
    /// `MRTD` — the initial TD measurement.
    pub mr_td: [u8; 48],
    pub rtmr0: [u8; 48],
    pub rtmr1: [u8; 48],
    pub rtmr2: [u8; 48],
    /// `RTMR3` — where dstack folds the app/compose event log
    /// (replayed in Task 5b).
    pub rtmr3: [u8; 48],
    /// TD attribute flags.
    pub td_attributes: [u8; 8],
}

impl TdxMeasurements {
    /// True iff the TD debug bit (bit 0 of `td_attributes`) is **off** — a
    /// production, non-debuggable TD (gateway `nearai.py` debug check; spec
    /// §1.5). A debug TD can be inspected/modified by its host, so a set bit
    /// must fail the attestation.
    pub fn debug_disabled(&self) -> bool {
        self.td_attributes[0] & 0x01 == 0
    }
}

fn measurements_from_report(report: &Report) -> Result<TdxMeasurements, VerifyError> {
    let td = match report {
        Report::TD10(r) => r,
        Report::TD15(r) => &r.base,
        Report::SgxEnclave(_) => {
            return Err(VerifyError::Malformed {
                what: "tdx quote",
                detail: "expected a TDX (TD10/TD15) quote, got an SGX enclave report".to_string(),
            });
        }
    };
    Ok(TdxMeasurements {
        report_data: td.report_data,
        mr_td: td.mr_td,
        rtmr0: td.rt_mr0,
        rtmr1: td.rt_mr1,
        rtmr2: td.rt_mr2,
        rtmr3: td.rt_mr3,
        td_attributes: td.td_attributes,
    })
}

/// Decode measurements from a raw TDX quote **without** network collateral.
/// Offline and deterministic; does *not* prove Intel signed the quote.
pub fn parse_tdx_quote(raw: &[u8]) -> Result<TdxMeasurements, VerifyError> {
    let quote = Quote::parse(raw).map_err(|e| VerifyError::Malformed {
        what: "tdx quote",
        detail: e.to_string(),
    })?;
    measurements_from_report(&quote.report)
}

/// Default Phala PCCS endpoint for DCAP collateral, re-exported so callers can
/// pin it in the daemon rather than fetch a URL through the untrusted cloud.
pub const PHALA_PCCS_URL: &str = dcap_qvl::PHALA_PCCS_URL;

/// Full DCAP verification: fetch collateral from `pccs_url`, verify Intel's
/// signature and TCB at `now_unix`, and return the measurements from the
/// **verified** report. Network-dependent.
pub async fn verify_tdx_quote(
    raw: &[u8],
    pccs_url: &str,
    now_unix: u64,
) -> Result<TdxMeasurements, VerifyError> {
    let collateral = dcap_qvl::collateral::get_collateral(pccs_url, raw)
        .await
        .map_err(|e| VerifyError::Transport {
            what: "dcap collateral",
            source: e.to_string().into(),
        })?;
    let verified =
        dcap_qvl::verify::rustcrypto::verify(raw, &collateral, now_unix).map_err(|e| {
            VerifyError::Malformed {
                what: "tdx quote verification",
                detail: e.to_string(),
            }
        })?;
    measurements_from_report(&verified.report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::near::report::AttestationReport;

    const FIXTURE: &str = include_str!("../../tests/fixtures/near_report.json");

    fn fixture_quote() -> Vec<u8> {
        let r: AttestationReport = serde_json::from_str(FIXTURE).unwrap();
        hex::decode(&r.model_attestations[0].intel_quote).unwrap()
    }

    #[test]
    fn parse_matches_the_reports_self_declared_tcb_info() {
        // The quote's decoded measurements must equal what the report's
        // tcb_info claims — a consistency cross-check that needs no network.
        let raw: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        let ti = &raw["model_attestations"][0]["info"]["tcb_info"];

        let m = parse_tdx_quote(&fixture_quote()).expect("fixture quote parses");

        assert_eq!(hex::encode(m.mr_td), ti["mrtd"].as_str().unwrap());
        assert_eq!(hex::encode(m.rtmr0), ti["rtmr0"].as_str().unwrap());
        assert_eq!(hex::encode(m.rtmr1), ti["rtmr1"].as_str().unwrap());
        assert_eq!(hex::encode(m.rtmr2), ti["rtmr2"].as_str().unwrap());
        assert_eq!(hex::encode(m.rtmr3), ti["rtmr3"].as_str().unwrap());
    }

    #[test]
    fn parse_exposes_report_data_and_debug_off() {
        let m = parse_tdx_quote(&fixture_quote()).unwrap();
        // report_data starts with the attested signing address (Task 3).
        assert!(hex::encode(m.report_data).starts_with("bb4d2e7ffe98eefcd9690e2139be41e92b95e333"));
        // The production TD has its debug bit cleared.
        assert!(m.debug_disabled());
    }

    #[test]
    fn parse_rejects_garbage_bytes() {
        let err = parse_tdx_quote(b"not a quote").unwrap_err();
        assert!(matches!(err, VerifyError::Malformed { .. }));
    }

    #[tokio::test]
    #[ignore = "fetches DCAP collateral from a live PCCS; run manually with network"]
    async fn full_dcap_verification_against_live_pccs() {
        let now = 1_749_800_000; // a fixed recent epoch; the quote/TCB must be valid at this time
        let m = verify_tdx_quote(&fixture_quote(), PHALA_PCCS_URL, now)
            .await
            .expect("live DCAP verification succeeds");
        assert!(m.debug_disabled());
    }
}
