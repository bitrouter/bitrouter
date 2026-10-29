use bitrouter_ai::core::evidence::NativeTokenRates;
use bitrouter_ai::core::protocol::{
    ClientMessage, Command, CommitStatus, ErrorCode, ErrorEnvelope, ServerMessage,
};
use serde_json::json;

#[test]
fn managed_pricing_evidence_retains_exact_finite_bits_without_application_features()
-> Result<(), Box<dyn std::error::Error>> {
    // This value exercises the non-roundtripping fast JSON float decoder.
    // https://github.com/serde-rs/json/blob/v1.0.150/tests/test.rs
    let rate: f64 = "51.248178375505404".parse()?;
    let rates = NativeTokenRates {
        uncached_input: Some(rate),
        cache_read: None,
        cache_write: None,
        output: Some(0.0),
    };
    let wire = serde_json::to_string(&rates)?;
    let decoded: NativeTokenRates = serde_json::from_str(&wire)?;
    assert_eq!(
        decoded.uncached_input.map(f64::to_bits),
        Some(rate.to_bits())
    );
    assert_eq!(decoded.cache_read, None);
    assert_eq!(decoded.output, Some(0.0));
    Ok(())
}

#[test]
fn checkpoint_wire_data_preserves_opaque_bytes_without_claiming_validation()
-> Result<(), Box<dyn std::error::Error>> {
    // A data decoder must not decode/reserialize the committed payload. The
    // service/harness codec separately rejects invalid encoding or digests.
    let wire = json!({
        "type": "checkpoint.proposed",
        "payload": {
            "identity": {
                "batch_id": "b1", "session_id": "s1",
                "execution_epoch": 1, "core_instance_id": "core1"
            },
            "payload_encoding": "base64-json",
            "payload_bytes": "not-yet-verified=\n",
            "payload_sha256": "not-yet-verified"
        }
    });
    let decoded: ServerMessage = serde_json::from_value(wire.clone())?;
    assert!(matches!(decoded, ServerMessage::Checkpoint(_)));
    assert_eq!(serde_json::to_value(decoded)?, wire);
    Ok(())
}

#[test]
fn client_mutations_retain_the_existing_tag_and_expected_revision()
-> Result<(), Box<dyn std::error::Error>> {
    let wire = json!({
        "version": 1, "session_id": "s1", "execution_epoch": 3,
        "operation_id": "op1", "expected_state_revision": 7,
        "type": "run.cancel", "payload": {"run_id": "run1"}
    });
    let decoded: ClientMessage = serde_json::from_value(wire.clone())?;
    assert!(matches!(decoded.command, Command::CancelRun { .. }));
    assert_eq!(decoded.expected_state_revision, Some(7));
    assert_eq!(serde_json::to_value(decoded)?, wire);
    let unsupported = json!({
        "version": 1, "session_id": "s1", "execution_epoch": 3,
        "operation_id": "op1", "type": "run.delete", "payload": {}
    });
    assert!(serde_json::from_value::<ClientMessage>(unsupported).is_err());
    Ok(())
}

#[test]
fn http_error_envelopes_preserve_unknown_commit_status() -> Result<(), Box<dyn std::error::Error>> {
    let wire = json!({"error": {
        "code": "checkpoint_unavailable", "message": "durable authority unavailable",
        "commit_status": "unknown"
    }});
    let decoded: ErrorEnvelope = serde_json::from_value(wire.clone())?;
    assert_eq!(decoded.error.code, ErrorCode::CheckpointUnavailable);
    assert_eq!(decoded.error.commit_status, CommitStatus::Unknown);
    assert_eq!(serde_json::to_value(decoded)?, wire);
    Ok(())
}
