//! dstack event-log replay — the anchor that makes the policy load-bearing
//! (spec §5.1 step 3; ports `verify_dstack_event_log_and_app_id` +
//! `replay_dstack_rtmr` from private-ai-gateway `src/aci/verifier/dstack.rs`,
//! Apache-2.0).
//!
//! The `info` block (`app_id`, `compose_hash`, `os_image_hash`,
//! `key_provider_info`) arrives as **cloud JSON** and on its own proves
//! nothing. This module replays the event log into RTMR3 and requires it to
//! equal the value the genuine TDX quote measured; only then are the event
//! payloads trustworthy. It then checks those payloads equal the `info` fields
//! the policy consumes — binding cloud metadata to the TEE measurement. Without
//! this, a genuine TEE running a *different* model could forge matching `info`
//! and pass the policy (spec §1.5 cond. 1).

use sha2::{Digest, Sha384};

use crate::near::report::{AttestationInfo, DstackEvent};

/// Replay the dstack event log into the register `imr`:
/// `mr ← sha384(mr ‖ digest)` for each matching event, starting from zero.
/// Returns `None` if a digest is unparseable or the result isn't 48 bytes.
pub fn replay_rtmr(events: &[DstackEvent], imr: u32) -> Option<[u8; 48]> {
    let mut mr = vec![0u8; 48];
    for event in events.iter().filter(|e| e.imr == imr) {
        let mut digest = hex::decode(&event.digest).ok()?;
        if digest.len() < 48 {
            digest.resize(48, 0);
        }
        mr.extend_from_slice(&digest);
        mr = Sha384::digest(&mr).to_vec();
    }
    mr.as_slice().try_into().ok()
}

/// The payload of the first `imr == 3` event named `name`, considering only
/// events up to `system-ready` (post-boot events can't define identity).
fn event_payload<'a>(events: &'a [DstackEvent], name: &str) -> Option<&'a str> {
    events
        .iter()
        .take_while(|e| !(e.imr == 3 && e.event == "system-ready"))
        .find(|e| e.imr == 3 && e.event == name)
        .map(|e| e.event_payload.as_str())
}

/// True iff the event log replays to the quote's RTMR3 **and** records exactly
/// the `info` fields the policy trusts. This is what anchors `app_id`,
/// `compose_hash`, `os_image_hash`, and `key_provider_info` to the genuine TEE
/// measurement. Any mismatch ⇒ `false` (fail-closed).
pub fn event_log_binds_info(
    events: &[DstackEvent],
    quote_rtmr3: &[u8; 48],
    info: &AttestationInfo,
) -> bool {
    match replay_rtmr(events, 3) {
        Some(replayed) if &replayed == quote_rtmr3 => {}
        _ => return false,
    }

    let app_id_ok =
        event_payload(events, "app-id").is_some_and(|p| p.eq_ignore_ascii_case(&info.app_id));
    let compose_ok = event_payload(events, "compose-hash")
        .is_some_and(|p| p.eq_ignore_ascii_case(&info.compose_hash));
    let os_ok = event_payload(events, "os-image-hash")
        .is_some_and(|p| p.eq_ignore_ascii_case(&info.os_image_hash));
    // The key-provider payload is the hex of the JSON `key_provider_info` blob.
    let kms_ok = event_payload(events, "key-provider")
        .and_then(|p| hex::decode(p).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .is_some_and(|s| s == info.key_provider_info);

    app_id_ok && compose_ok && os_ok && kms_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::near::report::AttestationReport;
    use crate::near::tdx::parse_tdx_quote;

    const FIXTURE: &str = include_str!("../../tests/fixtures/near_report.json");

    fn fixture() -> AttestationReport {
        serde_json::from_str(FIXTURE).unwrap()
    }

    #[test]
    fn event_log_replays_to_the_quote_rtmr3_and_binds_info() {
        let r = fixture();
        let m = &r.model_attestations[0];
        let measurements = parse_tdx_quote(&hex::decode(&m.intel_quote).unwrap()).unwrap();

        // Replay matches the genuine quote's RTMR3...
        assert_eq!(replay_rtmr(&m.event_log, 3), Some(measurements.rtmr3));
        // ...and the recorded payloads equal the info the policy consumes.
        assert!(event_log_binds_info(
            &m.event_log,
            &measurements.rtmr3,
            &m.info
        ));
    }

    #[test]
    fn rejects_when_rtmr3_does_not_match_the_quote() {
        let r = fixture();
        let m = &r.model_attestations[0];
        // A wrong (zero) RTMR3 must not be accepted even with a valid log.
        assert!(!event_log_binds_info(&m.event_log, &[0u8; 48], &m.info));
    }

    #[test]
    fn rejects_when_info_is_forged_against_a_genuine_log() {
        // Forge app_id in info while keeping the genuine event log: the binding
        // fails, so a forged-metadata report can't pass.
        let r = fixture();
        let m = &r.model_attestations[0];
        let measurements = parse_tdx_quote(&hex::decode(&m.intel_quote).unwrap()).unwrap();
        let mut info = m.info.clone();
        info.app_id = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string();
        assert!(!event_log_binds_info(
            &m.event_log,
            &measurements.rtmr3,
            &info
        ));
    }

    #[test]
    fn rejects_a_tampered_event_digest() {
        let r = fixture();
        let mut m = r.model_attestations[0].clone();
        let measurements = parse_tdx_quote(&hex::decode(&m.intel_quote).unwrap()).unwrap();
        // Flip a digest so the replay diverges from the quote.
        if let Some(ev) = m
            .event_log
            .iter_mut()
            .find(|e| e.imr == 3 && e.event == "app-id")
        {
            ev.digest = "00".repeat(48);
        }
        assert!(!event_log_binds_info(
            &m.event_log,
            &measurements.rtmr3,
            &m.info
        ));
    }
}
